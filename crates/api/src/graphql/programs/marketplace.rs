//! The `marketplace` program's read surface: entity types over
//! `migrations/0009_marketplace_state.sql` (as reshaped by `0012_redeploy_new_programs.sql`)
//! and the resolver bodies `QueryRoot` delegates to. See [`super`] for the shared
//! conventions.

use std::collections::HashMap;

use carbon_core::graphql::primitives::I64;
use chrono::{DateTime, Utc};
use juniper::{FieldError, FieldResult, GraphQLObject, Value, ID};
use sqlx::PgPool;

use super::{b58, hex_string, json_string, parse_b58, total_count_i32, utf8_lossy};
use crate::graphql::context::GraphQLContext;
use crate::graphql::enums::{unknown_enum_value, DocumentStatus, ListingStatus};
use crate::guards::{clamp_first, clamp_offset};

/// The marketplace program's Config PDA (singleton). `null` until `initialize_config` has been
/// indexed. `accepted_payment_mints` is the raw JSON of the accepted mint list
/// (`["<base58>", ...]` -- see migration 0009).
#[derive(GraphQLObject, Clone, Debug)]
pub struct MarketplaceConfig {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub authority: String,
    pub pending_authority: Option<String>,
    pub xcav_mint: String,
    pub treasury: String,
    pub rent_collector: String,
    pub accepted_payment_mints: String,
    pub listing_deposit: I64,
    pub lawyer_deposit: I64,
    pub min_property_shares: I64,
    pub max_property_shares: I64,
    pub marketplace_fee_bps: i32,
    pub investor_fee_bps: i32,
    pub max_ownership_bps: i32,
    pub claiming_time: I64,
    pub legal_process_time: I64,
    pub lawyer_voting_time: I64,
    pub min_voting_quorum_bps: i32,
    pub next_listing_id: I64,
    pub next_share_listing_id: I64,
}

/// One investor's position in one listing: bought/reserved share counts and the funds, tax and
/// fee amounts paid or reserved for them. Closed and routinely re-created at the same address
/// by the next buy/reserve.
#[derive(GraphQLObject, Clone, Debug)]
pub struct InvestorPosition {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub listing_id: I64,
    pub investor: String,
    pub payment_mint: String,
    pub payment_account: String,
    pub share_amount: I64,
    pub reserved_share_amount: I64,
    pub paid_funds: I64,
    pub paid_tax: I64,
    pub paid_fee: I64,
    pub reserved_funds: I64,
    pub reserved_tax: I64,
    pub reserved_fee: I64,
    pub cancelled: bool,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct InvestorPositionConnection {
    pub nodes: Vec<InvestorPosition>,
    pub total_count: i32,
}

/// A lawyer registered with the marketplace (the registry PDA keyed by the lawyer's wallet).
#[derive(GraphQLObject, Clone, Debug)]
pub struct Lawyer {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub lawyer: String,
    pub region_id: i32,
    pub deposit: I64,
    pub active_cases: I64,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct LawyerConnection {
    pub nodes: Vec<Lawyer>,
    pub total_count: i32,
}

/// A lawyer's candidacy for one listing's SPV-lawyer election round.
#[derive(GraphQLObject, Clone, Debug)]
pub struct LawyerCandidacy {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub listing_id: I64,
    pub round: I64,
    pub lawyer: String,
    pub costs: I64,
    pub vote_power: I64,
    pub rent_payer: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct LawyerCandidacyConnection {
    pub nodes: Vec<LawyerCandidacy>,
    pub total_count: i32,
}

/// One voter's vote in a listing's SPV-lawyer election round.
#[derive(GraphQLObject, Clone, Debug)]
pub struct LawyerVote {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub listing_id: I64,
    pub round: I64,
    pub voter: String,
    pub choice: String,
    pub power: I64,
    pub rent_payer: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct LawyerVoteConnection {
    pub nodes: Vec<LawyerVote>,
    pub total_count: i32,
}

/// A property listing: sale terms, progress counters, the two flattened `LawyerAssignment`s,
/// the flattened `SpvElection` and per-mint collected totals. `collected` is the raw JSON
/// `[{"mint": "<base58>", "funds": N, "fee": N, "tax": N}, ...]` (see migration 0009).
/// `propertyAsset` is the listing's tokenised property, LEFT-JOINed in the resolver
/// (`pa.asset_id = l.asset_id`); the join is 1:1 -- one row per PDA on each side and the
/// PropertyAsset PDA is seeded from the same id as the listing -- so it cannot duplicate
/// nodes. `null` if the asset's state row is absent (defensive: `list_property` writes
/// both accounts in one instruction, so this should not occur). The nested asset's
/// `metadata` is the fetched document attached by the same keyed lookup as the
/// `propertyAssets` connection (ADR-33) — `null` only while the enricher has no row for
/// the asset's PDA, exactly as on that path.
#[derive(GraphQLObject, Clone, Debug)]
pub struct Listing {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub listing_id: I64,
    pub developer: String,
    pub asset_id: I64,
    pub share_price: I64,
    pub listed_share_amount: I64,
    pub sold_share_amount: I64,
    pub reserved_share_amount: I64,
    pub tax_paid_by_developer: bool,
    pub tax_bps: i32,
    pub marketplace_fee_bps: i32,
    pub investor_fee_bps: i32,
    pub max_ownership_bps: i32,
    pub listing_expiry: I64,
    pub claiming_time: I64,
    pub claim_deadline: I64,
    pub legal_process_time: I64,
    pub lawyer_voting_time: I64,
    pub min_voting_quorum_bps: i32,
    pub position_count: I64,
    pub legal_deadline: I64,
    pub deposit: I64,
    pub developer_lawyer: String,
    pub developer_lawyer_costs: I64,
    pub developer_lawyer_doc_status: DocumentStatus,
    pub developer_lawyer_documents_hash: String,
    pub spv_lawyer: String,
    pub spv_lawyer_costs: I64,
    pub spv_lawyer_doc_status: DocumentStatus,
    pub spv_lawyer_documents_hash: String,
    pub second_attempt: bool,
    pub developer_engaged: bool,
    pub spv_costs_due: I64,
    pub spv_costs_payee: String,
    pub collected_fee_quote: I64,
    pub collected: String,
    pub spv_election_expiry: I64,
    pub spv_election_candidate_count: I64,
    pub spv_election_round: I64,
    pub status: ListingStatus,
    /// The tokenised property this listing sells shares of (1:1, same `asset_id`).
    pub property_asset: Option<PropertyAsset>,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct ListingConnection {
    pub nodes: Vec<Listing>,
    pub total_count: i32,
}

/// The tokenised property behind one listing (`asset_id == listing_id` in current source).
/// `name` and `metadata_uri` are empty until `init_property_assets` attaches them.
/// Reachable two ways: standalone via the `propertyAssets` connection, or nested as
/// `Listing.propertyAsset` (the `listings` resolver LEFT-JOINs this table, so one query
/// returns both). `metadata` is the fetched-and-decomposed off-chain document
/// `metadata_uri` points at (ADR-27), attached so asset and document answer in ONE
/// query. BOTH resolvers attach it the same way — one keyed `ANY(...)` lookup over the
/// derived 1:1 table for the page's PDA pubkeys (ADR-29, extended to the `listings`
/// path by ADR-33; the derived table stays outside the mirror join by design, ADR-27) —
/// so `null` means exactly that the enricher has no row for this PDA (fetch pending or
/// still failing), never a missing join.
#[derive(GraphQLObject, Clone, Debug)]
pub struct PropertyAsset {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub asset_id: I64,
    pub name: String,
    pub metadata_uri: String,
    /// The fetched-and-decomposed metadata document (ADR-27); `null` until one exists.
    pub metadata: Option<PropertyMetadata>,
    pub share_mint: String,
    pub region_id: i32,
    pub location: String,
    pub share_amount: I64,
    pub spv_created: bool,
    pub finalized: bool,
    pub holder_count: I64,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct PropertyAssetConnection {
    pub nodes: Vec<PropertyAsset>,
    pub total_count: i32,
}

/// A reservation total keyed by the investor's payment token account (shared across listings).
/// Closed at zero and re-created by the next reserve.
#[derive(GraphQLObject, Clone, Debug)]
pub struct Reservation {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub token_account: String,
    pub amount: I64,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct ReservationConnection {
    pub nodes: Vec<Reservation>,
    pub total_count: i32,
}

/// One owner's share holding in one property asset. The four `lock*` counters mirror the
/// on-chain per-`LockReason` array (the effective lock is the largest of them); `listed` is
/// what sits in open secondary listings, still counted in `amount`.
#[derive(GraphQLObject, Clone, Debug)]
pub struct ShareHolding {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub asset_id: I64,
    pub owner: String,
    pub amount: I64,
    pub lock_lawyer_election: I64,
    pub lock_agent_election: I64,
    pub lock_proposal: I64,
    pub lock_challenge: I64,
    pub listed: I64,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct ShareHoldingConnection {
    pub nodes: Vec<ShareHolding>,
    pub total_count: i32,
}

/// A holder's open secondary-market listing of part of their shares.
#[derive(GraphQLObject, Clone, Debug)]
pub struct ShareListing {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub share_listing_id: I64,
    pub asset_id: I64,
    pub seller: String,
    pub share_price: I64,
    pub amount: I64,
    pub fee_bps: i32,
    pub next_offer_nonce: I64,
    pub rent_payer: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct ShareListingConnection {
    pub nodes: Vec<ShareListing>,
    pub total_count: i32,
}

/// A bid on one share listing, one per bidder per listing; the bid money sits in the
/// offer's own vault until accept/reject/cancel. `listingId` is the ShareListing id.
#[derive(GraphQLObject, Clone, Debug)]
pub struct Offer {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub listing_id: I64,
    pub asset_id: I64,
    pub offeror: String,
    pub share_price: I64,
    pub amount: I64,
    pub payment_mint: String,
    pub held: I64,
    pub nonce: I64,
    pub rent_payer: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct OfferConnection {
    pub nodes: Vec<Offer>,
    pub total_count: i32,
}

// ============================================================================================
// Off-chain property metadata (migrations/0013, ADR-27). A DERIVED table, not an account
// mirror: the background enricher downloads the JSON document each `PropertyAsset`'s
// `metadata_uri` points at and decomposes it here. One row per asset PDA; `fetchedAt` is the
// snapshot's provenance and the document is re-fetched only when the on-chain URI changes or
// a fetch fails -- so `fetchedAt`/`raw`/the typed fields are the LAST SUCCESSFUL snapshot,
// while `metadataUri`/`lastError`/`attempts` describe the LAST ATTEMPT (a row whose fetch
// never succeeded has `fetchedAt = null` and all content fields `null`).
//
// Surfaced two ways: the root `propertyMetadata` connection (document-first access, filter
// `assetId`) and NESTED per asset as `PropertyAsset.metadata`, so asset + document answer
// in a single GraphQL query — `property_assets` attaches the page's rows with one extra
// keyed lookup (`pubkey = ANY(...)` over the derived 1:1).
// ============================================================================================

/// The document's `address` object.
#[derive(GraphQLObject, Clone, Debug)]
pub struct PropertyMetadataAddress {
    pub street: Option<String>,
    pub town_city: Option<String>,
    pub flat_or_unit: Option<String>,
    pub post_code: Option<String>,
    pub local_authority: Option<String>,
    pub region: Option<String>,
    pub location: Option<String>,
}

/// The document's `attributes` object.
#[derive(GraphQLObject, Clone, Debug)]
pub struct PropertyMetadataAttributes {
    pub area: Option<String>,
    pub quality: Option<String>,
    pub outdoor_space: Option<String>,
    pub number_of_bedrooms: Option<I64>,
    pub number_of_bathrooms: Option<I64>,
    /// The document's `YYYY-MM-DD` date, as stored.
    pub construction_date: Option<String>,
    pub off_street_parking: Option<String>,
}

/// The document's `finances` object.
#[derive(GraphQLObject, Clone, Debug)]
pub struct PropertyMetadataFinances {
    pub property_price: Option<I64>,
    pub number_of_shares: Option<I64>,
    pub share_price: Option<I64>,
    pub estimated_rental_income: Option<I64>,
    pub annual_service_charge: Option<I64>,
    pub stamp_duty_tax: Option<I64>,
    pub is_stamp_duty_paid: Option<bool>,
    pub is_annual_service_charge_paid: Option<bool>,
}

/// One fetched-and-decomposed metadata document. Wallets are base58; `raw` /
/// `otherDocuments` / `propertyImages` are raw JSON (juniper has no JSON scalar); the nested
/// objects are present only when the document had at least one field in them.
#[derive(GraphQLObject, Clone, Debug)]
pub struct PropertyMetadata {
    pub id: ID,
    pub asset_id: I64,
    /// The URI the last attempt targeted.
    pub metadata_uri: String,
    /// When the last successful snapshot was fetched; `null` until one exists.
    pub fetched_at: Option<DateTime<Utc>>,
    /// Consecutive failures for `metadata_uri` (reset to 0 by a success).
    pub attempts: i32,
    /// The last failure's message; `null` when the row's state is a success.
    pub last_error: Option<String>,
    /// The whole document verbatim -- the ground truth the typed fields are derived from;
    /// `null` until the first successful fetch.
    pub raw: Option<String>,
    // Identity / description (top-level document fields).
    pub property_id: Option<String>,
    pub property_name: Option<String>,
    pub property_type: Option<String>,
    pub status: Option<String>,
    pub tenure: Option<String>,
    pub property_description: Option<String>,
    pub planning_code: Option<String>,
    pub building_control_code: Option<String>,
    pub user: Option<String>,
    pub company_id: Option<String>,
    pub company_name: Option<String>,
    pub company_logo: Option<String>,
    pub company_wallet_address: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    // The document's nested objects.
    pub address: Option<PropertyMetadataAddress>,
    pub attributes: Option<PropertyMetadataAttributes>,
    pub finances: Option<PropertyMetadataFinances>,
    // Documents / media (top-level).
    pub floor_plan: Option<String>,
    pub map_url: Option<String>,
    pub sales_agreement: Option<String>,
    /// Raw JSON array of URL strings.
    pub other_documents: Option<String>,
    /// Raw JSON array of URL strings.
    pub property_images: Option<String>,
    /// Public URLs of the mirrored 720x720 JPEG thumbnails of `property_images` (ADR-31), in
    /// `image_index` order; `null` until the image mirror has uploaded at least one, and
    /// possibly a partial list during the first upload pass (entries appear as the mirror
    /// uploads them; indices beyond the current `propertyImages` array are never listed).
    pub property_image_thumbnails: Option<Vec<String>>,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct PropertyMetadataConnection {
    pub nodes: Vec<PropertyMetadata>,
    pub total_count: i32,
}

// ============================================================================================
// Row -> `PropertyMetadata` decomposition, SHARED by both metadata queries (the root
// `property_metadata` connection and the per-page `ANY(...)` lookup in `property_assets`).
// Both SELECT the identical 50-column projection of `marketplace_property_metadata`, but
// `query!` binds each query SITE to its own private `Record` row type that implements nothing
// shareable (no `sqlx::Row`, no `FromRow`), so a plain function cannot name the row and a
// `try_get`-by-name helper cannot typecheck. `macro_rules!` expands the decomposition
// textually against whichever row it is handed: each site keeps the `query!` macro's full
// per-field compile-time binding (a renamed or mistyped column still fails `prepare --check`
// per site), and the mapping itself lives in exactly one place.
// The body MOVES the row's fields, so a caller that needs `r.pubkey` afterwards (the
// `property_assets` map key) captures it first.
// ============================================================================================
macro_rules! property_metadata_from_row {
    ($row:expr) => {{
        let row = $row;

        // A nested object surfaces only when the document had at least one field in it
        // (all-null content means the document had none). The fields mix types
        // (String / i64 / NaiveDate / bool), so each check is a plain `||` chain
        // rather than an array of references.
        let has_address = row.address_street.is_some()
            || row.address_town_city.is_some()
            || row.address_flat_or_unit.is_some()
            || row.address_post_code.is_some()
            || row.address_local_authority.is_some()
            || row.address_region.is_some()
            || row.address_location.is_some();
        let has_attributes = row.area.is_some()
            || row.quality.is_some()
            || row.outdoor_space.is_some()
            || row.number_of_bedrooms.is_some()
            || row.number_of_bathrooms.is_some()
            || row.construction_date.is_some()
            || row.off_street_parking.is_some();
        let has_finances = row.property_price.is_some()
            || row.number_of_shares.is_some()
            || row.share_price.is_some()
            || row.estimated_rental_income.is_some()
            || row.annual_service_charge.is_some()
            || row.stamp_duty_tax.is_some()
            || row.stamp_duty_paid.is_some()
            || row.annual_service_charge_paid.is_some();

        PropertyMetadata {
            id: ID::new(b58(&row.pubkey)),
            asset_id: I64(row.asset_id),
            metadata_uri: row.metadata_uri,
            fetched_at: row.fetched_at,
            attempts: row.attempts,
            last_error: row.last_error,
            raw: row.raw.as_ref().map(json_string),
            // Identity / description (top-level document fields).
            property_id: row.property_id,
            property_name: row.property_name,
            property_type: row.property_type,
            status: row.status,
            tenure: row.tenure,
            property_description: row.property_description,
            planning_code: row.planning_code,
            building_control_code: row.building_control_code,
            user: row.user_pubkey.as_deref().map(b58),
            company_id: row.company_id,
            company_name: row.company_name,
            company_logo: row.company_logo,
            company_wallet_address: row.company_wallet_address.as_deref().map(b58),
            created_at: row.created_at,
            updated_at: row.updated_at,
            address: has_address.then_some(PropertyMetadataAddress {
                street: row.address_street,
                town_city: row.address_town_city,
                flat_or_unit: row.address_flat_or_unit,
                post_code: row.address_post_code,
                local_authority: row.address_local_authority,
                region: row.address_region,
                location: row.address_location,
            }),
            attributes: has_attributes.then_some(PropertyMetadataAttributes {
                area: row.area,
                quality: row.quality,
                outdoor_space: row.outdoor_space,
                number_of_bedrooms: row.number_of_bedrooms.map(I64),
                number_of_bathrooms: row.number_of_bathrooms.map(I64),
                construction_date: row.construction_date.map(|d| d.to_string()),
                off_street_parking: row.off_street_parking,
            }),
            finances: has_finances.then_some(PropertyMetadataFinances {
                property_price: row.property_price.map(I64),
                number_of_shares: row.number_of_shares.map(I64),
                share_price: row.share_price.map(I64),
                estimated_rental_income: row.estimated_rental_income.map(I64),
                annual_service_charge: row.annual_service_charge.map(I64),
                stamp_duty_tax: row.stamp_duty_tax.map(I64),
                is_stamp_duty_paid: row.stamp_duty_paid,
                is_annual_service_charge_paid: row.annual_service_charge_paid,
            }),
            floor_plan: row.floor_plan,
            map_url: row.map_url,
            sales_agreement: row.sales_agreement,
            other_documents: row.other_documents.as_ref().map(json_string),
            property_images: row.property_images.as_ref().map(json_string),
            // The thumbnails live in `marketplace_property_image` (ADR-31), a different
            // table than this projection: the callers attach them after the decomposition
            // (`with_thumbnails`), which keeps the 50-column projection shared by both
            // metadata queries untouched.
            property_image_thumbnails: None,
        }
    }};
}

/// The page's mirrored thumbnails (ADR-31) keyed by the asset PDA's pubkey: one `ANY(...)`
/// lookup over the derived table, `(image_index, thumb_uri)` pairs in `image_index` order
/// (the partial index `idx_mkt_property_image_thumb` covers exactly this). Skipped on an
/// empty page.
async fn image_thumbnails(
    pool: &PgPool,
    pubkeys: &[Vec<u8>],
) -> FieldResult<HashMap<Vec<u8>, Vec<(i32, String)>>> {
    let mut out: HashMap<Vec<u8>, Vec<(i32, String)>> = HashMap::new();
    if pubkeys.is_empty() {
        return Ok(out);
    }
    let rows = sqlx::query!(
        r#"
        SELECT asset_pubkey, image_index, thumb_uri
        FROM marketplace_property_image
        WHERE asset_pubkey = ANY($1) AND thumb_uri IS NOT NULL
        ORDER BY image_index
        "#,
        pubkeys,
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        // The column is nullable, so sqlx hands us `Option` even though the query filters
        // `IS NOT NULL`; skip the impossible `None` rather than panic on it.
        let Some(uri) = r.thumb_uri else {
            continue;
        };
        out.entry(r.asset_pubkey)
            .or_default()
            .push((r.image_index, uri));
    }
    Ok(out)
}

/// Attach the page's thumbnails to a built `PropertyMetadata` (ADR-31): `null` until the
/// mirror has uploaded one, and only the current array's indices (0016's rows outlive a
/// shrunken `propertyImages` array; the stale ones are dropped here).
fn with_thumbnails(
    mut metadata: PropertyMetadata,
    thumbs: Option<Vec<(i32, String)>>,
) -> PropertyMetadata {
    metadata.property_image_thumbnails = match (thumbs, current_image_count(&metadata)) {
        (Some(thumbs), Some(len)) => Some(
            thumbs
                .into_iter()
                .filter(|(i, _)| usize::try_from(*i).is_ok_and(|i| i < len))
                .map(|(_, uri)| uri)
                .collect(),
        ),
        _ => None,
    };
    metadata
}

/// The number of entries in the asset's current `propertyImages` array (the stale-thumbnail
/// filter, 0016); `None` when the document carries no images (or the JSON is unparseable).
fn current_image_count(metadata: &PropertyMetadata) -> Option<usize> {
    metadata.property_images.as_deref().and_then(|s| {
        serde_json::from_str::<Vec<serde_json::Value>>(s)
            .ok()
            .map(|v| v.len())
    })
}

// --- resolver bodies ------------------------------------------------------------------------

pub async fn marketplace_config(
    context: &GraphQLContext,
) -> FieldResult<Option<MarketplaceConfig>> {
    let row = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, authority, pending_authority, xcav_mint,
               treasury, rent_collector, accepted_payment_mints, listing_deposit, lawyer_deposit,
               min_property_shares, max_property_shares, marketplace_fee_bps, investor_fee_bps,
               max_ownership_bps, claiming_time, legal_process_time, lawyer_voting_time,
               min_voting_quorum_bps, next_listing_id, next_share_listing_id
        FROM marketplace_config
        ORDER BY slot DESC
        LIMIT 1
        "#
    )
    .fetch_optional(&context.pool)
    .await?;
    Ok(row.map(|r| MarketplaceConfig {
        id: ID::new(b58(&r.pubkey)),
        slot: I64(r.slot),
        lamports: I64(r.lamports),
        active: r.closed_at_slot.is_none(),
        closed_at_slot: r.closed_at_slot.map(I64),
        authority: b58(&r.authority),
        pending_authority: r.pending_authority.as_deref().map(b58),
        xcav_mint: b58(&r.xcav_mint),
        treasury: b58(&r.treasury),
        rent_collector: b58(&r.rent_collector),
        accepted_payment_mints: json_string(&r.accepted_payment_mints),
        listing_deposit: I64(r.listing_deposit),
        lawyer_deposit: I64(r.lawyer_deposit),
        min_property_shares: I64(r.min_property_shares),
        max_property_shares: I64(r.max_property_shares),
        marketplace_fee_bps: r.marketplace_fee_bps,
        investor_fee_bps: r.investor_fee_bps,
        max_ownership_bps: r.max_ownership_bps,
        claiming_time: I64(r.claiming_time),
        legal_process_time: I64(r.legal_process_time),
        lawyer_voting_time: I64(r.lawyer_voting_time),
        min_voting_quorum_bps: r.min_voting_quorum_bps,
        next_listing_id: I64(r.next_listing_id),
        next_share_listing_id: I64(r.next_share_listing_id),
    }))
}

pub async fn investor_positions(
    context: &GraphQLContext,
    listing_id: Option<I64>,
    investor: Option<String>,
    cancelled: Option<bool>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<InvestorPositionConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let listing_id = listing_id.map(|v| v.0);
    let investor = investor
        .as_deref()
        .map(|s| parse_b58("investor", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, listing_id, investor, payment_mint,
               payment_account, share_amount, reserved_share_amount, paid_funds, paid_tax,
               paid_fee, reserved_funds, reserved_tax, reserved_fee, cancelled
        FROM marketplace_investor_position
        WHERE ($1::bigint IS NULL OR listing_id = $1)
          AND ($2::bytea IS NULL OR investor = $2)
          AND ($3::bool IS NULL OR cancelled = $3)
          AND ($4::bool IS NULL OR (closed_at_slot IS NULL) = $4)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $5 OFFSET $6
        "#,
        listing_id,
        investor.as_deref(),
        cancelled,
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_investor_position
        WHERE ($1::bigint IS NULL OR listing_id = $1)
          AND ($2::bytea IS NULL OR investor = $2)
          AND ($3::bool IS NULL OR cancelled = $3)
          AND ($4::bool IS NULL OR (closed_at_slot IS NULL) = $4)
        "#,
        listing_id,
        investor.as_deref(),
        cancelled,
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(InvestorPositionConnection {
        nodes: rows
            .into_iter()
            .map(|r| InvestorPosition {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                listing_id: I64(r.listing_id),
                investor: b58(&r.investor),
                payment_mint: b58(&r.payment_mint),
                payment_account: b58(&r.payment_account),
                share_amount: I64(r.share_amount),
                reserved_share_amount: I64(r.reserved_share_amount),
                paid_funds: I64(r.paid_funds),
                paid_tax: I64(r.paid_tax),
                paid_fee: I64(r.paid_fee),
                reserved_funds: I64(r.reserved_funds),
                reserved_tax: I64(r.reserved_tax),
                reserved_fee: I64(r.reserved_fee),
                cancelled: r.cancelled,
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn lawyers(
    context: &GraphQLContext,
    lawyer: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<LawyerConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let lawyer = lawyer
        .as_deref()
        .map(|s| parse_b58("lawyer", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, lawyer, region_id, deposit, active_cases
        FROM marketplace_lawyer
        WHERE ($1::bytea IS NULL OR lawyer = $1)
          AND ($2::bool IS NULL OR (closed_at_slot IS NULL) = $2)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $3 OFFSET $4
        "#,
        lawyer.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_lawyer
        WHERE ($1::bytea IS NULL OR lawyer = $1)
          AND ($2::bool IS NULL OR (closed_at_slot IS NULL) = $2)
        "#,
        lawyer.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(LawyerConnection {
        nodes: rows
            .into_iter()
            .map(|r| Lawyer {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                lawyer: b58(&r.lawyer),
                region_id: r.region_id,
                deposit: I64(r.deposit),
                active_cases: I64(r.active_cases),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn lawyer_candidacies(
    context: &GraphQLContext,
    listing_id: Option<I64>,
    lawyer: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<LawyerCandidacyConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let listing_id = listing_id.map(|v| v.0);
    let lawyer = lawyer
        .as_deref()
        .map(|s| parse_b58("lawyer", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, listing_id, round, lawyer, costs,
               vote_power, rent_payer
        FROM marketplace_lawyer_candidacy
        WHERE ($1::bigint IS NULL OR listing_id = $1)
          AND ($2::bytea IS NULL OR lawyer = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        listing_id,
        lawyer.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_lawyer_candidacy
        WHERE ($1::bigint IS NULL OR listing_id = $1)
          AND ($2::bytea IS NULL OR lawyer = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        listing_id,
        lawyer.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(LawyerCandidacyConnection {
        nodes: rows
            .into_iter()
            .map(|r| LawyerCandidacy {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                listing_id: I64(r.listing_id),
                round: I64(r.round),
                lawyer: b58(&r.lawyer),
                costs: I64(r.costs),
                vote_power: I64(r.vote_power),
                rent_payer: b58(&r.rent_payer),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn lawyer_votes(
    context: &GraphQLContext,
    listing_id: Option<I64>,
    voter: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<LawyerVoteConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let listing_id = listing_id.map(|v| v.0);
    let voter = voter
        .as_deref()
        .map(|s| parse_b58("voter", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, listing_id, round, voter, choice, power,
               rent_payer
        FROM marketplace_lawyer_vote
        WHERE ($1::bigint IS NULL OR listing_id = $1)
          AND ($2::bytea IS NULL OR voter = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        listing_id,
        voter.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_lawyer_vote
        WHERE ($1::bigint IS NULL OR listing_id = $1)
          AND ($2::bytea IS NULL OR voter = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        listing_id,
        voter.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(LawyerVoteConnection {
        nodes: rows
            .into_iter()
            .map(|r| LawyerVote {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                listing_id: I64(r.listing_id),
                round: I64(r.round),
                voter: b58(&r.voter),
                choice: b58(&r.choice),
                power: I64(r.power),
                rent_payer: b58(&r.rent_payer),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

/// A `pa_*` LEFT-JOIN column that is NULL on a row whose `pa_pubkey` is present would mean
/// the two state tables disagree; the upserts write every column explicitly, so this is a
/// data-integrity error, not a representable state (same policy as `unknown_enum_value`).
fn asset_column_missing(column: &str) -> FieldError {
    FieldError::new(
        format!(
            "marketplace_property_asset.{column} was NULL on a row with a present asset \
             (data integrity issue)"
        ),
        Value::null(),
    )
}

pub async fn listings(
    context: &GraphQLContext,
    listing_id: Option<I64>,
    developer: Option<String>,
    status: Option<ListingStatus>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<ListingConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let listing_id = listing_id.map(|v| v.0);
    let developer = developer
        .as_deref()
        .map(|s| parse_b58("developer", s))
        .transpose()?;
    let status = status.map(|s| s.as_db_str());

    // LEFT JOIN brings `Listing.propertyAsset` along in the same round-trip (1:1 -- see the
    // `Listing` doc), so `listings { propertyAsset { ... } }` costs exactly this query plus
    // the `count(*)`, whatever page size. The join key `asset_id` is indexed
    // (`idx_mkt_property_asset_id`). Every `pa_*` column carries the `?` nullability
    // override: sqlx derives nullability from the source table's NOT NULL constraints (its
    // EXPLAIN-based patch for outer-join sides does not match aliased output columns), so
    // without the override the macro types them as non-`Option` and a listing without an
    // asset row would fail to decode.
    let rows = sqlx::query!(
        r#"
        SELECT l.pubkey, l.slot, l.lamports, l.closed_at_slot, l.listing_id, l.developer,
               l.asset_id, l.share_price, l.listed_share_amount, l.sold_share_amount,
               l.reserved_share_amount, l.tax_paid_by_developer, l.tax_bps,
               l.marketplace_fee_bps, l.investor_fee_bps, l.max_ownership_bps,
               l.listing_expiry, l.claiming_time, l.claim_deadline, l.legal_process_time,
               l.lawyer_voting_time, l.min_voting_quorum_bps, l.position_count,
               l.legal_deadline, l.deposit, l.developer_lawyer, l.developer_lawyer_costs,
               l.developer_lawyer_doc_status, l.developer_lawyer_documents_hash, l.spv_lawyer,
               l.spv_lawyer_costs, l.spv_lawyer_doc_status, l.spv_lawyer_documents_hash,
               l.second_attempt, l.developer_engaged, l.spv_costs_due, l.spv_costs_payee,
               l.collected_fee_quote, l.collected, l.spv_election_expiry,
               l.spv_election_candidate_count, l.spv_election_round, l.status,
               pa.pubkey AS "pa_pubkey?", pa.slot AS "pa_slot?",
               pa.lamports AS "pa_lamports?",
               pa.closed_at_slot AS "pa_closed_at_slot?",
               pa.asset_id AS "pa_asset_id?",
               pa.name AS "pa_name?",
               pa.metadata_uri AS "pa_metadata_uri?",
               pa.share_mint AS "pa_share_mint?",
               pa.region_id AS "pa_region_id?",
               pa.location AS "pa_location?",
               pa.share_amount AS "pa_share_amount?",
               pa.spv_created AS "pa_spv_created?",
               pa.finalized AS "pa_finalized?",
               pa.holder_count AS "pa_holder_count?"
        FROM marketplace_listing AS l
        LEFT JOIN marketplace_property_asset AS pa ON pa.asset_id = l.asset_id
        WHERE ($1::bigint IS NULL OR l.listing_id = $1)
          AND ($2::bytea IS NULL OR l.developer = $2)
          AND ($3::text IS NULL OR l.status = $3)
          AND ($4::bool IS NULL OR (l.closed_at_slot IS NULL) = $4)
        ORDER BY l.slot DESC, l.pubkey ASC
        LIMIT $5 OFFSET $6
        "#,
        listing_id,
        developer.as_deref(),
        status,
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_listing
        WHERE ($1::bigint IS NULL OR listing_id = $1)
          AND ($2::bytea IS NULL OR developer = $2)
          AND ($3::text IS NULL OR status = $3)
          AND ($4::bool IS NULL OR (closed_at_slot IS NULL) = $4)
        "#,
        listing_id,
        developer.as_deref(),
        status,
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    // Attach the fetched metadata document (ADR-27) and its mirrored thumbnails (ADR-31)
    // the same way the `propertyAssets` connection does (ADR-33): one keyed lookup over
    // the derived 1:1 tables for the page's asset PDA pubkeys, skipped when the page
    // carries no asset rows (a `LEFT JOIN` miss). The metadata query is byte-identical to
    // the `property_assets` site's, so both share its cached sqlx provenance.
    let asset_pubkeys: Vec<Vec<u8>> = rows.iter().filter_map(|r| r.pa_pubkey.clone()).collect();
    let mut metadata_by_pubkey: HashMap<Vec<u8>, PropertyMetadata> = HashMap::new();
    if !asset_pubkeys.is_empty() {
        let mrows = sqlx::query!(
            r#"
            SELECT pubkey, asset_id, metadata_uri, fetched_at, attempts, last_error, raw,
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
            FROM marketplace_property_metadata
            WHERE pubkey = ANY($1)
            "#,
            asset_pubkeys.as_slice(),
        )
        .fetch_all(&context.pool)
        .await?;
        for r in mrows {
            // The macro body moves the row's fields, so capture the map key first.
            let key = r.pubkey.clone();
            let metadata = property_metadata_from_row!(r);
            metadata_by_pubkey.insert(key, metadata);
        }
    }
    let mut thumbs_by_pubkey = image_thumbnails(&context.pool, &asset_pubkeys).await?;

    let nodes = rows
        .into_iter()
        .map(|r| {
            let developer_lawyer_doc_status =
                DocumentStatus::from_db_str(&r.developer_lawyer_doc_status).ok_or_else(|| {
                    unknown_enum_value(
                        "developer_lawyer_doc_status",
                        &r.developer_lawyer_doc_status,
                    )
                })?;
            let spv_lawyer_doc_status = DocumentStatus::from_db_str(&r.spv_lawyer_doc_status)
                .ok_or_else(|| {
                    unknown_enum_value("spv_lawyer_doc_status", &r.spv_lawyer_doc_status)
                })?;
            let status = ListingStatus::from_db_str(&r.status)
                .ok_or_else(|| unknown_enum_value("status", &r.status))?;
            let property_asset = match r.pa_pubkey {
                None => None, // no asset row for this listing (see `Listing` doc)
                Some(pa_pubkey) => {
                    // The join is 1:1 (see the `Listing` doc), so the page's asset PDA
                    // pubkeys are unique and each metadata row is consumed exactly once.
                    let metadata = metadata_by_pubkey
                        .remove(&pa_pubkey)
                        .map(|m| with_thumbnails(m, thumbs_by_pubkey.remove(&pa_pubkey)));
                    Some(PropertyAsset {
                        id: ID::new(b58(&pa_pubkey)),
                        slot: I64(r.pa_slot.ok_or_else(|| asset_column_missing("slot"))?),
                        lamports: I64(r
                            .pa_lamports
                            .ok_or_else(|| asset_column_missing("lamports"))?),
                        active: r.pa_closed_at_slot.is_none(),
                        closed_at_slot: r.pa_closed_at_slot.map(I64),
                        asset_id: I64(r
                            .pa_asset_id
                            .ok_or_else(|| asset_column_missing("asset_id"))?),
                        name: r.pa_name.ok_or_else(|| asset_column_missing("name"))?,
                        metadata_uri: r
                            .pa_metadata_uri
                            .ok_or_else(|| asset_column_missing("metadata_uri"))?,
                        metadata,
                        share_mint: b58(&r
                            .pa_share_mint
                            .ok_or_else(|| asset_column_missing("share_mint"))?),
                        region_id: r
                            .pa_region_id
                            .ok_or_else(|| asset_column_missing("region_id"))?,
                        location: utf8_lossy(
                            &r.pa_location
                                .ok_or_else(|| asset_column_missing("location"))?,
                        ),
                        share_amount: I64(r
                            .pa_share_amount
                            .ok_or_else(|| asset_column_missing("share_amount"))?),
                        spv_created: r
                            .pa_spv_created
                            .ok_or_else(|| asset_column_missing("spv_created"))?,
                        finalized: r
                            .pa_finalized
                            .ok_or_else(|| asset_column_missing("finalized"))?,
                        holder_count: I64(r
                            .pa_holder_count
                            .ok_or_else(|| asset_column_missing("holder_count"))?),
                    })
                }
            };
            Ok(Listing {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                listing_id: I64(r.listing_id),
                developer: b58(&r.developer),
                asset_id: I64(r.asset_id),
                share_price: I64(r.share_price),
                listed_share_amount: I64(r.listed_share_amount),
                sold_share_amount: I64(r.sold_share_amount),
                reserved_share_amount: I64(r.reserved_share_amount),
                tax_paid_by_developer: r.tax_paid_by_developer,
                tax_bps: r.tax_bps,
                marketplace_fee_bps: r.marketplace_fee_bps,
                investor_fee_bps: r.investor_fee_bps,
                max_ownership_bps: r.max_ownership_bps,
                listing_expiry: I64(r.listing_expiry),
                claiming_time: I64(r.claiming_time),
                claim_deadline: I64(r.claim_deadline),
                legal_process_time: I64(r.legal_process_time),
                lawyer_voting_time: I64(r.lawyer_voting_time),
                min_voting_quorum_bps: r.min_voting_quorum_bps,
                position_count: I64(r.position_count),
                legal_deadline: I64(r.legal_deadline),
                deposit: I64(r.deposit),
                developer_lawyer: b58(&r.developer_lawyer),
                developer_lawyer_costs: I64(r.developer_lawyer_costs),
                developer_lawyer_doc_status,
                developer_lawyer_documents_hash: hex_string(&r.developer_lawyer_documents_hash),
                spv_lawyer: b58(&r.spv_lawyer),
                spv_lawyer_costs: I64(r.spv_lawyer_costs),
                spv_lawyer_doc_status,
                spv_lawyer_documents_hash: hex_string(&r.spv_lawyer_documents_hash),
                second_attempt: r.second_attempt,
                developer_engaged: r.developer_engaged,
                spv_costs_due: I64(r.spv_costs_due),
                spv_costs_payee: b58(&r.spv_costs_payee),
                collected_fee_quote: I64(r.collected_fee_quote),
                collected: json_string(&r.collected),
                spv_election_expiry: I64(r.spv_election_expiry),
                spv_election_candidate_count: I64(r.spv_election_candidate_count),
                spv_election_round: I64(r.spv_election_round),
                status,
                property_asset,
            })
        })
        .collect::<FieldResult<Vec<_>>>()?;

    Ok(ListingConnection {
        nodes,
        total_count: total_count_i32(total),
    })
}

pub async fn property_assets(
    context: &GraphQLContext,
    asset_id: Option<I64>,
    region_id: Option<i32>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<PropertyAssetConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let asset_id = asset_id.map(|v| v.0);

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, asset_id, name, metadata_uri, share_mint,
               region_id, location, share_amount, spv_created, finalized, holder_count
        FROM marketplace_property_asset
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::int IS NULL OR region_id = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        asset_id,
        region_id,
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_property_asset
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::int IS NULL OR region_id = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        asset_id,
        region_id,
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    // Attach the fetched metadata document (ADR-27) so asset + document answer in ONE
    // GraphQL query: one keyed lookup over the derived 1:1 (at most one metadata row per
    // page node) instead of a second client-side round trip. Skipped on an empty page.
    let pubkeys: Vec<Vec<u8>> = rows.iter().map(|r| r.pubkey.clone()).collect();
    let mut metadata_by_pubkey: HashMap<Vec<u8>, PropertyMetadata> = HashMap::new();
    if !pubkeys.is_empty() {
        let mrows = sqlx::query!(
            r#"
            SELECT pubkey, asset_id, metadata_uri, fetched_at, attempts, last_error, raw,
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
            FROM marketplace_property_metadata
            WHERE pubkey = ANY($1)
            "#,
            pubkeys.as_slice(),
        )
        .fetch_all(&context.pool)
        .await?;
        for r in mrows {
            // The macro body moves the row's fields, so capture the map key first.
            let key = r.pubkey.clone();
            let metadata = property_metadata_from_row!(r);
            metadata_by_pubkey.insert(key, metadata);
        }
    }

    // Attach the mirrored thumbnails (ADR-31) alongside the metadata: one keyed lookup over
    // the derived image table, skipped on an empty page like the metadata lookup.
    let mut thumbs_by_pubkey = image_thumbnails(&context.pool, &pubkeys).await?;

    Ok(PropertyAssetConnection {
        nodes: rows
            .into_iter()
            .map(|r| PropertyAsset {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                asset_id: I64(r.asset_id),
                name: r.name,
                metadata_uri: r.metadata_uri,
                // The page's pubkeys are unique (table PK) and so are the metadata rows', so
                // each lookup is consumed exactly once.
                metadata: metadata_by_pubkey
                    .remove(&r.pubkey)
                    .map(|m| with_thumbnails(m, thumbs_by_pubkey.remove(&r.pubkey))),
                share_mint: b58(&r.share_mint),
                region_id: r.region_id,
                location: utf8_lossy(&r.location),
                share_amount: I64(r.share_amount),
                spv_created: r.spv_created,
                finalized: r.finalized,
                holder_count: I64(r.holder_count),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn reservations(
    context: &GraphQLContext,
    token_account: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<ReservationConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let token_account = token_account
        .as_deref()
        .map(|s| parse_b58("token_account", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, token_account, amount
        FROM marketplace_reservation
        WHERE ($1::bytea IS NULL OR token_account = $1)
          AND ($2::bool IS NULL OR (closed_at_slot IS NULL) = $2)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $3 OFFSET $4
        "#,
        token_account.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_reservation
        WHERE ($1::bytea IS NULL OR token_account = $1)
          AND ($2::bool IS NULL OR (closed_at_slot IS NULL) = $2)
        "#,
        token_account.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(ReservationConnection {
        nodes: rows
            .into_iter()
            .map(|r| Reservation {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                token_account: b58(&r.token_account),
                amount: I64(r.amount),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn share_holdings(
    context: &GraphQLContext,
    asset_id: Option<I64>,
    owner: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<ShareHoldingConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let asset_id = asset_id.map(|v| v.0);
    let owner = owner
        .as_deref()
        .map(|s| parse_b58("owner", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, asset_id, owner, amount,
               lock_lawyer_election, lock_agent_election, lock_proposal, lock_challenge, listed
        FROM marketplace_share_holding
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bytea IS NULL OR owner = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        asset_id,
        owner.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_share_holding
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bytea IS NULL OR owner = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        asset_id,
        owner.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(ShareHoldingConnection {
        nodes: rows
            .into_iter()
            .map(|r| ShareHolding {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                asset_id: I64(r.asset_id),
                owner: b58(&r.owner),
                amount: I64(r.amount),
                lock_lawyer_election: I64(r.lock_lawyer_election),
                lock_agent_election: I64(r.lock_agent_election),
                lock_proposal: I64(r.lock_proposal),
                lock_challenge: I64(r.lock_challenge),
                listed: I64(r.listed),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn share_listings(
    context: &GraphQLContext,
    share_listing_id: Option<I64>,
    asset_id: Option<I64>,
    seller: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<ShareListingConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let share_listing_id = share_listing_id.map(|v| v.0);
    let asset_id = asset_id.map(|v| v.0);
    let seller = seller
        .as_deref()
        .map(|s| parse_b58("seller", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, id, asset_id, seller, share_price,
               amount, fee_bps, next_offer_nonce, rent_payer
        FROM marketplace_share_listing
        WHERE ($1::bigint IS NULL OR id = $1)
          AND ($2::bigint IS NULL OR asset_id = $2)
          AND ($3::bytea IS NULL OR seller = $3)
          AND ($4::bool IS NULL OR (closed_at_slot IS NULL) = $4)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $5 OFFSET $6
        "#,
        share_listing_id,
        asset_id,
        seller.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_share_listing
        WHERE ($1::bigint IS NULL OR id = $1)
          AND ($2::bigint IS NULL OR asset_id = $2)
          AND ($3::bytea IS NULL OR seller = $3)
          AND ($4::bool IS NULL OR (closed_at_slot IS NULL) = $4)
        "#,
        share_listing_id,
        asset_id,
        seller.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(ShareListingConnection {
        nodes: rows
            .into_iter()
            .map(|r| ShareListing {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                share_listing_id: I64(r.id),
                asset_id: I64(r.asset_id),
                seller: b58(&r.seller),
                share_price: I64(r.share_price),
                amount: I64(r.amount),
                fee_bps: r.fee_bps,
                next_offer_nonce: I64(r.next_offer_nonce),
                rent_payer: b58(&r.rent_payer),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn offers(
    context: &GraphQLContext,
    listing_id: Option<I64>,
    offeror: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<OfferConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let listing_id = listing_id.map(|v| v.0);
    let offeror = offeror
        .as_deref()
        .map(|s| parse_b58("offeror", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, listing_id, asset_id, offeror,
               share_price, amount, payment_mint, held, nonce, rent_payer
        FROM marketplace_offer
        WHERE ($1::bigint IS NULL OR listing_id = $1)
          AND ($2::bytea IS NULL OR offeror = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        listing_id,
        offeror.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_offer
        WHERE ($1::bigint IS NULL OR listing_id = $1)
          AND ($2::bytea IS NULL OR offeror = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        listing_id,
        offeror.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(OfferConnection {
        nodes: rows
            .into_iter()
            .map(|r| Offer {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                listing_id: I64(r.listing_id),
                asset_id: I64(r.asset_id),
                offeror: b58(&r.offeror),
                share_price: I64(r.share_price),
                amount: I64(r.amount),
                payment_mint: b58(&r.payment_mint),
                held: I64(r.held),
                nonce: I64(r.nonce),
                rent_payer: b58(&r.rent_payer),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

/// Fetched and decomposed off-chain property metadata (ADR-27). `fetchedAt DESC NULLS LAST,
/// pubkey ASC`: newest snapshots first, rows that never fetched successfully (fetch state
/// only) last, stable tiebreak within a snapshot time.
pub async fn property_metadata(
    context: &GraphQLContext,
    asset_id: Option<I64>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<PropertyMetadataConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let asset_id = asset_id.map(|v| v.0);

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, asset_id, metadata_uri, fetched_at, attempts, last_error, raw,
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
        FROM marketplace_property_metadata
        WHERE ($1::bigint IS NULL OR asset_id = $1)
        ORDER BY fetched_at DESC NULLS LAST, pubkey ASC
        LIMIT $2 OFFSET $3
        "#,
        asset_id,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_property_metadata
        WHERE ($1::bigint IS NULL OR asset_id = $1)
        "#,
        asset_id,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    // Attach the mirrored thumbnails (ADR-31), the same keyed lookup `property_assets` uses.
    let pubkeys: Vec<Vec<u8>> = rows.iter().map(|r| r.pubkey.clone()).collect();
    let mut thumbs_by_pubkey = image_thumbnails(&context.pool, &pubkeys).await?;

    let nodes = rows
        .into_iter()
        .map(|r| {
            // The macro body moves the row's fields, so capture the map key first.
            let key = r.pubkey.clone();
            with_thumbnails(
                property_metadata_from_row!(r),
                thumbs_by_pubkey.remove(&key),
            )
        })
        .collect();

    Ok(PropertyMetadataConnection {
        nodes,
        total_count: total_count_i32(total),
    })
}
