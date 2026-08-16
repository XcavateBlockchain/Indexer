//! Process configuration, read entirely from the environment.
//!
//! Deliberately does NOT depend on `crates/indexer`'s `config` module even though the shape
//! overlaps a lot (RPC endpoint selection, URL redaction): pulling in the `indexer` crate as a
//! library dependency would drag its whole non-GraphQL dependency graph (Yellowstone gRPC,
//! `carbon-yellowstone-grpc-datasource`, `clap`, ...) into this binary's build -- including the
//! Windows `protoc` workaround `task-3-report.md` documents, which this crate otherwise has no
//! reason to need. The overlap is ~20 lines (`rpc_endpoints`, `redact_url_password`); duplicating
//! it here keeps this binary's build graph fully independent of the indexer's.

use std::fmt;
use std::net::SocketAddr;

use anyhow::{anyhow, Context, Result};
use axum::http::HeaderValue;

/// Public devnet RPC, used when no Alchemy key is configured and as the retry target for
/// `getSlot` (matches `crates/indexer/src/config.rs`'s `DEFAULT_RPC_FALLBACK_URL`).
pub const DEFAULT_RPC_FALLBACK_URL: &str = "https://api.devnet.solana.com";

/// `GRAPHQL_PORT` default (spec: bind `0.0.0.0:3010`).
pub const DEFAULT_GRAPHQL_PORT: u16 = 3010;

/// `METRICS_ADDR` default. NOTE: deliberately different from the indexer binary's `9464`
/// default (brief requirement) so the two binaries' `/metrics` listeners never collide when
/// run side by side on the same host.
pub const DEFAULT_METRICS_ADDR: &str = "0.0.0.0:9465";

pub struct Config {
    database_url: String,
    /// `ALCHEMY_API_KEY`, optional -- `/health` and `syncStatus.chainTipSlot` degrade to the
    /// public devnet endpoint (rate-limited but functional) when unset, rather than refusing
    /// to start; unlike the indexer, this binary has no write path that a bad key should gate.
    alchemy_api_key: Option<String>,
    rpc_url_override: Option<String>,
    pub rpc_fallback_url: String,
    pub graphql_addr: SocketAddr,
    pub metrics_addr: SocketAddr,
    /// `CORS_ALLOWED_ORIGINS`: `None` = allow every origin (the default), `Some(list)` = only
    /// these origins may call the API from a browser. See [`parse_cors_origins`].
    pub cors_allowed_origins: Option<Vec<HeaderValue>>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let database_url = std::env::var("DATABASE_URL")
            .ok()
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                anyhow!("DATABASE_URL is required (postgres://user:pass@host:port/db)")
            })?;

        let alchemy_api_key = std::env::var("ALCHEMY_API_KEY")
            .ok()
            .filter(|k| !k.is_empty());

        let rpc_url_override = std::env::var("ALCHEMY_RPC_URL")
            .ok()
            .filter(|u| !u.is_empty());

        let rpc_fallback_url = std::env::var("RPC_FALLBACK_URL")
            .unwrap_or_else(|_| DEFAULT_RPC_FALLBACK_URL.to_string());

        let port = match std::env::var("GRAPHQL_PORT") {
            Ok(s) => s
                .parse::<u16>()
                .with_context(|| format!("GRAPHQL_PORT is not a valid port: {s}"))?,
            Err(_) => DEFAULT_GRAPHQL_PORT,
        };
        let graphql_addr = SocketAddr::from(([0, 0, 0, 0], port));

        let metrics_addr_str =
            std::env::var("METRICS_ADDR").unwrap_or_else(|_| DEFAULT_METRICS_ADDR.to_string());
        let metrics_addr = metrics_addr_str.parse::<SocketAddr>().with_context(|| {
            format!("METRICS_ADDR is not a host:port address: {metrics_addr_str}")
        })?;

        let cors_allowed_origins =
            parse_cors_origins(std::env::var("CORS_ALLOWED_ORIGINS").ok())?;

        Ok(Self {
            database_url,
            alchemy_api_key,
            rpc_url_override,
            rpc_fallback_url,
            graphql_addr,
            metrics_addr,
            cors_allowed_origins,
        })
    }

    pub fn database_url(&self) -> &str {
        &self.database_url
    }

    /// JSON-RPC endpoints in preference order, for `getSlot`: the primary (Alchemy, if a key is
    /// configured), then the public devnet fallback (deduplicated when they are the same).
    pub fn rpc_endpoints(&self) -> Vec<String> {
        let primary = self.rpc_url();
        if primary == self.rpc_fallback_url {
            vec![primary]
        } else {
            vec![primary, self.rpc_fallback_url.clone()]
        }
    }

    fn rpc_url(&self) -> String {
        if let Some(url) = &self.rpc_url_override {
            return url.clone();
        }
        match &self.alchemy_api_key {
            Some(key) => format!("https://solana-devnet.g.alchemy.com/v2/{key}"),
            None => self.rpc_fallback_url.clone(),
        }
    }
}

/// Parses `CORS_ALLOWED_ORIGINS`: a comma-separated list of origins allowed to call the API
/// from a browser (`https://app.example.com,http://localhost:3000`). Unset, empty, or any
/// entry of `*` all mean "allow every origin" (`None`). Entries are whitespace-trimmed and a
/// trailing `/` is dropped -- browsers send the `Origin` header without one, so a
/// `https://app.example.com/` entry in an exact-match list would otherwise silently never
/// match.
fn parse_cors_origins(raw: Option<String>) -> Result<Option<Vec<HeaderValue>>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let entries: Vec<&str> = raw
        .split(',')
        .map(|entry| entry.trim().trim_end_matches('/'))
        .filter(|entry| !entry.is_empty())
        .collect();
    if entries.is_empty() || entries.contains(&"*") {
        return Ok(None);
    }
    entries
        .iter()
        .map(|origin| {
            HeaderValue::from_str(origin).with_context(|| {
                format!("CORS_ALLOWED_ORIGINS entry is not a valid origin: {origin}")
            })
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

/// Hand-written so `DATABASE_URL`'s password and the Alchemy key (bare or embedded in
/// `ALCHEMY_RPC_URL`) never reach a log line via `{:?}`.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("database_url", &redact_url_password(&self.database_url))
            .field(
                "alchemy_api_key",
                &self.alchemy_api_key.as_ref().map(|_| "<set>"),
            )
            .field(
                "rpc_url_override",
                &self.rpc_url_override.as_ref().map(|_| "<set>"),
            )
            .field("rpc_fallback_url", &self.rpc_fallback_url)
            .field("graphql_addr", &self.graphql_addr)
            .field("metrics_addr", &self.metrics_addr)
            .field("cors_allowed_origins", &self.cors_allowed_origins)
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
    use super::{parse_cors_origins, redact_url_password};

    #[test]
    fn cors_unset_empty_or_star_allows_all() {
        assert_eq!(parse_cors_origins(None).unwrap(), None);
        assert_eq!(parse_cors_origins(Some(String::new())).unwrap(), None);
        assert_eq!(parse_cors_origins(Some(" , ".into())).unwrap(), None);
        assert_eq!(parse_cors_origins(Some("*".into())).unwrap(), None);
        assert_eq!(
            parse_cors_origins(Some("https://app.example.com, *".into())).unwrap(),
            None
        );
    }

    #[test]
    fn cors_list_is_trimmed_and_trailing_slash_dropped() {
        let origins =
            parse_cors_origins(Some(" https://app.example.com/ ,http://localhost:3000".into()))
                .unwrap()
                .unwrap();
        let origins: Vec<&str> = origins.iter().map(|v| v.to_str().unwrap()).collect();
        assert_eq!(origins, ["https://app.example.com", "http://localhost:3000"]);
    }

    #[test]
    fn cors_invalid_origin_is_an_error() {
        assert!(parse_cors_origins(Some("https://app.example.com\u{7f}".into())).is_err());
    }

    #[test]
    fn redacts_password_only() {
        assert_eq!(
            redact_url_password("postgres://postgres:test@localhost:54329/postgres"),
            "postgres://postgres:***@localhost:54329/postgres"
        );
        assert_eq!(
            redact_url_password("postgres://localhost/postgres"),
            "postgres://localhost/postgres"
        );
    }
}
