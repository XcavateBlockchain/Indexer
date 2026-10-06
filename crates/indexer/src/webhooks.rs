//! The outbound webhook delivery loop (ADR-28/ADR-36): the background task that turns each
//! durable `webhook_events` row into a delivered `POST` to the endpoint URL configured for
//! the row's `event_type`.
//!
//! ## Why a separate loop and not the write path
//!
//! The pipeline (`datasource -> decode -> map -> batcher`) is pure chain-mirror machinery:
//! every write is idempotent, slot-guarded, and must never block on the outside world. An
//! HTTP POST on the write path would couple ingestion of all five programs to the availability
//! of an operator endpoint, and the batcher's forever-retry on a deterministic failure (the
//! write-migration skill's stall trap) would stall everything on one dead endpoint. So the
//! mapper only RECORDS the event durably (`WriteOp::RecordWebhookEvent` -> `webhook_events`);
//! the delivery is a background loop with its own per-event backoff, reading the undelivered
//! rows and never touching the batcher.
//!
//! ## Routing (ADR-36)
//!
//! Each event type has its own optional endpoint (`Config::webhook_routes`, one
//! `*_WEBHOOK_URL` env var per type). The work-set query selects only the configured types,
//! so an unrouted type's rows accumulate undelivered and drain if their URL is ever set --
//! per-type the same stance ADR-28 took for the whole loop.
//!
//! ## One cycle
//!
//! 1. `db::webhooks::pending_events` selects the work set: undelivered events of the
//!    configured types whose backoff has elapsed (or has no backoff yet), bounded by
//!    [`CYCLE_LIMIT`].
//! 2. Each item (sequential): POST its type's URL with the event's `payload`. A 2xx marks the
//!    row delivered; a failure records its error + exponential backoff (30 s, doubling, 1 h
//!    cap) and moves on; one event's fault never fails the loop.
//! 3. The `webhooks_pending` gauge is set to the remaining work-set size.
//!
//! Delivery is AT LEAST ONCE: a commit that succeeded on the server but errored on the wire is
//! retried and the endpoint receives the same event twice. Every payload carries the stable
//! `event` label plus the event's subject keys (asset/listing PDA, transaction signature),
//! so a well-behaved endpoint can dedupe; the mapper-facing doc on
//! [`crate::mapping::WebhookEvent`] explains the `event_id` shapes. The at-most-once part is
//! the RECORD (the `ON CONFLICT (event_id) DO NOTHING` insert).
//!
//! Active only when at least one `*_WEBHOOK_URL` is set (and `marketplace` or `property` --
//! the event sources -- is in `PROGRAMS`); with no routes the supervisor is never spawned and
//! no external calls are ever made.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::config::WebhookRoute;
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
/// if one were ever added): select the work set (configured types only), deliver each item to
/// its type's URL, mark successes, record failures with backoff, and set the pending gauge.
/// `shutdown` only gates BETWEEN items -- a cancel stops the cycle, never an in-flight request
/// (which has its own timeout from `metadata::build_client`).
pub async fn cycle(
    pool: &PgPool,
    routes: &[WebhookRoute],
    client: &reqwest::Client,
    shutdown: &CancellationToken,
) -> Result<CycleSummary> {
    let types: Vec<&str> = routes.iter().map(|r| r.event_type).collect();
    let pending = webhooks::pending_events(pool, CYCLE_LIMIT, &types)
        .await
        .context("selecting the webhook delivery work set")?;

    let mut summary = CycleSummary::default();
    for event in &pending {
        if shutdown.is_cancelled() {
            break;
        }
        // The work set is SQL-filtered to the configured types, so a missing route is
        // impossible by construction; if it ever happens (a row recorded under a renamed
        // type), skip the event WITHOUT recording a failure -- misrouting it or retrying it
        // forever would both be wrong.
        let Some(route) = routes
            .iter()
            .find(|r| r.event_type == event.event_type.as_str())
        else {
            log::warn!(
                "webhook {} has unconfigured type {:?}; leaving it pending",
                event.event_id,
                event.event_type
            );
            continue;
        };
        summary.attempted += 1;
        match deliver_one(client, &route.url, event).await {
            Ok(()) => {
                webhooks::mark_delivered(pool, &event.event_id)
                    .await
                    .with_context(|| format!("marking webhook {} delivered", event.event_id))?;
                summary.delivered += 1;
                crate::metrics::inc_webhook_delivery("success");
                log::info!(
                    "webhook delivered: {} (POST {})",
                    event.event_id,
                    host_of(&route.url)
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
                    host_of(&route.url)
                );
            }
        }
    }

    let remaining = webhooks::count_pending(pool, &types)
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
/// and the reconciliation supervisor; ADR-28/ADR-36). `routes` is non-empty (the caller's
/// spawn condition); the start log lists each route's type and host -- never the full URL,
/// which may carry a bearer token in the query string.
pub async fn supervise(
    pool: &PgPool,
    routes: Vec<WebhookRoute>,
    interval: Duration,
    shutdown: CancellationToken,
) {
    let route_list = routes
        .iter()
        .map(|r| format!("{} -> {}", r.event_type, host_of(&r.url)))
        .collect::<Vec<_>>()
        .join(", ");
    log::info!(
        "webhook delivery loop started (every {interval:?}, up to {CYCLE_LIMIT} event(s) per \
         cycle, routes: {route_list})"
    );
    let client = crate::metadata::build_client();
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        match cycle(pool, &routes, &client, &shutdown).await {
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
