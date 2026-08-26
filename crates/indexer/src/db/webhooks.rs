//! Row shapes and writes for the DURABLE webhook-event table (`migrations/0014_webhook_events.sql`,
//! ADR-28). Same `query!`-macro contract as [`super::property_metadata`], but this table is NOT
//! an account-state mirror: there is no slot guard, no soft close, and no `StateTable` entry
//! (see the migration header for the full argument). Two writers:
//!
//! * the batcher ([`super::super::batcher`]) records each event with
//!   [`record_event`] (`INSERT ... ON CONFLICT (event_id) DO NOTHING` -- idempotent under
//!   backfill re-walks, so an asset is announced at most once);
//! * the delivery loop (`crate::webhooks`) is the only writer of the delivery-state columns
//!   ([`mark_delivered`] / [`record_failure`]), and the only reader of the work set
//!   ([`pending_events`]).

use chrono::{DateTime, Utc};
use sqlx::postgres::PgQueryResult;
use sqlx::PgExecutor;

/// One undelivered webhook event, ready to POST to `WEBHOOK_URL`.
#[derive(Debug, Clone)]
pub struct PendingEvent {
    /// The `webhook_events` primary key (`<event_type>:<base58 subject>`).
    pub event_id: String,
    /// The JSON document to POST, verbatim.
    pub payload: serde_json::Value,
}

/// Record one webhook event. Idempotent: a backfill/reconciliation re-walk re-delivers the same
/// `init_property_assets` instruction, and the `ON CONFLICT (event_id) DO NOTHING` makes the
/// re-record a no-op -- each asset is announced at most once, ever.
///
/// The delivery-state columns (`attempts` / `next_attempt_at` / `last_error` / `delivered_at`)
/// take their column defaults: a freshly recorded event is pending (0 attempts, no backoff,
/// undelivered).
pub async fn record_event<'e, E>(
    executor: E,
    event_id: &str,
    event_type: &str,
    payload: &serde_json::Value,
    slot: i64,
    tx_signature: &str,
    block_time: DateTime<Utc>,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO webhook_events (
            event_id, event_type, payload, slot, tx_signature, block_time
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (event_id) DO NOTHING
        "#,
        event_id,
        event_type,
        payload,
        slot,
        tx_signature,
        block_time,
    )
    .execute(executor)
    .await
}

/// The delivery work set: undelivered events whose backoff has elapsed (or has no backoff yet).
/// Ordered by `event_id` so a cycle's batch is deterministic; bounded by `limit`.
pub async fn pending_events<'e, E>(
    executor: E,
    limit: i64,
) -> Result<Vec<PendingEvent>, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let rows = sqlx::query!(
        r#"
        SELECT event_id, payload
        FROM webhook_events
        WHERE delivered_at IS NULL
          AND (next_attempt_at IS NULL OR next_attempt_at <= now())
        ORDER BY event_id ASC
        LIMIT $1
        "#,
        limit,
    )
    .fetch_all(executor)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| PendingEvent {
            event_id: r.event_id,
            payload: r.payload,
        })
        .collect())
}

/// A successful delivery: stamp the delivery time and clear the retry state.
pub async fn mark_delivered<'e, E>(
    executor: E,
    event_id: &str,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        UPDATE webhook_events
        SET delivered_at = now(),
            attempts = 0,
            next_attempt_at = NULL,
            last_error = NULL
        WHERE event_id = $1
        "#,
        event_id,
    )
    .execute(executor)
    .await
}

/// A failed delivery: record the attempt's error and schedule the next one with exponential
/// backoff (30 s, doubling per consecutive failure, 1 h cap -- computed in SQL, mirroring
/// `db::property_metadata::record_failure`). The event stays pending (still in the work set
/// once the deadline elapses); a dead endpoint degrades to a lagging, retried-and-logged row,
/// never to a lost notification.
pub async fn record_failure<'e, E>(
    executor: E,
    event_id: &str,
    error: &str,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        UPDATE webhook_events
        SET attempts = attempts + 1,
            next_attempt_at = now() + (
                LEAST(3600, (30 * power(2, LEAST(attempts, 20))))::bigint
                * interval '1 second'),
            last_error = $2
        WHERE event_id = $1
        "#,
        event_id,
        error,
    )
    .execute(executor)
    .await
}

/// How many events are awaiting delivery right now (the `webhooks_pending` gauge, ADR-28).
pub async fn count_pending<'e, E>(executor: E) -> Result<i64, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let count: Option<i64> = sqlx::query_scalar!(
        r#"
        SELECT count(*)
        FROM webhook_events
        WHERE delivered_at IS NULL
          AND (next_attempt_at IS NULL OR next_attempt_at <= now())
        "#
    )
    .fetch_one(executor)
    .await?;

    Ok(count.unwrap_or(0))
}
