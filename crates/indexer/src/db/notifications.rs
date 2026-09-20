//! Row shapes and writes for the DURABLE sold-out-notification table
//! (`migrations/0018_sold_out_notifications.sql`, ADR-35). Same `query!`-macro contract as
//! [`super::webhooks`], and like that table this is NOT an account-state mirror: there is no
//! slot guard, no soft close, and no `StateTable` entry (see the migration header for the
//! full argument). Two writers:
//!
//! * the batcher ([`super::super::batcher`]) records each event with [`record_event`]
//!   (`INSERT ... ON CONFLICT (event_id) DO NOTHING` -- idempotent under backfill re-walks,
//!   so a listing's sold-out is announced at most once);
//! * the delivery loop (`crate::notifications`) is the only writer of the delivery-state
//!   columns ([`mark_delivered`] / [`record_failure`]), and the only reader of the work set
//!   ([`pending_events`]) and its per-event fan-out inputs ([`event_context`] /
//!   [`reservers`]).

use sqlx::postgres::PgQueryResult;
use sqlx::PgExecutor;

/// One undelivered sold-out event, ready to fan out into per-reserver push notifications.
#[derive(Debug, Clone)]
pub struct PendingNotification {
    /// The `sold_out_notifications` primary key (`listing_sold_out:<base58 Listing PDA>`).
    pub event_id: String,
    /// The sale's `listing_id` (fan-out input for [`reservers`] / [`event_context`]).
    pub listing_id: i64,
}

/// The send-time context of one sold-out event: everything the delivery loop needs to
/// decide WHETHER and WHAT to send, read fresh from the current mirror state (never from
/// the event row) so the decision self-corrects as the chain moves on.
#[derive(Debug, Clone)]
pub struct EventContext {
    /// `marketplace_property_asset.name` for the listing's `asset_id`; `None` when the asset
    /// row is not indexed yet (the loop treats that as transient and retries).
    pub property_name: Option<String>,
    /// `marketplace_listing.claim_deadline` (unix seconds): once it has passed, reservers
    /// can no longer claim and the notification would be wrong.
    pub claim_deadline: i64,
    /// `marketplace_listing.status` (the CHECK-constrained TEXT spelling, e.g. `SOLD_OUT`).
    pub status: String,
}

/// Record one sold-out event. Idempotent: a backfill/reconciliation re-walk re-upserts the
/// same SOLD_OUT listing state, and the `ON CONFLICT (event_id) DO NOTHING` makes the
/// re-record a no-op -- each listing's sold-out is announced at most once, ever.
///
/// The delivery-state columns (`attempts` / `next_attempt_at` / `last_error` /
/// `delivered_at`) take their column defaults: a freshly recorded event is pending
/// (0 attempts, no backoff, undelivered).
pub async fn record_event<'e, E>(
    executor: E,
    event_id: &str,
    listing_id: i64,
    asset_id: i64,
    slot: i64,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO sold_out_notifications (event_id, listing_id, asset_id, slot)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (event_id) DO NOTHING
        "#,
        event_id,
        listing_id,
        asset_id,
        slot,
    )
    .execute(executor)
    .await
}

/// The delivery work set: undelivered events whose backoff has elapsed (or has no backoff
/// yet). Ordered by `event_id` so a cycle's batch is deterministic; bounded by `limit`.
pub async fn pending_events<'e, E>(
    executor: E,
    limit: i64,
) -> Result<Vec<PendingNotification>, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let rows = sqlx::query!(
        r#"
        SELECT event_id, listing_id
        FROM sold_out_notifications
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
        .map(|r| PendingNotification {
            event_id: r.event_id,
            listing_id: r.listing_id,
        })
        .collect())
}

/// The send-time context of one event (see [`EventContext`]). `None` when the listing row
/// is not indexed yet -- the event was recorded by the listing upsert itself, so a missing
/// row can only be a cross-table visibility race the next cycle resolves (transient).
pub async fn event_context<'e, E>(
    executor: E,
    listing_id: i64,
) -> Result<Option<EventContext>, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query_as!(
        EventContext,
        r#"
        SELECT a.name AS "property_name?", l.claim_deadline, l.status
        FROM marketplace_listing l
        LEFT JOIN marketplace_property_asset a
               ON a.asset_id = l.asset_id
              AND a.closed_at_slot IS NULL
        WHERE l.listing_id = $1
          AND l.closed_at_slot IS NULL
        ORDER BY l.slot DESC
        LIMIT 1
        "#,
        listing_id,
    )
    .fetch_optional(executor)
    .await
}

/// The current reservers of one listing: investors holding a live, uncancelled position
/// with shares still in reservation. Read at SEND time, so claims / un-reserves / crank
/// releases between record and delivery shrink the set and nobody is told to claim what
/// they no longer hold. Ordered for a deterministic fan-out; `DISTINCT` is cheap insurance
/// over the one-position-per-(listing, investor) seed invariant.
pub async fn reservers<'e, E>(executor: E, listing_id: i64) -> Result<Vec<Vec<u8>>, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let rows = sqlx::query!(
        r#"
        SELECT DISTINCT investor
        FROM marketplace_investor_position
        WHERE listing_id = $1
          AND reserved_share_amount > 0
          AND cancelled = false
          AND closed_at_slot IS NULL
        ORDER BY investor ASC
        "#,
        listing_id,
    )
    .fetch_all(executor)
    .await?;

    Ok(rows.into_iter().map(|r| r.investor).collect())
}

/// A finished event: stamp the finish time and clear the retry state. "Finished" per the
/// migration header: every current reserver got a 2xx or a permanent skip (404 = wallet not
/// registered), or the claim window had already closed, or nobody holds a reservation.
pub async fn mark_delivered<'e, E>(
    executor: E,
    event_id: &str,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        UPDATE sold_out_notifications
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
/// `db::webhooks::record_failure`). The event stays pending (still in the work set once the
/// deadline elapses); a dead notifications API degrades to a lagging, retried-and-logged
/// row, never to a lost notification.
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
        UPDATE sold_out_notifications
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

/// How many events are awaiting delivery right now (the `notifications_pending` gauge,
/// ADR-35).
pub async fn count_pending<'e, E>(executor: E) -> Result<i64, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let count: Option<i64> = sqlx::query_scalar!(
        r#"
        SELECT count(*)
        FROM sold_out_notifications
        WHERE delivered_at IS NULL
          AND (next_attempt_at IS NULL OR next_attempt_at <= now())
        "#
    )
    .fetch_one(executor)
    .await?;

    Ok(count.unwrap_or(0))
}
