//! `marketplace_property_image` state (ADR-20: every state table carries
//! `attempts`, `next_attempt_at`, `last_error`) — one row per property-image URI
//! referenced by an open asset's fetched metadata document (ADR-31). Same
//! `query!`-macro contract as [`super::property_metadata`]: a DERIVED table (no slot
//! guard, no soft close, no `StateTable` entry), owned by the image mirror
//! (`crate::images`) — which is the only writer.
//!
//! Column shape (see `migrations/0016_property_images.sql` for the full argument):
//! `source_uri` is the URI the mirror last attempted for `(asset_pubkey, image_index)`;
//! `thumb_uri` / `uploaded_at` are the last SUCCESSFUL upload (the public 720x720
//! thumbnail URL the GraphQL API serves, NULL until one exists); the mirror state
//! (`attempts` / `next_attempt_at` / `last_error`) is the per-image backoff chain — a
//! dead image (404, non-image body, storage outage) backs off INDEPENDENTLY of its
//! siblings, so one bad URI can never stall the work set or its siblings. The
//! zero-based `image_index` is the entry's position in the metadata's
//! `property_images` array.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgQueryResult;
use sqlx::PgExecutor;

/// One work-set entry: an image of an open asset that needs an upload.
#[derive(Debug, Clone)]
pub struct PendingImage {
    pub asset_pubkey: Vec<u8>,
    pub image_index: i32,
    pub source_uri: String,
}

/// A successful upload: store the public thumbnail URL and clear the mirror state.
///
/// Not slot-guarded (ADR-31: derived table, the mirror's latest upload wins) — but like
/// every upsert in this crate the `DO UPDATE SET` names EVERY column: a re-upload must
/// overwrite stale content, and an omitted column would silently keep the previous
/// URI's thumbnail.
pub async fn upsert_success<'e, E>(
    executor: E,
    asset_pubkey: &[u8],
    image_index: i32,
    source_uri: &str,
    thumb_uri: &str,
    uploaded_at: DateTime<Utc>,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO marketplace_property_image (
            asset_pubkey, image_index, source_uri, thumb_uri, uploaded_at,
            attempts, next_attempt_at, last_error
        )
        VALUES ($1, $2, $3, $4, $5, 0, NULL, NULL)
        ON CONFLICT (asset_pubkey, image_index) DO UPDATE SET
            source_uri      = EXCLUDED.source_uri,
            thumb_uri       = EXCLUDED.thumb_uri,
            uploaded_at     = EXCLUDED.uploaded_at,
            attempts        = 0,
            next_attempt_at = NULL,
            last_error      = NULL
        "#,
        asset_pubkey,
        image_index,
        source_uri,
        thumb_uri,
        uploaded_at,
    )
    .execute(executor)
    .await
}

/// A failed attempt: record the attempt's error and schedule the next one with
/// exponential backoff (30 s, doubling per consecutive failure of the SAME URI, 1 h
/// cap — the ADR-27 shape, computed in the upsert).
///
/// The row keeps its last successful snapshot: `thumb_uri` / `uploaded_at` are NOT
/// touched, so a failure never erases the last published thumbnail (the API keeps
/// serving it). The `CASE` resets `attempts` to 1 when the failed URI differs from the
/// stored one (a fresh failure chain), else increments it. The CASE appears twice
/// because `EXCLUDED` cannot see the column the upsert is computing; `LEAST(..., 20)`
/// keeps `power()` finite for a pathological counter.
pub async fn record_failure<'e, E>(
    executor: E,
    asset_pubkey: &[u8],
    image_index: i32,
    source_uri: &str,
    error: &str,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO marketplace_property_image (
            asset_pubkey, image_index, source_uri, attempts, next_attempt_at, last_error
        )
        VALUES ($1, $2, $3, 1, now() + interval '30 seconds', $4)
        ON CONFLICT (asset_pubkey, image_index) DO UPDATE SET
            source_uri      = EXCLUDED.source_uri,
            attempts        = CASE
                                  WHEN marketplace_property_image.source_uri = EXCLUDED.source_uri
                                  THEN marketplace_property_image.attempts + 1
                                  ELSE 1
                              END,
            next_attempt_at = now() + (
                LEAST(3600, (30 * power(2, LEAST(
                    CASE
                        WHEN marketplace_property_image.source_uri = EXCLUDED.source_uri
                        THEN marketplace_property_image.attempts + 1
                        ELSE 1
                    END, 20)))::bigint)
                * interval '1 second'),
            last_error      = EXCLUDED.last_error
        "#,
        asset_pubkey,
        image_index,
        source_uri,
        error,
    )
    .execute(executor)
    .await
}

/// The mirror's work set: every (asset, image_index) of an OPEN asset whose stored
/// thumbnail is missing, stale (the metadata's `property_images` entry changed), or a
/// failure whose backoff has elapsed. The gate mirrors `property_metadata::pending_assets`
/// entry-by-entry, with the per-URI `source_uri` comparison in place of the document's
/// `metadata_uri`. Ordered by (pubkey, ordinal) so a cycle's batch is deterministic;
/// bounded by `limit`.
pub async fn pending_images<'e, E>(
    executor: E,
    limit: i64,
) -> Result<Vec<PendingImage>, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let rows = sqlx::query!(
        r#"
        SELECT a.pubkey AS asset_pubkey,
               (el.ord - 1)::int AS image_index,
               el.value #>> '{}' AS source_uri
        FROM marketplace_property_asset a
        JOIN marketplace_property_metadata m ON m.pubkey = a.pubkey
        CROSS JOIN LATERAL jsonb_array_elements(m.property_images) WITH ORDINALITY
             AS el(value, ord)
        LEFT JOIN marketplace_property_image p
             ON p.asset_pubkey = a.pubkey AND p.image_index = (el.ord - 1)::int
        WHERE a.closed_at_slot IS NULL
          AND jsonb_typeof(el.value) = 'string'
          AND el.value #>> '{}' <> ''
          AND (
                p.asset_pubkey IS NULL
             OR p.source_uri <> (el.value #>> '{}')
             OR (p.last_error IS NOT NULL
                 AND (p.next_attempt_at IS NULL OR p.next_attempt_at <= now()))
          )
        ORDER BY a.pubkey ASC, el.ord ASC
        LIMIT $1
        "#,
        limit,
    )
    .fetch_all(executor)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            // The macro infers the LATERAL-derived columns as nullable (a `#>>` cast or a
            // `LEFT JOIN` can yield NULL in general); the query's own `WHERE` makes both
            // non-NULL on every returned row, so unwrap them here.
            PendingImage {
                asset_pubkey: r.asset_pubkey,
                image_index: r
                    .image_index
                    .expect("non-NULL: (el.ord - 1)::int is never NULL"),
                source_uri: r
                    .source_uri
                    .expect("non-NULL: the WHERE filters to non-empty string entries"),
            }
        })
        .collect())
}

/// How many images are awaiting an upload right now (the `property_images_pending`
/// gauge; ADR-31).
pub async fn count_pending<'e, E>(executor: E) -> Result<i64, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let count: Option<i64> = sqlx::query_scalar!(
        r#"
        SELECT count(*)
        FROM marketplace_property_asset a
        JOIN marketplace_property_metadata m ON m.pubkey = a.pubkey
        CROSS JOIN LATERAL jsonb_array_elements(m.property_images) WITH ORDINALITY
             AS el(value, ord)
        LEFT JOIN marketplace_property_image p
             ON p.asset_pubkey = a.pubkey AND p.image_index = (el.ord - 1)::int
        WHERE a.closed_at_slot IS NULL
          AND jsonb_typeof(el.value) = 'string'
          AND el.value #>> '{}' <> ''
          AND (
                p.asset_pubkey IS NULL
             OR p.source_uri <> (el.value #>> '{}')
             OR (p.last_error IS NOT NULL
                 AND (p.next_attempt_at IS NULL OR p.next_attempt_at <= now()))
          )
        "#
    )
    .fetch_one(executor)
    .await?;

    Ok(count.unwrap_or(0))
}
