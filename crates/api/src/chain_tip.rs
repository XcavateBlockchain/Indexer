//! A `getSlot(confirmed)` reading, cached for <=5s and shared between `GET /health` and the
//! `syncStatus.chainTipSlot` resolver so neither pays for its own RPC round trip on every
//! request (the brief requires the `/health` reading specifically be "cached <=5s").

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;

const CACHE_TTL: Duration = Duration::from_secs(5);

struct Cached {
    slot: u64,
    at: Instant,
}

pub struct ChainTipCache {
    rpc_urls: Vec<String>,
    cached: Mutex<Option<Cached>>,
}

impl ChainTipCache {
    pub fn new(rpc_urls: Vec<String>) -> Self {
        Self {
            rpc_urls,
            cached: Mutex::new(None),
        }
    }

    /// Returns the last known chain tip slot, refreshing via RPC if the cached value is older
    /// than [`CACHE_TTL`] (or there isn't one yet). Tries each configured endpoint in turn.
    pub async fn get(&self) -> Result<u64> {
        if let Some(slot) = self.fresh_cached_slot() {
            return Ok(slot);
        }

        let mut last_err = anyhow!("no RPC endpoint configured");
        for url in &self.rpc_urls {
            let rpc = RpcClient::new_with_commitment(url.clone(), CommitmentConfig::confirmed());
            match rpc
                .get_slot_with_commitment(CommitmentConfig::confirmed())
                .await
            {
                Ok(slot) => {
                    *self.cached.lock().expect("chain tip cache mutex poisoned") = Some(Cached {
                        slot,
                        at: Instant::now(),
                    });
                    return Ok(slot);
                }
                Err(e) => last_err = anyhow!("getSlot failed: {e}"),
            }
        }
        Err(last_err)
    }

    fn fresh_cached_slot(&self) -> Option<u64> {
        let guard = self.cached.lock().expect("chain tip cache mutex poisoned");
        let cached = guard.as_ref()?;
        if cached.at.elapsed() < CACHE_TTL {
            Some(cached.slot)
        } else {
            None
        }
    }
}
