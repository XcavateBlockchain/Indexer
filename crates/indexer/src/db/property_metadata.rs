//! Row shapes and upserts for the DERIVED `marketplace_property_metadata` table
//! (`migrations/0013_property_metadata.sql`, ADR-27). Same `query!`-macro contract as
//! [`super::marketplace`], but this table is NOT an account-state mirror: there is no
//! slot guard, no soft close, and no `StateTable` entry (see the migration header for the
//! full argument). The fetcher (`crate::metadata`) is the only writer.
//!
//! Shape notes: the document's nested objects are flattened into typed columns (0010's
//! convention); `propertyImages` / `otherDocuments` are JSONB URL lists in the shape this
//! indexer constructs; `user_pubkey` / `companyWalletAddress` are base58 wallet addresses as
//! BYTEA; `raw` is the whole document verbatim. All content columns are nullable -- the
//! fetcher's parsing is lenient per field, so a partial document still indexes its good
//! fields.

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::postgres::PgQueryResult;
use sqlx::PgExecutor;

/// One fetched-and-parsed metadata document, ready to upsert as the asset's current
/// snapshot. The fetch state columns (`attempts` / `next_attempt_at` / `last_error`) are
/// NOT here: a success always writes them to 0 / NULL / NULL, so they have no per-row
/// value.
#[derive(Debug, Clone)]
pub struct PropertyMetadataRow {
    /// The PropertyAsset PDA this document describes (the table's key, 1:1 with
    /// `marketplace_property_asset.pubkey`).
    pub pubkey: Vec<u8>,
    pub asset_id: i64,
    /// The URI this snapshot was fetched from.
    pub metadata_uri: String,
    /// When the snapshot was taken.
    pub fetched_at: DateTime<Utc>,
    /// The whole document verbatim (the typed columns are derived from it).
    pub raw: serde_json::Value,
    // Identity / description.
    pub property_id: Option<String>,
    pub property_name: Option<String>,
    pub property_type: Option<String>,
    pub status: Option<String>,
    pub tenure: Option<String>,
    pub property_description: Option<String>,
    pub planning_code: Option<String>,
    pub building_control_code: Option<String>,
    pub user_pubkey: Option<Vec<u8>>,
    pub company_id: Option<String>,
    pub company_name: Option<String>,
    pub company_logo: Option<String>,
    pub company_wallet_address: Option<Vec<u8>>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    // `address` object, flattened.
    pub address_street: Option<String>,
    pub address_town_city: Option<String>,
    pub address_flat_or_unit: Option<String>,
    pub address_post_code: Option<String>,
    pub address_local_authority: Option<String>,
    pub address_region: Option<String>,
    pub address_location: Option<String>,
    // `attributes` object, flattened.
    pub area: Option<String>,
    pub quality: Option<String>,
    pub outdoor_space: Option<String>,
    pub number_of_bedrooms: Option<i64>,
    pub number_of_bathrooms: Option<i64>,
    pub construction_date: Option<NaiveDate>,
    pub off_street_parking: Option<String>,
    // `finances` object, flattened.
    pub property_price: Option<i64>,
    pub number_of_shares: Option<i64>,
    pub share_price: Option<i64>,
    pub estimated_rental_income: Option<i64>,
    pub annual_service_charge: Option<i64>,
    pub stamp_duty_tax: Option<i64>,
    pub stamp_duty_paid: Option<bool>,
    pub annual_service_charge_paid: Option<bool>,
    // Documents / media.
    pub floor_plan: Option<String>,
    pub map_url: Option<String>,
    pub sales_agreement: Option<String>,
    /// JSONB array of URL strings.
    pub other_documents: Option<serde_json::Value>,
    /// JSONB array of URL strings.
    pub property_images: Option<serde_json::Value>,
}

/// One work-set entry: an open `PropertyAsset` whose metadata needs a (re)fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAsset {
    pub pubkey: Vec<u8>,
    pub asset_id: i64,
    pub metadata_uri: String,
}

/// A successful fetch: store the snapshot, clear the fetch state.
///
/// Not slot-guarded (ADR-27: derived table, the fetcher's latest fetch wins) -- but like
/// every upsert in this crate the `DO UPDATE SET` names EVERY column: a re-fetch must
/// overwrite stale content, and an omitted column would silently keep the previous
/// document's value.
pub async fn upsert_success<'e, E>(
    executor: E,
    row: &PropertyMetadataRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO marketplace_property_metadata (
            pubkey, asset_id, metadata_uri, fetched_at, raw,
            property_id, property_name, property_type, status, tenure, property_description,
            planning_code, building_control_code, user_pubkey, company_id, company_name,
            company_logo, company_wallet_address, created_at, updated_at,
            address_street, address_town_city, address_flat_or_unit, address_post_code,
            address_local_authority, address_region, address_location,
            area, quality, outdoor_space, number_of_bedrooms, number_of_bathrooms,
            construction_date, off_street_parking,
            property_price, number_of_shares, share_price, estimated_rental_income,
            annual_service_charge, stamp_duty_tax, stamp_duty_paid,
            annual_service_charge_paid,
            floor_plan, map_url, sales_agreement, other_documents, property_images
        )
        VALUES (
            $1, $2, $3, $4, $5,
            $6, $7, $8, $9, $10, $11,
            $12, $13, $14, $15, $16, $17,
            $18, $19, $20, $21,
            $22, $23, $24, $25,
            $26, $27, $28,
            $29, $30, $31, $32, $33, $34,
            $35, $36, $37, $38,
            $39, $40, $41, $42,
            $43, $44, $45, $46, $47
        )
        ON CONFLICT (pubkey) DO UPDATE SET
            asset_id                     = EXCLUDED.asset_id,
            metadata_uri                 = EXCLUDED.metadata_uri,
            fetched_at                   = EXCLUDED.fetched_at,
            raw                          = EXCLUDED.raw,
            property_id                  = EXCLUDED.property_id,
            property_name                = EXCLUDED.property_name,
            property_type                = EXCLUDED.property_type,
            status                       = EXCLUDED.status,
            tenure                       = EXCLUDED.tenure,
            property_description         = EXCLUDED.property_description,
            planning_code                = EXCLUDED.planning_code,
            building_control_code        = EXCLUDED.building_control_code,
            user_pubkey                  = EXCLUDED.user_pubkey,
            company_id                   = EXCLUDED.company_id,
            company_name                 = EXCLUDED.company_name,
            company_logo                 = EXCLUDED.company_logo,
            company_wallet_address       = EXCLUDED.company_wallet_address,
            created_at                   = EXCLUDED.created_at,
            updated_at                   = EXCLUDED.updated_at,
            address_street               = EXCLUDED.address_street,
            address_town_city            = EXCLUDED.address_town_city,
            address_flat_or_unit         = EXCLUDED.address_flat_or_unit,
            address_post_code            = EXCLUDED.address_post_code,
            address_local_authority      = EXCLUDED.address_local_authority,
            address_region               = EXCLUDED.address_region,
            address_location             = EXCLUDED.address_location,
            area                         = EXCLUDED.area,
            quality                      = EXCLUDED.quality,
            outdoor_space                = EXCLUDED.outdoor_space,
            number_of_bedrooms           = EXCLUDED.number_of_bedrooms,
            number_of_bathrooms          = EXCLUDED.number_of_bathrooms,
            construction_date            = EXCLUDED.construction_date,
            off_street_parking           = EXCLUDED.off_street_parking,
            property_price               = EXCLUDED.property_price,
            number_of_shares             = EXCLUDED.number_of_shares,
            share_price                  = EXCLUDED.share_price,
            estimated_rental_income      = EXCLUDED.estimated_rental_income,
            annual_service_charge        = EXCLUDED.annual_service_charge,
            stamp_duty_tax               = EXCLUDED.stamp_duty_tax,
            stamp_duty_paid              = EXCLUDED.stamp_duty_paid,
            annual_service_charge_paid   = EXCLUDED.annual_service_charge_paid,
            floor_plan                   = EXCLUDED.floor_plan,
            map_url                      = EXCLUDED.map_url,
            sales_agreement              = EXCLUDED.sales_agreement,
            other_documents              = EXCLUDED.other_documents,
            property_images              = EXCLUDED.property_images,
            attempts                     = 0,
            next_attempt_at              = NULL,
            last_error                   = NULL
        "#,
        row.pubkey,
        row.asset_id,
        row.metadata_uri,
        // Nullable fetch-state columns (0013): a success always has both, so the values
        // borrow in directly.
        row.fetched_at,
        &row.raw,
        // Nullable content columns: bind a BY-VALUE `Option` (the house idiom, cf.
        // `upsert_config`'s `as_deref()` binds): for INSERT parameters the macro checks
        // the value's type against the parameter OID and never sees the column's
        // nullability, so a `&Option<_>` reference would be rejected.
        row.property_id.as_deref(),
        row.property_name.as_deref(),
        row.property_type.as_deref(),
        row.status.as_deref(),
        row.tenure.as_deref(),
        row.property_description.as_deref(),
        row.planning_code.as_deref(),
        row.building_control_code.as_deref(),
        row.user_pubkey.as_deref(),
        row.company_id.as_deref(),
        row.company_name.as_deref(),
        row.company_logo.as_deref(),
        row.company_wallet_address.as_deref(),
        row.created_at,
        row.updated_at,
        row.address_street.as_deref(),
        row.address_town_city.as_deref(),
        row.address_flat_or_unit.as_deref(),
        row.address_post_code.as_deref(),
        row.address_local_authority.as_deref(),
        row.address_region.as_deref(),
        row.address_location.as_deref(),
        row.area.as_deref(),
        row.quality.as_deref(),
        row.outdoor_space.as_deref(),
        row.number_of_bedrooms,
        row.number_of_bathrooms,
        row.construction_date,
        row.off_street_parking.as_deref(),
        row.property_price,
        row.number_of_shares,
        row.share_price,
        row.estimated_rental_income,
        row.annual_service_charge,
        row.stamp_duty_tax,
        row.stamp_duty_paid,
        row.annual_service_charge_paid,
        row.floor_plan.as_deref(),
        row.map_url.as_deref(),
        row.sales_agreement.as_deref(),
        row.other_documents.clone(),
        row.property_images.clone(),
    )
    .execute(executor)
    .await
}

/// A failed fetch: record the attempt's error and schedule the next one with exponential
/// backoff (30 s, doubling per consecutive failure of the SAME uri, 1 h cap -- ADR-27).
///
/// The row (if it exists) keeps its last successful snapshot: a failure of a new URI does
/// not erase the previous document, and `metadata_uri` + `last_error` tell the reader
/// which URI the state describes. The `CASE` resets `attempts` to 1 when the failed URI
/// differs from the stored one (a fresh failure chain), else increments it. The CASE
/// appears twice because `EXCLUDED` cannot see the column the upsert is computing;
/// `LEAST(..., 20)` keeps `power()` finite for a pathological counter.
pub async fn record_failure<'e, E>(
    executor: E,
    pubkey: &[u8],
    asset_id: i64,
    metadata_uri: &str,
    error: &str,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO marketplace_property_metadata (
            pubkey, asset_id, metadata_uri, attempts, next_attempt_at, last_error
        )
        VALUES ($1, $2, $3, 1, now() + interval '30 seconds', $4)
        ON CONFLICT (pubkey) DO UPDATE SET
            asset_id        = EXCLUDED.asset_id,
            metadata_uri    = EXCLUDED.metadata_uri,
            attempts        = CASE
                                  WHEN marketplace_property_metadata.metadata_uri = EXCLUDED.metadata_uri
                                  THEN marketplace_property_metadata.attempts + 1
                                  ELSE 1
                              END,
            next_attempt_at = now() + (
                LEAST(3600, (30 * power(2, LEAST(
                    CASE
                        WHEN marketplace_property_metadata.metadata_uri = EXCLUDED.metadata_uri
                        THEN marketplace_property_metadata.attempts + 1
                        ELSE 1
                    END, 20)))::bigint)
                * interval '1 second'),
            last_error      = EXCLUDED.last_error
        "#,
        pubkey,
        asset_id,
        metadata_uri,
        error,
    )
    .execute(executor)
    .await
}

/// The work set: open `PropertyAsset`s with a non-empty `metadata_uri` whose stored
/// metadata is missing, stale (the on-chain URI changed), or a failure whose backoff has
/// elapsed. Ordered by pubkey so a cycle's batch is deterministic; bounded by `limit`.
pub async fn pending_assets<'e, E>(
    executor: E,
    limit: i64,
) -> Result<Vec<PendingAsset>, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let rows = sqlx::query!(
        r#"
        SELECT a.pubkey, a.asset_id, a.metadata_uri
        FROM marketplace_property_asset a
        LEFT JOIN marketplace_property_metadata m ON m.pubkey = a.pubkey
        WHERE a.closed_at_slot IS NULL
          AND a.metadata_uri <> ''
          AND (
                m.pubkey IS NULL
             OR m.metadata_uri <> a.metadata_uri
             OR (m.last_error IS NOT NULL
                 AND (m.next_attempt_at IS NULL OR m.next_attempt_at <= now()))
          )
        ORDER BY a.pubkey ASC
        LIMIT $1
        "#,
        limit,
    )
    .fetch_all(executor)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| PendingAsset {
            pubkey: r.pubkey,
            asset_id: r.asset_id,
            metadata_uri: r.metadata_uri,
        })
        .collect())
}

/// How many assets are awaiting a metadata fetch right now (the
/// `property_metadata_pending` gauge; ADR-27).
pub async fn count_pending<'e, E>(executor: E) -> Result<i64, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let count: Option<i64> = sqlx::query_scalar!(
        r#"
        SELECT count(*)
        FROM marketplace_property_asset a
        LEFT JOIN marketplace_property_metadata m ON m.pubkey = a.pubkey
        WHERE a.closed_at_slot IS NULL
          AND a.metadata_uri <> ''
          AND (
                m.pubkey IS NULL
             OR m.metadata_uri <> a.metadata_uri
             OR (m.last_error IS NOT NULL
                 AND (m.next_attempt_at IS NULL OR m.next_attempt_at <= now()))
          )
        "#
    )
    .fetch_one(executor)
    .await?;

    Ok(count.unwrap_or(0))
}
