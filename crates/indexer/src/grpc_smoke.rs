//! `smoke-grpc`: prove the Yellowstone endpoint, credentials and filter shapes actually work,
//! without waiting for the (very idle) programs to be used.
//!
//! ## Why this drives the raw client instead of the carbon datasource
//!
//! Two properties the exit check needs, that `YellowstoneGrpcGeyserClient` cannot give:
//!
//! 1. **A guaranteed message on an idle program.** The datasource hardcodes
//!    `slots: HashMap::new()` and `blocks_meta: HashMap::new()` in the `SubscribeRequest` it
//!    builds (carbon-yellowstone-grpc-datasource 0.12.0, `src/lib.rs`), so the only
//!    heartbeat-ish subscription it can express is a full `blocks` subscription -- which on
//!    devnet means every transaction in every block, and which yields no carbon `Update` at
//!    all if `include_transactions` is false. A `slots` subscription is one tiny message every
//!    ~400 ms and is what a smoke check wants.
//! 2. **An auth failure that reaches the caller.** `consume()` connects the channel inline but
//!    performs the actual `subscribe` inside a spawned task, where a plan/auth rejection is
//!    only `log::error!`-ed. The task returns `Ok(())` regardless, so a smoke check built on
//!    it would exit 0 on a rejected API key.
//!
//! So this builds the same `SubscribeRequest` the datasource would build from
//! `pipeline::account_filters` / `pipeline::transaction_filters` -- same endpoint, same
//! `X-Token`, same commitment, same TLS/timeout config path via the datasource crate's own
//! `YellowstoneGrpcClientConfig::geyser_config_builder` -- and adds a `slots` entry. Everything
//! about the connection that the live pipeline depends on is exercised; only the delivery of
//! updates into carbon's channel is not.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestFilterSlots,
};

use crate::config::Config;
use crate::pipeline::{account_filters, transaction_filters};

/// The first update a subscription delivered.
#[derive(Debug, Clone)]
pub struct SmokeResult {
    /// Human-readable description, printed by `smoke-grpc`.
    pub description: String,
    /// Slot the update carried, when it carried one. This is the "slot at which the stream
    /// connected" that spec §7 requires `run` to record before taking the snapshot: the slot
    /// heartbeat gives a real value within ~400 ms even though the program itself is idle for
    /// days at a time.
    pub slot: Option<u64>,
}

impl std::fmt::Display for SmokeResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.description)
    }
}

/// Runs the check. Returns the first update received, or an error containing the server's
/// verbatim rejection.
///
/// `run` calls this as a **startup subscribe gate** before building the live pipeline: carbon's
/// Yellowstone datasource performs its subscribe inside a spawned task and only `log::error!`s
/// a plan/auth rejection, so without this gate a bad API key would leave the process
/// hot-looping inside the datasource while looking healthy from the outside.
pub async fn run(cfg: &Config, timeout: Duration) -> Result<SmokeResult> {
    let api_key = cfg.require_api_key()?;

    log::info!(
        "smoke-grpc: connecting to {} (commitment=confirmed, programs={})",
        cfg.grpc_url,
        cfg.programs
            .iter()
            .map(|p| p.name)
            .collect::<Vec<_>>()
            .join(",")
    );

    let builder = GeyserGrpcClient::build_from_shared(cfg.grpc_url.clone())
        .with_context(|| format!("invalid gRPC endpoint: {}", cfg.grpc_url))?
        .x_token(Some(api_key.to_string()))
        .context("ALCHEMY_API_KEY is not a valid HTTP header value")?;

    // Identical TLS / connect-timeout / compression handling to the live datasource.
    let mut client = carbon_yellowstone_grpc_datasource::YellowstoneGrpcClientConfig::default()
        .geyser_config_builder(builder)
        .map_err(|e| anyhow!("failed to configure the gRPC client: {e}"))?
        .connect()
        .await
        .map_err(|e| anyhow!("failed to connect to {}: {e}", cfg.grpc_url))?;

    let request = SubscribeRequest {
        // The heartbeat. `filter_by_commitment: Some(true)` keeps it to one message per slot
        // at our commitment level rather than one per commitment transition.
        slots: HashMap::from([(
            "slot_heartbeat".to_string(),
            SubscribeRequestFilterSlots {
                filter_by_commitment: Some(true),
                interslot_updates: Some(false),
            },
        )]),
        // The real filters, so a server that rejects them fails the smoke check.
        accounts: account_filters(&cfg.programs),
        transactions: transaction_filters(&cfg.programs),
        transactions_status: HashMap::new(),
        entry: HashMap::new(),
        blocks: HashMap::new(),
        blocks_meta: HashMap::new(),
        commitment: Some(CommitmentLevel::Confirmed as i32),
        accounts_data_slice: vec![],
        ping: None,
        from_slot: None,
    };

    let (_tx, mut stream) = client
        .subscribe_with_request(Some(request))
        .await
        .map_err(|e| anyhow!("subscribe was rejected by {}: {e}", cfg.grpc_url))?;

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let next = tokio::time::timeout_at(deadline, stream.next()).await;
        match next {
            Err(_) => {
                return Err(anyhow!(
                    "no update received from {} within {timeout:?}",
                    cfg.grpc_url
                ))
            }
            Ok(None) => return Err(anyhow!("the gRPC stream closed before any update arrived")),
            // A rejected key/plan surfaces here, as a tonic `Status`, not at connect time.
            Ok(Some(Err(status))) => {
                return Err(anyhow!(
                    "gRPC stream error (code {:?}): {}",
                    status.code(),
                    status.message()
                ))
            }
            Ok(Some(Ok(update))) => match update.update_oneof {
                // Pings carry no information about whether our filters were accepted, so keep
                // waiting for a real update.
                Some(UpdateOneof::Ping(_)) | Some(UpdateOneof::Pong(_)) | None => continue,
                Some(UpdateOneof::Slot(slot)) => {
                    return Ok(SmokeResult {
                        description: format!(
                            "Slot {{ slot: {}, status: {} }}",
                            slot.slot, slot.status
                        ),
                        slot: Some(slot.slot),
                    })
                }
                Some(UpdateOneof::Account(a)) => {
                    return Ok(SmokeResult {
                        description: format!("Account at slot {}", a.slot),
                        slot: Some(a.slot),
                    })
                }
                Some(UpdateOneof::Transaction(t)) => {
                    return Ok(SmokeResult {
                        description: format!("Transaction at slot {}", t.slot),
                        slot: Some(t.slot),
                    })
                }
                Some(other) => {
                    return Ok(SmokeResult {
                        description: format!("{other:?}"),
                        slot: None,
                    })
                }
            },
        }
    }
}
