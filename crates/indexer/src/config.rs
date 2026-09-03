//! Process configuration, read entirely from the environment.
//!
//! Every knob has a documented default except `DATABASE_URL` (always required) and
//! `ALCHEMY_API_KEY` (required only for the subcommands that talk to Alchemy). Nothing here
//! ever logs the key -- `Debug` is hand-written to redact it, so an accidental
//! `log::debug!("{cfg:?}")` can't leak it into a log file.

use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::programs::{self, ProgramSpec};

/// Alchemy's Solana devnet Yellowstone gRPC host. The API key travels as the `X-Token`
/// header (the datasource's `x_token` argument), NOT in the URL path -- unlike JSON-RPC.
pub const DEFAULT_GRPC_URL: &str = "https://solana-devnet.g.alchemy.com";

/// Public devnet RPC, used when `ALCHEMY_RPC_URL` errors (throttling, plan limits).
pub const DEFAULT_RPC_FALLBACK_URL: &str = "https://api.devnet.solana.com";

/// Prometheus scrape endpoint. 0.0.0.0 (not 127.0.0.1) so the metrics port is reachable from
/// another container in the compose stack.
pub const DEFAULT_METRICS_ADDR: &str = "0.0.0.0:9464";

/// How often the reconciliation supervisor re-walks the tip (see [`crate::reconcile`]).
/// 5 minutes: ~576 RPC requests/day, a rounding error against the free tier's budget, while
/// bounding how long an undetected stream outage can leave `last_contiguous_slot` stale.
pub const DEFAULT_RECONCILE_INTERVAL_SECS: u64 = 300;

/// How often the property-metadata fetcher polls its work set (see [`crate::metadata`],
/// ADR-27). 30 seconds: a work-set query is one indexed Postgres read, and a new
/// `init_property_assets` transaction should show up in the API within a minute; the
/// fetching itself is bounded (`metadata::CYCLE_LIMIT`) and backed off per URI, so the
/// interval costs nothing while the work set is empty.
pub const DEFAULT_METADATA_FETCH_INTERVAL_SECS: u64 = 30;

/// How often the webhook delivery loop drains its work set (see [`crate::webhooks`], ADR-28).
/// 5 seconds: a property-asset registration should reach the endpoint within a few seconds;
/// the work-set query is one indexed Postgres read and the loop is bounded and per-event
/// backed off, so the interval costs nothing while nothing is pending.
pub const DEFAULT_WEBHOOK_INTERVAL_SECS: u64 = 5;

/// How often the property image mirror polls its work set (see [`crate::images`], ADR-31).
/// 30 seconds: like the metadata fetcher the work-set query is one indexed Postgres read; a
/// thumbnail showing up within a minute of the metadata fetch is plenty, and the downloads are
/// bounded (`images::CYCLE_LIMIT`) and backed off per image, so the interval costs nothing
/// while the work set is empty.
pub const DEFAULT_IMAGE_MIRROR_INTERVAL_SECS: u64 = 30;

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
    /// `PROGRAMS` (comma-separated registry names), default: every program in
    /// [`crate::programs::PROGRAMS`]. Each program's address and backfill floor (its deploy
    /// slot) are compiled in -- see the registry for why they are not env-overridable.
    pub programs: Vec<&'static ProgramSpec>,
    /// `METRICS_ADDR`, default [`DEFAULT_METRICS_ADDR`].
    pub metrics_addr: SocketAddr,
    /// `RECONCILE_INTERVAL` (seconds), default [`DEFAULT_RECONCILE_INTERVAL_SECS`].
    pub reconcile_interval: Duration,
    /// `METADATA_FETCH_INTERVAL` (seconds), default
    /// [`DEFAULT_METADATA_FETCH_INTERVAL_SECS`].
    pub metadata_fetch_interval: Duration,
    /// `WEBHOOK_URL`: the endpoint the property-asset-registration webhook POSTs to (ADR-28).
    /// `None` = the webhook is disabled -- the durable `webhook_events` rows are still
    /// recorded (the record), but the delivery loop is never spawned and no external call is
    /// ever made. Never logged (an operator may encode a bearer token in the query string).
    pub webhook_url: Option<String>,
    /// `WEBHOOK_INTERVAL` (seconds), default [`DEFAULT_WEBHOOK_INTERVAL_SECS`].
    pub webhook_interval: Duration,
    /// Object storage for the property image mirror (ADR-31). `None` (no `OBJECT_STORAGE_*`
    /// variables set) disables the mirror entirely -- the supervisor is never spawned and no
    /// external call is ever made. Partially-set `OBJECT_STORAGE_*` is a hard error (see
    /// [`object_storage_from_env`]).
    pub object_storage: Option<ObjectStorage>,
    /// `IMAGE_MIRROR_INTERVAL` (seconds), default [`DEFAULT_IMAGE_MIRROR_INTERVAL_SECS`].
    pub image_mirror_interval: Duration,
}

/// The object storage target for the mirrored image thumbnails (ADR-31).
///
/// Deliberately plain fields rather than an `s3::bucket::Bucket`: this is the *configuration*,
/// and the bucket client is built once per process (see [`crate::images`]), so the one-shot
/// `mirror-images` subcommand and tests can inspect what is configured without TLS
/// initialisation.
#[derive(Clone)]
pub struct ObjectStorage {
    /// `OBJECT_STORAGE_ENDPOINT` -- the S3 endpoint URL, e.g.
    /// `https://fsn1.<your-objectstorage-domain>`: `<region>.<domain>`, where the domain
    /// (a `*.your-objectstorage.com`-style name) is assigned per customer and shown in the
    /// Hetzner Console.
    pub endpoint: String,
    /// `OBJECT_STORAGE_BUCKET` -- the public-read bucket the thumbnails are uploaded to.
    pub bucket: String,
    /// `OBJECT_STORAGE_REGION` -- the bucket's location code, e.g. `fsn1` or `nbg1` (also
    /// the first label of the endpoint host).
    pub region: String,
    /// `OBJECT_STORAGE_ACCESS_KEY`. Never logged (`Debug` is hand-written below).
    pub access_key: String,
    /// `OBJECT_STORAGE_SECRET_KEY`. Never logged.
    pub secret_key: String,
    /// `OBJECT_STORAGE_PUBLIC_BASE_URL`, or `{scheme}://{bucket}.{host}` derived from the
    /// endpoint when unset (virtual-hosted, matching how the bucket serves objects): the
    /// prefix of every public thumbnail URL the API serves.
    pub public_base_url: String,
}

/// Hand-written so the access key / secret key never reach a log line via `{:?}` (the same
/// treatment as [`Config`]).
impl fmt::Debug for ObjectStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObjectStorage")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("access_key", &"<set>")
            .field("secret_key", &"<set>")
            .field("public_base_url", &self.public_base_url)
            .finish()
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let database_url = std::env::var("DATABASE_URL").ok().filter(|u| !u.is_empty());

        let alchemy_api_key = std::env::var("ALCHEMY_API_KEY")
            .ok()
            .filter(|k| !k.is_empty());

        let programs = match std::env::var("PROGRAMS") {
            Ok(s) if !s.trim().is_empty() => {
                let mut selected = Vec::new();
                for name in s.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                    let spec = programs::by_name(name).ok_or_else(|| {
                        anyhow!(
                            "PROGRAMS names an unknown program: {name} (known: {})",
                            programs::PROGRAMS
                                .iter()
                                .map(|p| p.name)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?;
                    if !selected.iter().any(|p: &&ProgramSpec| p.name == spec.name) {
                        selected.push(spec);
                    }
                }
                if selected.is_empty() {
                    return Err(anyhow!("PROGRAMS is set but selects no programs: {s:?}"));
                }
                selected
            }
            _ => programs::PROGRAMS.iter().collect(),
        };

        let metrics_addr_str =
            std::env::var("METRICS_ADDR").unwrap_or_else(|_| DEFAULT_METRICS_ADDR.to_string());
        let metrics_addr = metrics_addr_str.parse::<SocketAddr>().with_context(|| {
            format!("METRICS_ADDR is not a host:port address: {metrics_addr_str}")
        })?;

        // Intervals come from the environment; an empty or whitespace-only value means
        // "use the default" -- docker compose's `${VAR:-}` expands to exactly this when the
        // host variable is unset, and crashing on it was a 2026-09-03 prod restart loop.
        // Only a non-numeric value, or 0, is a hard error. Applies to every interval below.
        let reconcile_interval_secs = match std::env::var("RECONCILE_INTERVAL") {
            Ok(s) if !s.trim().is_empty() => s
                .parse::<u64>()
                .with_context(|| format!("RECONCILE_INTERVAL is not a u64 (seconds): {s}"))
                .and_then(|v| {
                    if v == 0 {
                        Err(anyhow!("RECONCILE_INTERVAL must be greater than 0 seconds"))
                    } else {
                        Ok(v)
                    }
                })?,
            _ => DEFAULT_RECONCILE_INTERVAL_SECS,
        };

        let metadata_fetch_interval_secs = match std::env::var("METADATA_FETCH_INTERVAL") {
            Ok(s) if !s.trim().is_empty() => s
                .parse::<u64>()
                .with_context(|| format!("METADATA_FETCH_INTERVAL is not a u64 (seconds): {s}"))
                .and_then(|v| {
                    if v == 0 {
                        Err(anyhow!(
                            "METADATA_FETCH_INTERVAL must be greater than 0 seconds"
                        ))
                    } else {
                        Ok(v)
                    }
                })?,
            _ => DEFAULT_METADATA_FETCH_INTERVAL_SECS,
        };

        // `WEBHOOK_URL` is optional: `None` (or empty) disables the webhook -- the durable
        // `webhook_events` rows are still recorded, but the delivery loop is never spawned.
        let webhook_url = std::env::var("WEBHOOK_URL").ok().filter(|u| !u.is_empty());

        let webhook_interval_secs = match std::env::var("WEBHOOK_INTERVAL") {
            Ok(s) if !s.trim().is_empty() => s
                .parse::<u64>()
                .with_context(|| format!("WEBHOOK_INTERVAL is not a u64 (seconds): {s}"))
                .and_then(|v| {
                    if v == 0 {
                        Err(anyhow!("WEBHOOK_INTERVAL must be greater than 0 seconds"))
                    } else {
                        Ok(v)
                    }
                })?,
            _ => DEFAULT_WEBHOOK_INTERVAL_SECS,
        };

        // Object storage (ADR-31) is all-or-nothing: any one of the five required
        // `OBJECT_STORAGE_*` variables set means all must be present -- a half-configured
        // mirror is a misconfiguration, not a degraded mode.
        let object_storage = object_storage_from_env()?;

        let image_mirror_interval_secs = match std::env::var("IMAGE_MIRROR_INTERVAL") {
            Ok(s) if !s.trim().is_empty() => s
                .parse::<u64>()
                .with_context(|| format!("IMAGE_MIRROR_INTERVAL is not a u64 (seconds): {s}"))
                .and_then(|v| {
                    if v == 0 {
                        Err(anyhow!(
                            "IMAGE_MIRROR_INTERVAL must be greater than 0 seconds"
                        ))
                    } else {
                        Ok(v)
                    }
                })?,
            _ => DEFAULT_IMAGE_MIRROR_INTERVAL_SECS,
        };

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
            programs,
            metrics_addr,
            reconcile_interval: Duration::from_secs(reconcile_interval_secs),
            metadata_fetch_interval: Duration::from_secs(metadata_fetch_interval_secs),
            webhook_url,
            webhook_interval: Duration::from_secs(webhook_interval_secs),
            object_storage,
            image_mirror_interval: Duration::from_secs(image_mirror_interval_secs),
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

    /// JSON-RPC endpoints in preference order: the primary, then the public devnet fallback
    /// (deduplicated when they are the same). Callers that can retry -- the crawl, the snapshot
    /// -- walk this list; Alchemy's free tier throttles, and every RPC read here is idempotent.
    pub fn rpc_endpoints(&self) -> Vec<String> {
        let primary = self.rpc_url();
        if primary == self.rpc_fallback_url {
            vec![primary]
        } else {
            vec![primary, self.rpc_fallback_url.clone()]
        }
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

/// The `OBJECT_STORAGE_*` variables (ADR-31), all-or-nothing: `None` (none set) disables the
/// image mirror entirely; any one set means all five must be present and non-empty.
fn object_storage_from_env() -> Result<Option<ObjectStorage>> {
    let non_empty = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let endpoint = non_empty("OBJECT_STORAGE_ENDPOINT");
    let bucket = non_empty("OBJECT_STORAGE_BUCKET");
    let region = non_empty("OBJECT_STORAGE_REGION");
    let access_key = non_empty("OBJECT_STORAGE_ACCESS_KEY");
    let secret_key = non_empty("OBJECT_STORAGE_SECRET_KEY");

    let missing: Vec<&str> = [
        ("OBJECT_STORAGE_ENDPOINT", &endpoint),
        ("OBJECT_STORAGE_BUCKET", &bucket),
        ("OBJECT_STORAGE_REGION", &region),
        ("OBJECT_STORAGE_ACCESS_KEY", &access_key),
        ("OBJECT_STORAGE_SECRET_KEY", &secret_key),
    ]
    .into_iter()
    .filter(|(_, v)| v.is_none())
    .map(|(name, _)| name)
    .collect();

    match missing.len() {
        // Not configured at all -> the mirror is disabled, which is a first-class state.
        5 => Ok(None),
        0 => {
            let endpoint = endpoint.expect("checked above");
            let bucket = bucket.expect("checked above");
            let public_base_url = match non_empty("OBJECT_STORAGE_PUBLIC_BASE_URL") {
                Some(base) => base,
                None => public_base_from_endpoint(&endpoint, &bucket)?,
            };
            Ok(Some(ObjectStorage {
                endpoint,
                bucket,
                region: region.expect("checked above"),
                access_key: access_key.expect("checked above"),
                secret_key: secret_key.expect("checked above"),
                public_base_url,
            }))
        }
        n => Err(anyhow!(
            "Object storage is partially configured: {n} of the five required \
             OBJECT_STORAGE_* variable(s) missing or empty ({}). Set all of \
             OBJECT_STORAGE_ENDPOINT, OBJECT_STORAGE_BUCKET, OBJECT_STORAGE_REGION, \
             OBJECT_STORAGE_ACCESS_KEY, OBJECT_STORAGE_SECRET_KEY -- or none of them to \
             disable the image mirror",
            missing.join(", ")
        )),
    }
}

/// `{scheme}://{bucket}.{host}` from the endpoint URL: Hetzner Object Storage serves objects
/// in virtual-hosted style (the bucket name as a subdomain of the endpoint host -- the same
/// shape its own docs use for object operations and the one the chain's metadata already
/// exhibits, e.g. `https://xcavate-profile.fsn1.your-objectstorage.com/...`), so
/// `OBJECT_STORAGE_PUBLIC_BASE_URL` can usually be left unset. (The endpoint must be a bare
/// `scheme://host` -- Object Storage endpoints have no path component -- so cutting at the
/// first `/` after the scheme is exact.)
fn public_base_from_endpoint(endpoint: &str, bucket: &str) -> Result<String> {
    let Some(scheme_end) = endpoint.find("://") else {
        return Err(anyhow!(
            "OBJECT_STORAGE_ENDPOINT is not a scheme://host URL: {endpoint:?}"
        ));
    };
    let host = endpoint[scheme_end + 3..]
        .split('/')
        .next()
        .unwrap_or_default();
    if host.is_empty() {
        return Err(anyhow!("OBJECT_STORAGE_ENDPOINT has no host: {endpoint:?}"));
    }
    Ok(format!(
        "{}://{}.{}",
        &endpoint[..scheme_end + 3],
        bucket,
        host
    ))
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
            .field(
                "programs",
                &self.programs.iter().map(|p| p.name).collect::<Vec<_>>(),
            )
            .field("metrics_addr", &self.metrics_addr)
            .field("reconcile_interval", &self.reconcile_interval)
            .field("metadata_fetch_interval", &self.metadata_fetch_interval)
            // The webhook URL may carry a bearer token in the query string: like the other
            // credentials, it is never logged -- only whether it is set.
            .field("webhook_url", &self.webhook_url.as_ref().map(|_| "<set>"))
            .field("webhook_interval", &self.webhook_interval)
            // ObjectStorage's Debug is hand-written to redact its keys.
            .field("object_storage", &self.object_storage.as_ref())
            .field("image_mirror_interval", &self.image_mirror_interval)
            .finish()
    }
}

/// Redacts an Alchemy API key embedded in a `/v2/<KEY>` URL path segment inside arbitrary text,
/// e.g. an already-formatted error message.
///
/// The key never appears in a URL we construct for logging (see `redact_url_password` above and
/// the "never log the URL" comments throughout `crawl.rs`/`snapshot.rs`/`block_time.rs`), but it
/// can still reach a log line indirectly: reqwest 0.12's `Error` Display appends
/// `" for url (<url>)"`, and solana-rpc-client's `RpcClient` attaches the same keyed URL via
/// `error_for_status()`. Any `{e}`/`{e:#}` formatting of such an error -- or of an anyhow chain
/// that wraps one -- would otherwise print the key. This scans for every `/v2/` occurrence and
/// replaces the run of non-slash, non-whitespace characters that follows it with `***`, so it is
/// applied at the log call site as a last line of defence regardless of how many layers of
/// context/anyhow wrapping the original error passed through.
///
/// Idempotent, and does nothing to a bare trailing `/v2/` with no key characters after it (no
/// key was there to begin with, so nothing is redacted).
pub fn redact_key(s: &str) -> String {
    const NEEDLE: &str = "/v2/";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find(NEEDLE) {
        out.push_str(&rest[..idx + NEEDLE.len()]);
        rest = &rest[idx + NEEDLE.len()..];
        let key_len = rest
            .find(|c: char| c == '/' || c.is_whitespace())
            .unwrap_or(rest.len());
        if key_len > 0 {
            out.push_str("***");
        }
        rest = &rest[key_len..];
    }
    out.push_str(rest);
    out
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
    use super::{redact_key, redact_url_password};

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

    // --- redact_key ------------------------------------------------------------------------

    #[test]
    fn redact_key_hides_a_key_mid_string() {
        // The key run extends to the next slash/whitespace, so it also swallows immediately
        // trailing punctuation like reqwest's closing `)` -- that is a feature, not a bug: it
        // errs on the side of redacting too much rather than leaving a fragment of the key
        // visible.
        let s = "reqwest::Error { kind: Status(429) } for url \
                  (https://solana-devnet.g.alchemy.com/v2/AbCdEf0123456789)";
        let redacted = redact_key(s);
        assert!(!redacted.contains("AbCdEf0123456789"));
        assert_eq!(
            redacted,
            "reqwest::Error { kind: Status(429) } for url \
             (https://solana-devnet.g.alchemy.com/v2/***"
        );
    }

    #[test]
    fn redact_key_hides_every_occurrence() {
        let s = "primary https://a.example.com/v2/KEY1 failed; fallback \
                  https://b.example.com/v2/KEY2 failed too";
        assert_eq!(
            redact_key(s),
            "primary https://a.example.com/v2/*** failed; fallback \
             https://b.example.com/v2/*** failed too"
        );
    }

    #[test]
    fn redact_key_does_not_touch_a_bare_v2_at_the_end_of_string() {
        // Nothing follows `/v2/`, so there is no key to redact -- must be a no-op, not a
        // spurious `/v2/***`.
        let s = "some diagnostic mentioning the path /v2/";
        assert_eq!(redact_key(s), s);
    }

    #[test]
    fn redact_key_is_idempotent() {
        let once =
            redact_key("failed for url (https://solana-devnet.g.alchemy.com/v2/AbCdEf0123456789)");
        let twice = redact_key(&once);
        assert_eq!(once, twice);
        assert_eq!(
            once,
            "failed for url (https://solana-devnet.g.alchemy.com/v2/***"
        );
    }
}
