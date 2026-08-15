//! Process configuration, read entirely from the environment.
//!
//! Every knob has a documented default except `DATABASE_URL` (always required) and
//! `ALCHEMY_API_KEY` (required only for the subcommands that talk to Alchemy). Nothing here
//! ever logs the key -- `Debug` is hand-written to redact it, so an accidental
//! `log::debug!("{cfg:?}")` can't leak it into a log file.

use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use solana_pubkey::Pubkey;

/// The whitelist program on devnet. Same value as the decoder crate's `PROGRAM_ID`; kept as a
/// string default so `PROGRAM_ID` can be overridden from the environment without a rebuild.
pub const DEFAULT_PROGRAM_ID: &str = "2vVARM46pPD4rcHdbXHnYA4vTGN14q6skQAzsQWcHUxn";

/// Alchemy's Solana devnet Yellowstone gRPC host. The API key travels as the `X-Token`
/// header (the datasource's `x_token` argument), NOT in the URL path -- unlike JSON-RPC.
pub const DEFAULT_GRPC_URL: &str = "https://solana-devnet.g.alchemy.com";

/// Public devnet RPC, used when `ALCHEMY_RPC_URL` errors (throttling, plan limits).
pub const DEFAULT_RPC_FALLBACK_URL: &str = "https://api.devnet.solana.com";

/// Slot the whitelist program was deployed at; the floor for backfill and the initial
/// `sync_state.last_contiguous_slot`. There is nothing to index below it.
pub const DEFAULT_BACKFILL_START_SLOT: u64 = 483_386_556;

/// Prometheus scrape endpoint. 0.0.0.0 (not 127.0.0.1) so the metrics port is reachable from
/// another container in the compose stack.
pub const DEFAULT_METRICS_ADDR: &str = "0.0.0.0:9464";

pub struct Config {
    /// `DATABASE_URL`. Required by every subcommand that writes rows; `smoke-grpc` never
    /// touches Postgres, so it is validated at use (see [`Config::require_database_url`])
    /// rather than at load.
    database_url: Option<String>,
    /// `ALCHEMY_API_KEY`. `None` is only tolerated by subcommands that never reach Alchemy.
    alchemy_api_key: Option<String>,
    /// `ALCHEMY_GRPC_URL`, default [`DEFAULT_GRPC_URL`].
    pub grpc_url: String,
    /// `ALCHEMY_RPC_URL`, default `https://solana-devnet.g.alchemy.com/v2/$ALCHEMY_API_KEY`.
    rpc_url: Option<String>,
    /// `RPC_FALLBACK_URL`, default [`DEFAULT_RPC_FALLBACK_URL`].
    pub rpc_fallback_url: String,
    /// `PROGRAM_ID`, default [`DEFAULT_PROGRAM_ID`].
    pub program_id: Pubkey,
    /// `BACKFILL_START_SLOT`, default [`DEFAULT_BACKFILL_START_SLOT`].
    pub backfill_start_slot: u64,
    /// `METRICS_ADDR`, default [`DEFAULT_METRICS_ADDR`].
    pub metrics_addr: SocketAddr,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let database_url = std::env::var("DATABASE_URL").ok().filter(|u| !u.is_empty());

        let alchemy_api_key = std::env::var("ALCHEMY_API_KEY")
            .ok()
            .filter(|k| !k.is_empty());

        let program_id_str =
            std::env::var("PROGRAM_ID").unwrap_or_else(|_| DEFAULT_PROGRAM_ID.to_string());
        let program_id = Pubkey::from_str(&program_id_str).with_context(|| {
            format!("PROGRAM_ID is not a valid base58 pubkey: {program_id_str}")
        })?;

        let backfill_start_slot = match std::env::var("BACKFILL_START_SLOT") {
            Ok(s) => s
                .parse::<u64>()
                .with_context(|| format!("BACKFILL_START_SLOT is not a u64: {s}"))?,
            Err(_) => DEFAULT_BACKFILL_START_SLOT,
        };

        let metrics_addr_str =
            std::env::var("METRICS_ADDR").unwrap_or_else(|_| DEFAULT_METRICS_ADDR.to_string());
        let metrics_addr = metrics_addr_str.parse::<SocketAddr>().with_context(|| {
            format!("METRICS_ADDR is not a host:port address: {metrics_addr_str}")
        })?;

        Ok(Self {
            database_url,
            alchemy_api_key,
            grpc_url: std::env::var("ALCHEMY_GRPC_URL")
                .unwrap_or_else(|_| DEFAULT_GRPC_URL.to_string()),
            rpc_url: std::env::var("ALCHEMY_RPC_URL")
                .ok()
                .filter(|u| !u.is_empty()),
            rpc_fallback_url: std::env::var("RPC_FALLBACK_URL")
                .unwrap_or_else(|_| DEFAULT_RPC_FALLBACK_URL.to_string()),
            program_id,
            backfill_start_slot,
            metrics_addr,
        })
    }

    pub fn require_database_url(&self) -> Result<&str> {
        self.database_url
            .as_deref()
            .ok_or_else(|| anyhow!("DATABASE_URL is required (postgres://user:pass@host:port/db)"))
    }

    /// The Alchemy API key, erroring (rather than silently degrading to the public endpoint)
    /// when it is missing -- the datasource choice is a deliberate user decision.
    pub fn require_api_key(&self) -> Result<&str> {
        self.alchemy_api_key
            .as_deref()
            .ok_or_else(|| anyhow!("ALCHEMY_API_KEY is required for this subcommand"))
    }

    /// Primary JSON-RPC URL: `ALCHEMY_RPC_URL` if set, else Alchemy's devnet v2 endpoint with
    /// the key in the path. Falls back to the public devnet endpoint when no key is set at
    /// all, so the DB-only subcommands stay usable without credentials.
    pub fn rpc_url(&self) -> String {
        if let Some(url) = &self.rpc_url {
            return url.clone();
        }
        match &self.alchemy_api_key {
            Some(key) => format!("https://solana-devnet.g.alchemy.com/v2/{key}"),
            None => self.rpc_fallback_url.clone(),
        }
    }
}

/// Hand-written so the API key (and the key embedded in `ALCHEMY_RPC_URL`) can never reach a
/// log line via `{:?}`.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field(
                "database_url",
                &self.database_url.as_deref().map(redact_url_password),
            )
            .field(
                "alchemy_api_key",
                &self.alchemy_api_key.as_ref().map(|_| "<set>"),
            )
            .field("grpc_url", &self.grpc_url)
            .field("rpc_url", &self.rpc_url.as_ref().map(|_| "<set>"))
            .field("rpc_fallback_url", &self.rpc_fallback_url)
            .field("program_id", &self.program_id)
            .field("backfill_start_slot", &self.backfill_start_slot)
            .field("metrics_addr", &self.metrics_addr)
            .finish()
    }
}

/// `postgres://user:secret@host/db` -> `postgres://user:***@host/db`.
pub fn redact_url_password(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let rest = &url[scheme_end + 3..];
    let Some(at) = rest.find('@') else {
        return url.to_string();
    };
    let creds = &rest[..at];
    let Some(colon) = creds.find(':') else {
        return url.to_string();
    };
    format!(
        "{}://{}:***@{}",
        &url[..scheme_end],
        &creds[..colon],
        &rest[at + 1..]
    )
}

#[cfg(test)]
mod tests {
    use super::redact_url_password;

    #[test]
    fn redacts_password_only() {
        assert_eq!(
            redact_url_password("postgres://postgres:test@localhost:54329/postgres"),
            "postgres://postgres:***@localhost:54329/postgres"
        );
        // No credentials -> unchanged.
        assert_eq!(
            redact_url_password("postgres://localhost/postgres"),
            "postgres://localhost/postgres"
        );
        // Username but no password -> unchanged (nothing to hide).
        assert_eq!(
            redact_url_password("postgres://postgres@localhost/postgres"),
            "postgres://postgres@localhost/postgres"
        );
    }
}
