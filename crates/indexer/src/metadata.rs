//! The off-chain property metadata fetcher (ADR-27): the background loop that turns each
//! `marketplace_property_asset.metadata_uri` into the decomposed `marketplace_property_metadata`
//! row.
//!
//! ## Why a separate loop and not the account pipeline
//!
//! The pipeline (`datasource -> decode -> map -> batcher`) is pure chain-mirror machinery:
//! every write is idempotent, slot-guarded, and must never fail deterministically -- the
//! batcher retries a failed batch forever, so one dead URI would stall ingestion of ALL
//! four programs (the write-migration skill's stall trap). The metadata document is
//! off-chain, mutable, and operator-hosted: fetching it belongs in its own loop with its
//! own per-URI backoff, writing through `db::property_metadata` and never through the
//! batcher.
//!
//! ## One cycle
//!
//! 1. `db::property_metadata::pending_assets` selects the work set: open assets with a
//!    non-empty `metadata_uri` that have no stored row, whose stored row points at a
//!    different URI, or whose last attempt failed past its backoff deadline.
//! 2. Each item (bounded by [`CYCLE_LIMIT`], sequential -- the devnet asset count is small
//!    and sequential fetches keep the object-storage load trivial): URI guard -> bounded
//!    HTTP GET -> lenient per-field parse -> upsert. A failure records its error +
//!    backoff on the row and moves on; one asset's fault never fails the loop.
//! 3. The `property_metadata_pending` gauge is set to the remaining work-set size.
//!
//! A fetch is a point-in-time SNAPSHOT: `fetched_at` is its provenance, and the document
//! is re-fetched only when the on-chain URI changes or a fetch fails (not on off-chain
//! content edits under an unchanged URI -- see ADR-27).
//!
//! The same [`cycle`] backs the one-shot `indexer fetch-metadata` subcommand (run by hand
//! against a production `DATABASE_URL`, like `snapshot` / `backfill`).

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use futures::StreamExt;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::db::property_metadata::{self, PropertyMetadataRow};

/// How many work-set items one cycle fetches. The rest wait for the next cycle: a cycle
/// must stay bounded so a long object-storage outage cannot stretch it past several
/// intervals.
const CYCLE_LIMIT: i64 = 50;
/// Total wall time for one metadata GET (connect + body).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Connect phase only.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Refuse to buffer more than this many bytes of one document (the fetch is streamed and
/// cut off at the cap -- a hostile or misconfigured endpoint cannot exhaust memory).
pub const MAX_DOCUMENT_BYTES: usize = 2 * 1024 * 1024;
/// `last_error` is an operator-facing log line in the database; keep it short.
const MAX_ERROR_LEN: usize = 500;

/// One cycle's outcome.
#[derive(Debug, Clone, Copy, Default)]
pub struct CycleSummary {
    /// Work-set items taken up this cycle.
    pub attempted: usize,
    /// Successful fetches (snapshots upserted).
    pub succeeded: usize,
    /// Failed fetches (error + backoff recorded on the row).
    pub failed: usize,
}

/// The decomposed metadata document: every field is `Option` -- the parsing is LENIENT
/// PER FIELD (ADR-27): a missing key or a mis-typed value stores `None` for that column
/// while the rest of the document is still indexed (and the whole document remains in
/// `raw`). Only a body that is not a JSON object at all is a parse failure.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Document {
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
    // `address` object.
    pub address_street: Option<String>,
    pub address_town_city: Option<String>,
    pub address_flat_or_unit: Option<String>,
    pub address_post_code: Option<String>,
    pub address_local_authority: Option<String>,
    pub address_region: Option<String>,
    pub address_location: Option<String>,
    // `attributes` object.
    pub area: Option<String>,
    pub quality: Option<String>,
    pub outdoor_space: Option<String>,
    pub number_of_bedrooms: Option<i64>,
    pub number_of_bathrooms: Option<i64>,
    pub construction_date: Option<NaiveDate>,
    pub off_street_parking: Option<String>,
    // `finances` object.
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
    pub other_documents: Option<serde_json::Value>,
    pub property_images: Option<serde_json::Value>,
}

/// One fetched, parsed document plus its verbatim ground truth.
pub struct ParsedDocument {
    /// The decomposed fields (lenient per field).
    pub document: Document,
    /// The whole document as fetched: what the typed columns are derived from, and what a
    /// future document field costs no migration to retrieve.
    pub raw: serde_json::Value,
}

/// Parse one fetched document. `Ok` for any JSON object (fields extracted leniently, see
/// [`Document`]); `Err` only when the body is not a JSON object at all -- the fetch then
/// fails and is retried with backoff like any other error.
pub fn decode_document(bytes: &[u8]) -> Result<ParsedDocument> {
    let value =
        serde_json::from_slice::<serde_json::Value>(bytes).with_context(|| "not valid JSON")?;
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow!("top-level JSON is not an object"))?;

    // The top-level scalars initialize in place; the `address` / `attributes` /
    // `finances` groups stay `None` until their objects are present (the blocks below).
    let mut doc = Document {
        property_id: str_field(obj, "propertyId"),
        property_name: str_field(obj, "propertyName"),
        property_type: str_field(obj, "propertyType"),
        status: str_field(obj, "status"),
        tenure: str_field(obj, "tenure"),
        property_description: str_field(obj, "propertyDescription"),
        planning_code: str_field(obj, "planningCode"),
        building_control_code: str_field(obj, "buildingControlCode"),
        user_pubkey: wallet_field(obj, "userId"),
        company_id: str_field(obj, "companyId"),
        company_name: str_field(obj, "companyName"),
        company_logo: str_field(obj, "companyLogo"),
        company_wallet_address: wallet_field(obj, "companyWalletAddress"),
        created_at: datetime_field(obj, "createdAt"),
        updated_at: datetime_field(obj, "updatedAt"),
        floor_plan: str_field(obj, "floorPlan"),
        map_url: str_field(obj, "map"),
        sales_agreement: str_field(obj, "salesAgreement"),
        other_documents: string_array_field(obj, "otherDocuments"),
        property_images: string_array_field(obj, "propertyImages"),
        ..Default::default()
    };

    // The `address` object.
    if let Some(address) = obj.get("address").and_then(|v| v.as_object()) {
        doc.address_street = str_field(address, "street");
        doc.address_town_city = str_field(address, "townCity");
        doc.address_flat_or_unit = str_field(address, "flatOrUnit");
        doc.address_post_code = str_field(address, "postCode");
        doc.address_local_authority = str_field(address, "localAuthority");
        doc.address_region = str_field(address, "region");
        doc.address_location = str_field(address, "location");
    }

    // The `attributes` object.
    if let Some(attributes) = obj.get("attributes").and_then(|v| v.as_object()) {
        doc.area = str_field(attributes, "area");
        doc.quality = str_field(attributes, "quality");
        doc.outdoor_space = str_field(attributes, "outdoorSpace");
        doc.number_of_bedrooms = i64_field(attributes, "numberOfBedrooms");
        doc.number_of_bathrooms = i64_field(attributes, "numberOfBathrooms");
        doc.construction_date = date_field(attributes, "constructionDate");
        doc.off_street_parking = str_field(attributes, "offStreetParking");
    }

    // The `finances` object.
    if let Some(finances) = obj.get("finances").and_then(|v| v.as_object()) {
        doc.property_price = i64_field(finances, "propertyPrice");
        doc.number_of_shares = i64_field(finances, "numberOfShares");
        doc.share_price = i64_field(finances, "sharePrice");
        doc.estimated_rental_income = i64_field(finances, "estimatedRentalIncome");
        doc.annual_service_charge = i64_field(finances, "annualServiceCharge");
        doc.stamp_duty_tax = i64_field(finances, "stampDutyTax");
        doc.stamp_duty_paid = bool_field(finances, "isStampDutyPaid");
        doc.annual_service_charge_paid = bool_field(finances, "isAnnualServiceChargePaid");
    }

    Ok(ParsedDocument {
        document: doc,
        raw: value,
    })
}

// --- lenient field extractors (a wrong type is None for that field, never an error) ------

fn str_field(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(|v| v.as_str()).map(str::to_owned)
}

fn i64_field(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<i64> {
    obj.get(key).and_then(|v| v.as_i64())
}

fn bool_field(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<bool> {
    obj.get(key).and_then(|v| v.as_bool())
}

/// `"2026-08-25"` -> `NaiveDate` (the document's `constructionDate` spelling).
fn date_field(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<NaiveDate> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
}

/// RFC 3339 (`2026-08-25T09:08:44.630Z`) -> `DateTime<Utc>`.
fn datetime_field(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<DateTime<Utc>> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// A base58 Solana wallet address -> the 32 raw bytes the BYTEA columns store; an invalid
/// or wrong-length string is `None` (never an error -- ADR-27's lenient parsing).
fn wallet_field(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<Vec<u8>> {
    obj.get(key).and_then(|v| v.as_str()).and_then(|s| {
        let bytes = bs58::decode(s).into_vec().ok()?;
        (bytes.len() == 32).then_some(bytes)
    })
}

/// A genuine list of URL strings -> a JSONB array in the shape the indexer constructs
/// (0009/0010's JSONB rule: never a crate's serde output); anything that is not an
/// all-string array is `None`.
fn string_array_field(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<serde_json::Value> {
    let arr = obj.get(key)?.as_array()?;
    let urls: Vec<serde_json::Value> = arr
        .iter()
        .filter_map(|v| v.as_str().map(|s| serde_json::Value::String(s.to_owned())))
        .collect();
    (urls.len() == arr.len()).then_some(serde_json::Value::Array(urls))
}

// --- the fetch ----------------------------------------------------------------------------

/// Build the fetcher's HTTP client: rustls-only TLS (matching sqlx's
/// `runtime-tokio-rustls`), finite timeouts.
pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .expect("a client with finite timeouts always builds")
}

/// Whether an IP literal is a GLOBAL (public) address: not loopback, not private
/// (RFC 1918, CGNAT `100.64.0.0/10`, unique-local `fc00::/7`), not link-local
/// (`169.254.0.0/16`, which holds the `169.254.169.254` cloud-metadata endpoint), not
/// unspecified, and (for IPv4) not a documentation address. `IpAddr::is_global` is the
/// same contract but unstable on stable Rust, so the stable predicates compose it here.
fn is_global_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            // RFC 6598 CGNAT (100.64.0.0/10) is checked explicitly: `Ipv4Addr::is_private`
            // does not cover it on this toolchain.
            let cgnat = v4.octets()[0] == 100 && v4.octets()[1] & 0xc0 == 0x40;
            !v4.is_loopback()
                && !v4.is_private()
                && !cgnat
                && !v4.is_link_local()
                && !v4.is_unspecified()
                && !v4.is_documentation()
        }
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            // An IPv4-mapped literal (`::ffff:a.b.c.d`) is judged as the mapped IPv4.
            Some(mapped) => is_global_ip(std::net::IpAddr::V4(mapped)),
            None => {
                // `Ipv6Addr::is_private` / `is_link_local` are unstable on stable Rust:
                // fc00::/7 (unique-local) is tested by its first-octet prefix, fe80::/10
                // via the stable unicast-link-local predicate.
                let unique_local = v6.octets()[0] & 0xfe == 0xfc;
                !v6.is_loopback()
                    && !unique_local
                    && !v6.is_unicast_link_local()
                    && !v6.is_unspecified()
            }
        },
    }
}

/// SSRF guard for an on-chain-supplied URL (ADR-27): http/https only, a non-empty host is
/// required, `localhost` is rejected, and IP-LITERAL hosts must be global (loopback,
/// private and link-local literals are refused). DNS names are allowed as-is -- a hostname
/// resolving to a private IP is a documented, accepted devnet limitation (the URIs come
/// from the protocol team's own deployments).
pub fn uri_allowed(uri: &str) -> Result<()> {
    let url = reqwest::Url::parse(uri).with_context(|| format!("not a parseable URL: {uri:?}"))?;
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(anyhow!("scheme {other:?} is not allowed (http/https only)")),
    }
    let host = url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| anyhow!("the URL has no host"))?;
    if host.eq_ignore_ascii_case("localhost") {
        return Err(anyhow!("host {host:?} is not allowed"));
    }
    // `host_str()` serializes an IPv6 literal with brackets (`[::1]`); a domain can never
    // contain brackets, so stripping them is the faithful inverse for IP-literal parsing.
    let maybe_ip = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = maybe_ip.parse::<std::net::IpAddr>() {
        if !is_global_ip(ip) {
            return Err(anyhow!("host {maybe_ip} is a non-global IP literal"));
        }
    }
    Ok(())
}

/// GET one document, streaming the body and refusing to buffer more than
/// [`MAX_DOCUMENT_BYTES`].
pub async fn fetch_document(client: &reqwest::Client, uri: &str) -> Result<Vec<u8>> {
    let response = client
        .get(uri)
        .send()
        .await
        .with_context(|| format!("GET {uri} failed"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("GET {uri} returned HTTP {status}"));
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading the response body failed")?;
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(anyhow!(
                "GET {uri}: response body exceeds the {MAX_DOCUMENT_BYTES}-byte cap"
            ));
        }
    }
    Ok(bytes)
}

/// One fetch attempt for one work-set item: guard the URI, fetch, parse.
pub async fn fetch_one(
    client: &reqwest::Client,
    asset: &property_metadata::PendingAsset,
) -> Result<ParsedDocument> {
    uri_allowed(&asset.metadata_uri)?;
    let bytes = fetch_document(client, &asset.metadata_uri).await?;
    decode_document(&bytes)
        .with_context(|| format!("parsing the document at {}", asset.metadata_uri))
}

/// Assemble the upsert row for one successful fetch.
fn row_for(
    asset: &property_metadata::PendingAsset,
    parsed: &ParsedDocument,
    fetched_at: DateTime<Utc>,
) -> PropertyMetadataRow {
    let d = &parsed.document;
    PropertyMetadataRow {
        pubkey: asset.pubkey.clone(),
        asset_id: asset.asset_id,
        metadata_uri: asset.metadata_uri.clone(),
        fetched_at,
        raw: parsed.raw.clone(),
        property_id: d.property_id.clone(),
        property_name: d.property_name.clone(),
        property_type: d.property_type.clone(),
        status: d.status.clone(),
        tenure: d.tenure.clone(),
        property_description: d.property_description.clone(),
        planning_code: d.planning_code.clone(),
        building_control_code: d.building_control_code.clone(),
        user_pubkey: d.user_pubkey.clone(),
        company_id: d.company_id.clone(),
        company_name: d.company_name.clone(),
        company_logo: d.company_logo.clone(),
        company_wallet_address: d.company_wallet_address.clone(),
        created_at: d.created_at,
        updated_at: d.updated_at,
        address_street: d.address_street.clone(),
        address_town_city: d.address_town_city.clone(),
        address_flat_or_unit: d.address_flat_or_unit.clone(),
        address_post_code: d.address_post_code.clone(),
        address_local_authority: d.address_local_authority.clone(),
        address_region: d.address_region.clone(),
        address_location: d.address_location.clone(),
        area: d.area.clone(),
        quality: d.quality.clone(),
        outdoor_space: d.outdoor_space.clone(),
        number_of_bedrooms: d.number_of_bedrooms,
        number_of_bathrooms: d.number_of_bathrooms,
        construction_date: d.construction_date,
        off_street_parking: d.off_street_parking.clone(),
        property_price: d.property_price,
        number_of_shares: d.number_of_shares,
        share_price: d.share_price,
        estimated_rental_income: d.estimated_rental_income,
        annual_service_charge: d.annual_service_charge,
        stamp_duty_tax: d.stamp_duty_tax,
        stamp_duty_paid: d.stamp_duty_paid,
        annual_service_charge_paid: d.annual_service_charge_paid,
        floor_plan: d.floor_plan.clone(),
        map_url: d.map_url.clone(),
        sales_agreement: d.sales_agreement.clone(),
        other_documents: d.other_documents.clone(),
        property_images: d.property_images.clone(),
    }
}

// --- the loop -----------------------------------------------------------------------------

/// Run one fetch cycle (also the whole job of the `indexer fetch-metadata` subcommand):
/// select the work set, fetch each item, upsert successes, record failures with backoff,
/// and set the pending gauge. `shutdown` only gates BETWEEN items -- a cancel stops the
/// cycle, never an in-flight request (which has its own 15 s timeout).
pub async fn cycle(
    pool: &PgPool,
    client: &reqwest::Client,
    shutdown: &CancellationToken,
) -> Result<CycleSummary> {
    let pending = property_metadata::pending_assets(pool, CYCLE_LIMIT)
        .await
        .context("selecting the metadata work set")?;

    let mut summary = CycleSummary::default();
    for asset in &pending {
        if shutdown.is_cancelled() {
            break;
        }
        summary.attempted += 1;
        match fetch_one(client, asset).await {
            Ok(parsed) => {
                let fetched_at = Utc::now();
                let row = row_for(asset, &parsed, fetched_at);
                property_metadata::upsert_success(pool, &row)
                    .await
                    .with_context(|| {
                        format!("storing the fetched metadata for asset {}", asset.asset_id)
                    })?;
                summary.succeeded += 1;
                crate::metrics::inc_property_metadata_fetch("success");
                log::info!(
                    "property metadata fetch: asset {} fetched from {} at {}",
                    asset.asset_id,
                    asset.metadata_uri,
                    fetched_at.to_rfc3339()
                );
            }
            Err(e) => {
                summary.failed += 1;
                crate::metrics::inc_property_metadata_fetch("failure");
                // A failure's error + backoff is recorded on the row (survives restarts,
                // feeds the work set's retry gate). The DB write itself failing means the
                // pool is down -- propagate: the supervisor retries the whole cycle next
                // interval, and nothing else in the process depends on this one.
                let error = format!("{e:#}");
                let error: String = error.chars().take(MAX_ERROR_LEN).collect();
                property_metadata::record_failure(
                    pool,
                    &asset.pubkey,
                    asset.asset_id,
                    &asset.metadata_uri,
                    &error,
                )
                .await
                .with_context(|| {
                    format!("recording the fetch failure for asset {}", asset.asset_id)
                })?;
                log::warn!(
                    "property metadata fetch: asset {} failed ({}); retrying with backoff",
                    asset.asset_id,
                    error
                );
            }
        }
    }

    let remaining = property_metadata::count_pending(pool)
        .await
        .context("counting the remaining metadata work set")?;
    crate::metrics::set_property_metadata_pending(remaining);

    if summary.attempted > 0 {
        log::info!(
            "property metadata fetch cycle: {} attempted, {} fetched, {} failed ({} still pending)",
            summary.attempted,
            summary.succeeded,
            summary.failed,
            remaining
        );
    }
    Ok(summary)
}

/// Run fetch cycles until `shutdown` fires (spawned by `run` next to the reconciliation
/// supervisor; ADR-27).
pub async fn supervise(pool: &PgPool, interval: Duration, shutdown: CancellationToken) {
    log::info!(
        "property metadata fetcher started (every {interval:?}, up to {CYCLE_LIMIT} asset(s) per cycle)"
    );
    let client = build_client();
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        match cycle(pool, &client, &shutdown).await {
            // `cycle` logs its own per-asset and summary lines.
            Ok(_) => {}
            Err(e) => log::error!(
                "property metadata fetch cycle failed (will retry next interval): {e:#}"
            ),
        }

        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.cancelled() => break,
        }
    }
    log::info!("property metadata fetcher stopping");
}

// --- tests (hermetic: parsing and the URI guard only; the network path is covered by the
// --- `indexer fetch-metadata` run against devnet in verify-and-ship) ----------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The protocol team's reference document shape (2026-08-25 devnet), trimmed to the
    /// fields the tests assert on.
    const SAMPLE: &str = r#"{
      "address": {
        "street": "The Willows",
        "townCity": "St Albans",
        "flatOrUnit": "Apartment 2B",
        "postCode": "AL1",
        "localAuthority": "East London District Council",
        "region": "Hertfordshire",
        "location": "St Albans, Hertfordshire"
      },
      "attributes": {
        "area": "72 m² (775 sq ft)",
        "quality": "High",
        "outdoorSpace": "Juliet balcony",
        "numberOfBedrooms": 2,
        "numberOfBathrooms": 1,
        "constructionDate": "2026-08-25",
        "offStreetParking": "Allocated parking space"
      },
      "buildingControlCode": "BC-01-8821",
      "companyId": "company_4vAESqKcSxBXlHjNFZ3JiQ",
      "companyLogo": "https://xcavate-profile.fsn1.your-objectstorage.com/companies/logo.png",
      "companyName": "Oak & Spire Developments Ltd.",
      "companyWalletAddress": "3oVtApF8dsfZJSEbCi8TM6fq7xZgWF2J2WeWeeJe36Q5",
      "createdAt": "2026-08-25T09:08:44.630Z",
      "finances": {
        "propertyPrice": 395000,
        "numberOfShares": 100,
        "sharePrice": 3950,
        "estimatedRentalIncome": 2000,
        "annualServiceCharge": 0,
        "stampDutyTax": 0,
        "isStampDutyPaid": true,
        "isAnnualServiceChargePaid": true
      },
      "floorPlan": "https://bucket.s3.eu-west-1.amazonaws.com/floor-plan.png",
      "map": "https://maps.google.com/?q=property+01",
      "otherDocuments": [
        "https://bucket.s3.eu-west-1.amazonaws.com/certificate.pdf"
      ],
      "planningCode": "PLN-01-2024",
      "propertyDescription": "A light-filled apartment.",
      "propertyId": "prp_ow2wHZvnyMsw",
      "propertyImages": [
        "https://bucket.s3.eu-west-1.amazonaws.com/prop1.jpg"
      ],
      "propertyName": "The Willows – Apartment 2B",
      "propertyType": "Apartment",
      "salesAgreement": "https://bucket.s3.eu-west-1.amazonaws.com/agreement.pdf",
      "status": "verified",
      "tenure": "Leasehold",
      "updatedAt": "2026-08-25T09:27:05.623Z",
      "userId": "3oVtApF8dsfZJSEbCi8TM6fq7xZgWF2J2WeWeeJe36Q5"
    }"#;

    #[test]
    fn the_reference_document_parses_into_every_field() {
        let parsed = decode_document(SAMPLE.as_bytes()).expect("the sample parses");
        let d = &parsed.document;

        // Top level.
        assert_eq!(d.property_id.as_deref(), Some("prp_ow2wHZvnyMsw"));
        assert_eq!(
            d.property_name.as_deref(),
            Some("The Willows – Apartment 2B")
        );
        assert_eq!(d.property_type.as_deref(), Some("Apartment"));
        assert_eq!(d.status.as_deref(), Some("verified"));
        assert_eq!(d.tenure.as_deref(), Some("Leasehold"));
        assert_eq!(
            d.property_description.as_deref(),
            Some("A light-filled apartment.")
        );
        assert_eq!(d.planning_code.as_deref(), Some("PLN-01-2024"));
        assert_eq!(d.building_control_code.as_deref(), Some("BC-01-8821"));
        assert_eq!(
            d.company_name.as_deref(),
            Some("Oak & Spire Developments Ltd.")
        );
        assert_eq!(
            d.company_id.as_deref(),
            Some("company_4vAESqKcSxBXlHjNFZ3JiQ")
        );
        assert_eq!(
            d.floor_plan.as_deref(),
            Some("https://bucket.s3.eu-west-1.amazonaws.com/floor-plan.png")
        );
        assert_eq!(
            d.map_url.as_deref(),
            Some("https://maps.google.com/?q=property+01")
        );
        assert_eq!(
            d.sales_agreement.as_deref(),
            Some("https://bucket.s3.eu-west-1.amazonaws.com/agreement.pdf")
        );

        // Wallets decode to the 32 raw bytes the BYTEA columns store.
        let wallet = bs58::decode("3oVtApF8dsfZJSEbCi8TM6fq7xZgWF2J2WeWeeJe36Q5")
            .into_vec()
            .unwrap();
        assert_eq!(d.user_pubkey.as_deref(), Some(wallet.as_slice()));
        assert_eq!(d.company_wallet_address.as_deref(), Some(wallet.as_slice()));

        // Timestamps.
        assert_eq!(
            d.created_at
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "2026-08-25T09:08:44.630Z"
        );
        assert_eq!(
            d.updated_at
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "2026-08-25T09:27:05.623Z"
        );

        // address / attributes / finances.
        assert_eq!(d.address_street.as_deref(), Some("The Willows"));
        assert_eq!(d.address_town_city.as_deref(), Some("St Albans"));
        assert_eq!(d.address_flat_or_unit.as_deref(), Some("Apartment 2B"));
        assert_eq!(d.address_post_code.as_deref(), Some("AL1"));
        assert_eq!(
            d.address_local_authority.as_deref(),
            Some("East London District Council")
        );
        assert_eq!(d.address_region.as_deref(), Some("Hertfordshire"));
        assert_eq!(
            d.address_location.as_deref(),
            Some("St Albans, Hertfordshire")
        );
        assert_eq!(d.area.as_deref(), Some("72 m² (775 sq ft)"));
        assert_eq!(d.quality.as_deref(), Some("High"));
        assert_eq!(d.outdoor_space.as_deref(), Some("Juliet balcony"));
        assert_eq!(d.number_of_bedrooms, Some(2));
        assert_eq!(d.number_of_bathrooms, Some(1));
        assert_eq!(
            d.construction_date,
            Some(NaiveDate::parse_from_str("2026-08-25", "%Y-%m-%d").unwrap())
        );
        assert_eq!(
            d.off_street_parking.as_deref(),
            Some("Allocated parking space")
        );
        assert_eq!(d.property_price, Some(395000));
        assert_eq!(d.number_of_shares, Some(100));
        assert_eq!(d.share_price, Some(3950));
        assert_eq!(d.estimated_rental_income, Some(2000));
        assert_eq!(d.annual_service_charge, Some(0));
        assert_eq!(d.stamp_duty_tax, Some(0));
        assert_eq!(d.stamp_duty_paid, Some(true));
        assert_eq!(d.annual_service_charge_paid, Some(true));

        // URL lists land as JSONB arrays in the indexer-constructed shape.
        assert_eq!(
            d.property_images,
            Some(serde_json::json!([
                "https://bucket.s3.eu-west-1.amazonaws.com/prop1.jpg"
            ]))
        );
        assert_eq!(
            d.other_documents,
            Some(serde_json::json!([
                "https://bucket.s3.eu-west-1.amazonaws.com/certificate.pdf"
            ]))
        );

        // `raw` is the whole document, verbatim (unknown fields survive).
        assert_eq!(parsed.raw["status"], "verified");
        assert_eq!(parsed.raw["address"]["street"], "The Willows");
    }

    #[test]
    fn an_empty_object_parses_into_all_none_fields() {
        let parsed = decode_document(br#"{ }"#).expect("an empty object parses");
        assert_eq!(parsed.document, Document::default());
        assert_eq!(parsed.raw, serde_json::json!({}));
    }

    #[test]
    fn a_non_object_body_is_a_parse_error() {
        assert!(decode_document(br#"[1, 2]"#).is_err());
        assert!(decode_document(br#""just a string""#).is_err());
        assert!(decode_document(b"not json at all").is_err());
    }

    #[test]
    fn a_mis_typed_field_is_none_not_an_error() {
        // Lenient per field (ADR-27): the bad field is dropped, the rest still indexes.
        let body = br#"{
            "numberOfBedrooms": null,
            "attributes": { "numberOfBedrooms": "two", "numberOfBathrooms": 1 },
            "address": "not an object",
            "finances": { "propertyPrice": "395000", "isStampDutyPaid": "yes" },
            "propertyImages": ["ok.jpg", 42],
            "status": "verified"
        }"#;
        let parsed = decode_document(body).expect("a mis-typed field must not fail the fetch");
        let d = &parsed.document;
        assert_eq!(d.number_of_bedrooms, None);
        assert_eq!(d.number_of_bathrooms, Some(1));
        assert!(
            d.address_street.is_none(),
            "a non-object address stores no address fields"
        );
        assert_eq!(d.property_price, None);
        assert_eq!(d.stamp_duty_paid, None);
        assert_eq!(
            d.property_images, None,
            "a mixed-type list is not a URL list"
        );
        assert_eq!(
            d.status.as_deref(),
            Some("verified"),
            "good fields still index"
        );
    }

    #[test]
    fn invalid_wallet_and_date_strings_are_none_not_errors() {
        let body = br#"{
            "userId": "not-base58!!",
            "companyWalletAddress": "tooShort",
            "createdAt": "yesterday",
            "attributes": { "constructionDate": "25/08/2026" }
        }"#;
        let parsed = decode_document(body).expect("bad wallet/date values must not fail");
        assert_eq!(parsed.document.user_pubkey, None);
        assert_eq!(parsed.document.company_wallet_address, None);
        assert_eq!(parsed.document.created_at, None);
        assert_eq!(parsed.document.construction_date, None);
    }

    // --- uri_allowed ---------------------------------------------------------------------

    #[test]
    fn public_http_and_https_uris_are_allowed() {
        assert!(uri_allowed(
            "https://realxmarketplace-dev-bucket.s3.eu-west-1.amazonaws.com/x.json"
        )
        .is_ok());
        assert!(uri_allowed("http://example.com/x.json").is_ok());
        // `https:///no-host` is not hostless: the extra slash is a tolerated violation and
        // the URL normalizes to host `no-host` -- a DNS name, allowed like any other.
        assert!(uri_allowed("https:///no-host").is_ok());
        // Public IP literals are global.
        assert!(uri_allowed("https://52.216.100.1/bucket/x.json").is_ok());
    }

    #[test]
    fn non_http_schemes_and_hostless_urls_are_refused() {
        assert!(uri_allowed("ftp://example.com/x.json").is_err());
        assert!(uri_allowed("file:///etc/passwd").is_err());
        assert!(uri_allowed("").is_err());
        assert!(uri_allowed("not a url").is_err());
        // Truly hostless http(s) URLs are refused at parse time: url rejects an empty
        // host on a special scheme (EmptyHost), which `uri_allowed` surfaces as an error.
        assert!(uri_allowed("https://").is_err());
        assert!(uri_allowed("https://:8443/").is_err());
    }

    #[test]
    fn loopback_private_and_link_local_ips_are_refused() {
        // The SSRF guard (ADR-27): the indexer must never be pointed at a private service
        // by an on-chain-supplied URI -- 169.254.169.254 is the cloud metadata endpoint.
        for uri in [
            "http://127.0.0.1/x",
            "http://localhost/x",
            "http://LOCALHOST/x",
            "http://[::1]/x",
            "http://10.0.0.5/x",
            "http://192.168.1.10/x",
            "http://169.254.169.254/latest/meta-data/",
            "http://172.16.0.1/x",
            "http://100.64.0.1/x",
            "http://0.0.0.0/x",
        ] {
            assert!(uri_allowed(uri).is_err(), "{uri} must be refused");
        }
    }
}
