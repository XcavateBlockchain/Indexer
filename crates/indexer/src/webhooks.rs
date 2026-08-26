//! The outbound webhook delivery loop (ADR-28): the background task that turns each durable
//! `webhook_events` row into a delivered `POST` to `WEBHOOK_URL`.
//!
//! ## Why a separate loop and not the write path
//!
//! The pipeline (`datasource -> decode -> map -> batcher`) is pure chain-mirror machinery:
//! every write is idempotent, slot-guarded, and must never block on the outside world. An
//! HTTP POST on the write path would couple ingestion of all four programs to the availability
//! of an operator endpoint, and the batcher's forever-retry on a deterministic failure (the
//! write-migration skill's stall trap) would stall everything on one dead endpoint. So the
//! mapper only RECORDS the event durably (`WriteOp::RecordWebhookEvent` -> `webhook_events`);
//! the delivery is a background loop with its own per-event backoff, reading the undelivered
//! rows and never touching the batcher.
//!
//! ## One cycle
//!
//! 1. `db::webhooks::pending_events` selects the work set: undelivered events whose backoff
//!    has elapsed (or has no backoff yet), bounded by [`CYCLE_LIMIT`].
//! 2. Each item (sequential): `POST WEBHOOK_URL` with the event's `payload`. A 2xx marks the
//!    row delivered; a failure records its error + exponential backoff (30 s, doubling, 1 h
//!    cap) and moves on; one event's fault never fails the loop.
//! 3. The `webhooks_pending` gauge is set to the remaining work-set size.
//!
//! Delivery is AT LEAST ONCE: a commit that succeeded on the server but errored on the wire is
//! retried and the endpoint receives the same event twice. The payload carries the stable
//! `pubkey` (the dedup key `event_id` is built from) and the `event` label, so a well-behaved
//! endpoint can dedupe. The at-most-once part is the RECORD (the
//! `ON CONFLICT (event_id) DO NOTHING` insert), so an asset is enqueued once.
//!
//! Active only when `WEBHOOK_URL` is set (and `marketplace` is in `PROGRAMS`); with no URL the
//! supervisor is never spawned and no external calls are ever made.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::db::webhooks::{self, PendingEvent};

/// How many pending events one cycle delivers. The rest wait for the next cycle: a cycle must
/// stay bounded so a stalled endpoint cannot stretch it past several intervals.
const CYCLE_LIMIT: i64 = 50;
/// `last_error` is an operator-facing log line in the database; keep it short.
const MAX_ERROR_LEN: usize = 500;

/// One cycle's outcome.
#[derive(Debug, Clone, Copy, Default)]
pub struct CycleSummary {
    /// Work-set items taken up this cycle.
    pub attempted: usize,
    /// Successful deliveries (row marked delivered).
    pub delivered: usize,
    /// Failed deliveries (error + backoff recorded on the row).
    pub failed: usize,
}

/// The URL's scheme + host (+ port), with no path/query/token -- safe to put in a log line or
/// a stored `last_error` (an operator may encode a bearer token in the query string).
pub fn host_of(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => {
            let mut s = format!("{}://{}", u.scheme(), u.host_str().unwrap_or("<no-host>"));
            if let Some(p) = u.port() {
                s.push_str(&format!(":{p}"));
            }
            s
        }
        Err(_) => "<invalid webhook url>".to_string(),
    }
}

/// One delivery attempt for one pending event: `POST` the payload to `url`; a 2xx is success,
/// anything else is a failure (the body is drained and discarded -- the endpoint is ours).
pub async fn deliver_one(client: &reqwest::Client, url: &str, event: &PendingEvent) -> Result<()> {
    let response = client
        .post(url)
        .json(&event.payload)
        .send()
        .await
        .with_context(|| format!("POST {} failed", host_of(url)))?;
    let status = response.status();
    let _ = response.text().await;
    if status.is_success() {
        Ok(())
    } else {
        Err(anyhow!("POST {} returned HTTP {status}", host_of(url)))
    }
}

/// One delivery cycle (also the whole job of a hypothetical one-shot `indexer deliver-webhooks`
/// if one were ever added): select the work set, deliver each item, mark successes, record
/// failures with backoff, and set the pending gauge. `shutdown` only gates BETWEEN items -- a
/// cancel stops the cycle, never an in-flight request (which has its own timeout from
/// `metadata::build_client`).
pub async fn cycle(
    pool: &PgPool,
    url: &str,
    client: &reqwest::Client,
    shutdown: &CancellationToken,
) -> Result<CycleSummary> {
    let pending = webhooks::pending_events(pool, CYCLE_LIMIT)
        .await
        .context("selecting the webhook delivery work set")?;

    let mut summary = CycleSummary::default();
    for event in &pending {
        if shutdown.is_cancelled() {
            break;
        }
        summary.attempted += 1;
        match deliver_one(client, url, event).await {
            Ok(()) => {
                webhooks::mark_delivered(pool, &event.event_id)
                    .await
                    .with_context(|| format!("marking webhook {} delivered", event.event_id))?;
                summary.delivered += 1;
                crate::metrics::inc_webhook_delivery("success");
                log::info!(
                    "webhook delivered: {} (POST {})",
                    event.event_id,
                    host_of(url)
                );
            }
            Err(e) => {
                summary.failed += 1;
                crate::metrics::inc_webhook_delivery("failure");
                // A failure's error + backoff is recorded on the row (survives restarts, feeds
                // the work set's retry gate). A dead endpoint degrades to a lagging,
                // retried-and-logged row -- never a lost notification, never a stalled pipeline.
                let error = format!("{e:#}");
                let error: String = error.chars().take(MAX_ERROR_LEN).collect();
                webhooks::record_failure(pool, &event.event_id, &error)
                    .await
                    .with_context(|| {
                        format!("recording the webhook failure for {}", event.event_id)
                    })?;
                log::warn!(
                    "webhook delivery failed for {} (POST {}): {error}; retrying with backoff",
                    event.event_id,
                    host_of(url)
                );
            }
        }
    }

    let remaining = webhooks::count_pending(pool)
        .await
        .context("counting the remaining webhook work set")?;
    crate::metrics::set_webhooks_pending(remaining);

    if summary.attempted > 0 {
        log::info!(
            "webhook delivery cycle: {} attempted, {} delivered, {} failed ({} still pending)",
            summary.attempted,
            summary.delivered,
            summary.failed,
            remaining
        );
    }
    Ok(summary)
}

/// Run delivery cycles until `shutdown` fires (spawned by `run` next to the metadata fetcher
/// and the reconciliation supervisor; ADR-28).
pub async fn supervise(
    pool: &PgPool,
    url: String,
    interval: Duration,
    shutdown: CancellationToken,
) {
    log::info!(
        "webhook delivery loop started (every {interval:?}, up to {CYCLE_LIMIT} event(s) per \
         cycle, target {})",
        host_of(&url)
    );
    let client = crate::metadata::build_client();
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        match cycle(pool, &url, &client, &shutdown).await {
            // `cycle` logs its own per-event and summary lines.
            Ok(_) => {}
            Err(e) => {
                log::error!("webhook delivery cycle failed (will retry next interval): {e:#}")
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.cancelled() => break,
        }
    }
    log::info!("webhook delivery loop stopping");
}

#[cfg(test)]
mod tests {
    use super::host_of;

    // --- host_of: never leak a path / query / token into a log or a stored last_error -----

    #[test]
    fn a_bare_url_round_trips_to_its_host() {
        assert_eq!(
            host_of("https://hooks.example.com/asset"),
            "https://hooks.example.com"
        );
    }

    #[test]
    fn a_query_string_token_is_not_leaked() {
        // The whole reason host_of exists: an operator may put the shared secret in the query.
        assert_eq!(
            host_of("https://hooks.example.com/asset?token=super-secret-value"),
            "https://hooks.example.com"
        );
    }

    #[test]
    fn a_port_is_kept_and_the_path_and_query_dropped() {
        assert_eq!(
            host_of("http://1.2.3.4:8080/hook?x=1"),
            "http://1.2.3.4:8080"
        );
    }

    #[test]
    fn an_unparseable_url_is_labelled_not_panic() {
        assert_eq!(host_of("not a url at all"), "<invalid webhook url>");
    }
}
