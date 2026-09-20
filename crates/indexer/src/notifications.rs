//! The sold-out claim notification delivery loop (ADR-35): the background task that turns
//! each durable `sold_out_notifications` row into one push notification per reserver of
//! that listing, sent through the Xcavate notifications API
//! (`POST {NOTIFICATIONS_API_URL}/api/fcm/send-notification/`,
//! `Authorization: Api-Key <NOTIFICATIONS_API_KEY>`).
//!
//! ## Why a separate loop and not the write path
//!
//! Identical argument to ADR-28 (`crate::webhooks`): the pipeline is pure chain-mirror
//! machinery whose every write is idempotent and slot-guarded and must never block on the
//! outside world. The batcher only RECORDS the event durably (same transaction as the
//! `marketplace_listing` upsert that showed `SOLD_OUT`); delivery happens here, with its
//! own per-event backoff, reading the undelivered rows and never touching the batcher.
//!
//! ## One event's delivery
//!
//! Everything that decides WHETHER and WHAT to send is read fresh from the mirror at send
//! time (`db::notifications::event_context` / `reservers`), never from the event row, so
//! the decision self-corrects as the chain moves on:
//!
//! 1. Listing or property-asset row not visible yet -> transient failure, retry next cycle
//!    (the recording upsert commits with the event, but the asset row can land a batch
//!    later; a cross-table read has no ordering guarantee).
//! 2. `claim_deadline` already past -> the event is DONE without a single POST: reservers
//!    can no longer claim, so "must be claimed now" would be a lie.
//! 3. The current reservers (live, uncancelled positions with shares still reserved) each
//!    get one POST: 2xx is a send; 404 is a permanent SKIP -- the notifications API
//!    documents "no device holds that address" as a normal outcome (the investor never
//!    registered a device), not an incident; anything else (401 bad key, 5xx, network) is
//!    transient: the fan-out aborts and the WHOLE event retries with backoff.
//! 4. Every reserver sent-or-skipped (or none remain) -> the row is marked delivered.
//!
//! Delivery is AT LEAST ONCE per investor: a crash or transient failure mid-fan-out retries
//! the whole event and an already-notified investor gets the push again. The at-most-once
//! part is the RECORD (`ON CONFLICT (event_id) DO NOTHING`), so a listing's sold-out is
//! enqueued once.
//!
//! Active only when `NOTIFICATIONS_API_URL` + `NOTIFICATIONS_API_KEY` are set (and
//! `marketplace` is in `PROGRAMS`); unset, the supervisor is never spawned, no external
//! call is ever made, and recorded events accumulate until it is configured.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::config::NotificationsApiConfig;
use crate::db::notifications::{self, PendingNotification};

/// How many pending events one cycle works through. The rest wait for the next cycle: a
/// cycle must stay bounded so a stalled notifications API cannot stretch it past several
/// intervals.
const CYCLE_LIMIT: i64 = 50;
/// `last_error` is an operator-facing log line in the database; keep it short.
const MAX_ERROR_LEN: usize = 500;

/// The notifications API's send path. Trailing slash is LOAD-BEARING: Django's
/// APPEND_SLASH answers the unslashed form with a 301 that reqwest re-issues as a bodyless
/// GET (the API's own api-reference.md warns about exactly this).
const SEND_PATH: &str = "/api/fcm/send-notification/";

/// The API's field limits (api-reference.md): `title` <= 150 chars, `body` <= 500.
const TITLE_MAX: usize = 150;
const BODY_MAX: usize = 500;

/// One cycle's outcome.
#[derive(Debug, Clone, Copy, Default)]
pub struct CycleSummary {
    /// Work-set events taken up this cycle.
    pub attempted: usize,
    /// Events finished (every reserver sent or skipped, or nothing left to do).
    pub delivered: usize,
    /// Events that failed transiently (error + backoff recorded on the row).
    pub failed: usize,
}

/// The `{base}/api/fcm/send-notification/` join, tolerant of an operator pasting the base
/// URL with or without trailing slashes.
pub fn send_url(base: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), SEND_PATH)
}

/// The (title, body) of the push for one property. The mandated sentence -- "Property
/// <name> tokens must be claimed now" -- is the body, verbatim; the title adds the
/// sold-out context. The on-chain name is unbounded, so it is clamped to the budget that
/// keeps BOTH fields inside the API's limits (title `Property <name> sold out` is the
/// tighter one).
pub fn build_message(property_name: &str) -> (String, String) {
    const TITLE_PREFIX: &str = "Property ";
    const TITLE_SUFFIX: &str = " sold out";
    let name_budget = TITLE_MAX - TITLE_PREFIX.len() - TITLE_SUFFIX.len();
    let name: String = property_name.chars().take(name_budget).collect();
    let title = format!("{TITLE_PREFIX}{name}{TITLE_SUFFIX}");
    let body = format!("Property {name} tokens must be claimed now");
    debug_assert!(title.chars().count() <= TITLE_MAX);
    debug_assert!(body.chars().count() <= BODY_MAX);
    (title, body)
}

/// The JSON body of one `send-notification` call, targeting one wallet on Solana only (the
/// chain qualifier stops a bare address from also matching another chain's registration or
/// a legacy uid). `data` values must be strings -- the API rejects anything else with a
/// 400; the app gets them as launch extras when the push is tapped.
pub fn build_payload(
    address_b58: &str,
    title: &str,
    body: &str,
    listing_id: i64,
) -> serde_json::Value {
    serde_json::json!({
        "chain": "solana",
        "address": address_b58,
        "title": title,
        "body": body,
        "data": {
            "kind": "listing_sold_out",
            "listing_id": listing_id.to_string(),
        },
    })
}

/// What one per-investor POST came back as.
enum InvestorOutcome {
    /// 2xx -- the API attempted delivery to the investor's device(s).
    Sent,
    /// 404 -- no registered device holds this wallet (a normal outcome per the API's
    /// docs: the investor never linked it, or never reported an FCM token).
    Skipped,
}

/// One send attempt for one investor: POST the payload with the API key. Only a 2xx is a
/// send and only a 404 is a permanent skip; every other outcome (401/403 = key problem,
/// 400 = our bug, 5xx, connect error, timeout) is a transient error that fails the whole
/// event so it retries with backoff and surfaces via the failure counter + alert. The
/// response body is drained and folded into the error (truncated downstream) -- the
/// endpoint is ours, and its error JSON names the failing field.
async fn notify_investor(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    payload: &serde_json::Value,
) -> Result<InvestorOutcome> {
    let response = client
        .post(url)
        .header("Authorization", format!("Api-Key {api_key}"))
        .json(payload)
        .send()
        .await
        .with_context(|| format!("POST {} failed", crate::webhooks::host_of(url)))?;
    let status = response.status();
    if status.is_success() {
        let _ = response.text().await;
        Ok(InvestorOutcome::Sent)
    } else if status == reqwest::StatusCode::NOT_FOUND {
        let _ = response.text().await;
        Ok(InvestorOutcome::Skipped)
    } else {
        let body = response.text().await.unwrap_or_default();
        let body: String = body.chars().take(200).collect();
        Err(anyhow!(
            "POST {} returned HTTP {status}: {body}",
            crate::webhooks::host_of(url)
        ))
    }
}

/// All send-time decisions for one event (see the module docs): load the context, apply the
/// claim-deadline gate, fan out over the current reservers. `Ok(report)` means the event is
/// DONE (mark it delivered); `Err` means transient -- the row gets its error + backoff and
/// the whole event retries later.
async fn deliver_event(
    pool: &PgPool,
    client: &reqwest::Client,
    api: &NotificationsApiConfig,
    event: &PendingNotification,
) -> Result<String> {
    let context = notifications::event_context(pool, event.listing_id)
        .await
        .context("reading the sold-out event's context")?
        .ok_or_else(|| {
            anyhow!(
                "listing {} is not indexed yet (transient)",
                event.listing_id
            )
        })?;
    let property_name = context.property_name.ok_or_else(|| {
        anyhow!(
            "property asset for listing {} is not indexed yet (transient)",
            event.listing_id
        )
    })?;

    if context.claim_deadline <= chrono::Utc::now().timestamp() {
        return Ok(format!(
            "skipped: the claim window for listing {} ({property_name}) closed at {} \
             (status {})",
            event.listing_id, context.claim_deadline, context.status
        ));
    }

    let reservers = notifications::reservers(pool, event.listing_id)
        .await
        .context("reading the sold-out event's reservers")?;
    if reservers.is_empty() {
        return Ok(format!(
            "skipped: listing {} ({property_name}) has no outstanding reservations",
            event.listing_id
        ));
    }

    let url = send_url(&api.url);
    let (title, body) = build_message(&property_name);
    let mut sent = 0usize;
    let mut skipped = 0usize;
    for investor in &reservers {
        let address = bs58::encode(investor).into_string();
        let payload = build_payload(&address, &title, &body, event.listing_id);
        match notify_investor(client, &url, &api.api_key, &payload).await {
            Ok(InvestorOutcome::Sent) => sent += 1,
            Ok(InvestorOutcome::Skipped) => {
                skipped += 1;
                log::info!(
                    "sold-out notification for listing {} not sent to {address}: no \
                     registered device (404)",
                    event.listing_id
                );
            }
            // A transient failure aborts the fan-out: hammering a struggling API once per
            // remaining investor helps nobody. The whole event retries with backoff --
            // already-sent investors may get one duplicate push (at-least-once, ADR-35).
            Err(e) => return Err(e),
        }
    }
    Ok(format!(
        "listing {} ({property_name}): {sent} sent, {skipped} skipped (unregistered) of \
         {} reserver(s)",
        event.listing_id,
        reservers.len()
    ))
}

/// One delivery cycle: select the work set, deliver each event, mark the finished ones,
/// record failures with backoff, and set the pending gauge. `shutdown` only gates BETWEEN
/// events -- a cancel stops the cycle, never an in-flight request (which has its own
/// timeout from `metadata::build_client`).
pub async fn cycle(
    pool: &PgPool,
    api: &NotificationsApiConfig,
    client: &reqwest::Client,
    shutdown: &CancellationToken,
) -> Result<CycleSummary> {
    let pending = notifications::pending_events(pool, CYCLE_LIMIT)
        .await
        .context("selecting the sold-out notification work set")?;

    let mut summary = CycleSummary::default();
    for event in &pending {
        if shutdown.is_cancelled() {
            break;
        }
        summary.attempted += 1;
        match deliver_event(pool, client, api, event).await {
            Ok(report) => {
                notifications::mark_delivered(pool, &event.event_id)
                    .await
                    .with_context(|| {
                        format!("marking sold-out notification {} delivered", event.event_id)
                    })?;
                summary.delivered += 1;
                crate::metrics::inc_notification_delivery("success");
                log::info!("sold-out notification done: {} ({report})", event.event_id);
            }
            Err(e) => {
                summary.failed += 1;
                crate::metrics::inc_notification_delivery("failure");
                // A failure's error + backoff is recorded on the row (survives restarts,
                // feeds the work set's retry gate). A dead notifications API degrades to a
                // lagging, retried-and-logged row -- never a lost notification, never a
                // stalled pipeline.
                let error = format!("{e:#}");
                let error: String = error.chars().take(MAX_ERROR_LEN).collect();
                notifications::record_failure(pool, &event.event_id, &error)
                    .await
                    .with_context(|| {
                        format!(
                            "recording the sold-out notification failure for {}",
                            event.event_id
                        )
                    })?;
                log::warn!(
                    "sold-out notification failed for {} (POST {}): {error}; retrying with \
                     backoff",
                    event.event_id,
                    crate::webhooks::host_of(&send_url(&api.url))
                );
            }
        }
    }

    let remaining = notifications::count_pending(pool)
        .await
        .context("counting the remaining sold-out notification work set")?;
    crate::metrics::set_notifications_pending(remaining);

    if summary.attempted > 0 {
        log::info!(
            "sold-out notification cycle: {} attempted, {} delivered, {} failed ({} still \
             pending)",
            summary.attempted,
            summary.delivered,
            summary.failed,
            remaining
        );
    }
    Ok(summary)
}

/// Run delivery cycles until `shutdown` fires (spawned by `run` next to the webhook
/// delivery loop and the image mirror; ADR-35).
pub async fn supervise(
    pool: &PgPool,
    api: NotificationsApiConfig,
    interval: Duration,
    shutdown: CancellationToken,
) {
    log::info!(
        "sold-out notification loop started (every {interval:?}, up to {CYCLE_LIMIT} \
         event(s) per cycle, target {})",
        crate::webhooks::host_of(&api.url)
    );
    let client = crate::metadata::build_client();
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        match cycle(pool, &api, &client, &shutdown).await {
            // `cycle` logs its own per-event and summary lines.
            Ok(_) => {}
            Err(e) => {
                log::error!("sold-out notification cycle failed (will retry next interval): {e:#}")
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.cancelled() => break,
        }
    }
    log::info!("sold-out notification loop stopping");
}

#[cfg(test)]
mod tests {
    use super::{build_message, build_payload, send_url, BODY_MAX, TITLE_MAX};

    // --- send_url: the trailing slash is load-bearing (Django APPEND_SLASH) -------------

    #[test]
    fn a_bare_base_url_gets_the_slashed_send_path() {
        assert_eq!(
            send_url("https://notify.example.com"),
            "https://notify.example.com/api/fcm/send-notification/"
        );
    }

    #[test]
    fn trailing_slashes_on_the_base_are_not_doubled() {
        assert_eq!(
            send_url("https://notify.example.com/"),
            "https://notify.example.com/api/fcm/send-notification/"
        );
        assert_eq!(
            send_url("https://notify.example.com//"),
            "https://notify.example.com/api/fcm/send-notification/"
        );
    }

    #[test]
    fn a_base_url_with_a_path_prefix_is_kept() {
        // A reverse proxy may mount the API under a prefix; the send path appends to it.
        assert_eq!(
            send_url("https://example.com/notify"),
            "https://example.com/notify/api/fcm/send-notification/"
        );
    }

    // --- build_message: the mandated sentence, inside the API's field limits ------------

    #[test]
    fn the_body_is_the_mandated_sentence() {
        let (title, body) = build_message("Sunset Villas");
        assert_eq!(body, "Property Sunset Villas tokens must be claimed now");
        assert_eq!(title, "Property Sunset Villas sold out");
    }

    #[test]
    fn an_unbounded_on_chain_name_is_clamped_into_the_limits() {
        let long: String = "x".repeat(1000);
        let (title, body) = build_message(&long);
        assert!(title.chars().count() <= TITLE_MAX);
        assert!(body.chars().count() <= BODY_MAX);
        // Even clamped, the body still says the required words.
        assert!(body.starts_with("Property "));
        assert!(body.ends_with(" tokens must be claimed now"));
    }

    // --- build_payload: the API's exact contract ----------------------------------------

    #[test]
    fn the_payload_targets_a_solana_wallet_with_string_data() {
        let payload = build_payload("9xQeABC", "t", "b", 42);
        assert_eq!(payload["chain"], "solana");
        assert_eq!(payload["address"], "9xQeABC");
        assert_eq!(payload["title"], "t");
        assert_eq!(payload["body"], "b");
        // `data` values must be STRINGS -- the API 400s anything else.
        assert_eq!(payload["data"]["kind"], "listing_sold_out");
        assert_eq!(payload["data"]["listing_id"], "42");
        assert!(payload["data"]["listing_id"].is_string());
        // Exactly one targeting mode: chain + address, never a bare user_id alongside.
        assert!(payload.get("user_id").is_none());
    }
}
