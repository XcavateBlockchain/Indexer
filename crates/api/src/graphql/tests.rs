//! End-to-end GraphQL tests: execute real queries against a migrated, seeded database.
//! Run with a live Postgres reachable via `DATABASE_URL` (same harness as the indexer
//! crate's `db::tests`); `#[sqlx::test]` creates and migrates a fresh throwaway database
//! per test.

use std::sync::Arc;

use sqlx::PgPool;

use super::{GraphQLContext, QueryRoot, Schema};
use crate::chain_tip::ChainTipCache;

fn context(pool: PgPool) -> GraphQLContext {
    GraphQLContext {
        pool,
        // No resolver under test touches the chain tip (only `syncStatus`/`/health` do);
        // an empty endpoint list would error only if one did.
        chain_tip: Arc::new(ChainTipCache::new(vec![])),
        program_filter: None,
    }
}

fn schema() -> Schema {
    Schema::new(
        QueryRoot,
        carbon_core::graphql::server::DefaultMutation::new(),
        carbon_core::graphql::server::DefaultSubscription::new(),
    )
}

/// One open `marketplace_property_asset` row (asset_id 7) plus its fetched metadata
/// document (ADR-27). Only the NOT NULL columns are seeded; everything else takes the
/// column default.
async fn seed_asset_with_metadata(pool: &PgPool, pubkey: &[u8], property_images: &str) {
    sqlx::query(
        r#"INSERT INTO marketplace_property_asset
               (pubkey, slot, lamports, asset_id, name, metadata_uri, share_mint,
                region_id, location, share_amount, spv_created, finalized, holder_count, bump)
           VALUES ($1, 100, 1000000, 7, 'Test House', 'https://md.example/7.json', $2,
                   1, $3, 1000, TRUE, FALSE, 3, 255)"#,
    )
    .bind(pubkey)
    .bind(vec![8u8; 32])
    .bind(b"London".as_slice())
    .execute(pool)
    .await
    .expect("seed marketplace_property_asset");

    sqlx::query(
        r#"INSERT INTO marketplace_property_metadata
               (pubkey, asset_id, metadata_uri, fetched_at, property_images)
           VALUES ($1, 7, 'https://md.example/7.json', now(), $2::jsonb)"#,
    )
    .bind(pubkey)
    .bind(property_images)
    .execute(pool)
    .await
    .expect("seed marketplace_property_metadata");
}

async fn seed_image(pool: &PgPool, pubkey: &[u8], index: i32, thumb_uri: Option<&str>) {
    sqlx::query(
        r#"INSERT INTO marketplace_property_image
               (asset_pubkey, image_index, source_uri, thumb_uri, uploaded_at)
           VALUES ($1, $2, $3, $4, CASE WHEN $4 IS NULL THEN NULL ELSE now() END)"#,
    )
    .bind(pubkey)
    .bind(index)
    .bind(format!("https://img.example/{index}.png"))
    .bind(thumb_uri)
    .execute(pool)
    .await
    .expect("seed marketplace_property_image");
}

/// One marketplace_listing row with caller-chosen ids, slot (the feed's ordering key) and
/// status. All NOT NULL columns are seeded; the CHECK-constrained TEXT columns take a valid
/// enum value. A dangling `asset_id` is fine: the resolver LEFT JOINs the asset.
async fn seed_listing_row(
    pool: &PgPool,
    pubkey: &[u8],
    listing_id: i64,
    asset_id: i64,
    slot: i64,
    status: &str,
) {
    sqlx::query(
        r#"INSERT INTO marketplace_listing
               (pubkey, slot, lamports, listing_id, developer, asset_id, share_price,
                listed_share_amount, sold_share_amount, reserved_share_amount,
                tax_paid_by_developer, tax_bps, marketplace_fee_bps, investor_fee_bps,
                max_ownership_bps, listing_expiry, claiming_time, claim_deadline,
                legal_process_time, lawyer_voting_time, min_voting_quorum_bps,
                position_count, legal_deadline, deposit, developer_lawyer,
                developer_lawyer_costs, developer_lawyer_doc_status,
                developer_lawyer_documents_hash, spv_lawyer, spv_lawyer_costs,
                spv_lawyer_doc_status, spv_lawyer_documents_hash, second_attempt,
                developer_engaged, spv_costs_due, spv_costs_payee, collected,
                spv_election_expiry, spv_election_candidate_count, spv_election_round,
                status, bump)
           VALUES ($1, $8, 1000000, $9, $2, $10, 55, 10, 0, 0,
                   FALSE, 0, 0, 0, 10000, 0, 0, 0, 0, 0, 0,
                   0, 0, 0, $3, 0, 'PENDING', $4, $5, 0,
                   'PENDING', $6, FALSE, FALSE, 0, $7, '[]'::jsonb,
                   0, 0, 0, $11, 254)"#,
    )
    .bind(pubkey)
    .bind(vec![13u8; 32]) // developer
    .bind(vec![14u8; 32]) // developer_lawyer
    .bind(vec![0u8; 32]) // developer_lawyer_documents_hash
    .bind(vec![15u8; 32]) // spv_lawyer
    .bind(vec![0u8; 32]) // spv_lawyer_documents_hash
    .bind(vec![16u8; 32]) // spv_costs_payee
    .bind(slot)
    .bind(listing_id)
    .bind(asset_id)
    .bind(status)
    .execute(pool)
    .await
    .expect("seed marketplace_listing");
}

/// One `LISTED` marketplace_listing row pointing at asset_id 7 (the join key the
/// `listings` resolver uses to attach `propertyAsset`).
async fn seed_listing(pool: &PgPool, pubkey: &[u8]) {
    seed_listing_row(pool, pubkey, 7, 7, 101, "LISTED").await;
}

/// The columns the ADR-39 feed-state partition reads, per row: the feed tests seed one
/// listing per state with these and leave every other column at a valid default.
struct ListingStateSeed {
    pubkey: [u8; 32],
    listing_id: i64,
    slot: i64,
    status: &'static str,
    listed: i64,
    sold: i64,
    reserved: i64,
    listing_expiry: i64,
    claim_deadline: i64,
}

async fn seed_listing_state(pool: &PgPool, seed: &ListingStateSeed) {
    sqlx::query(
        r#"INSERT INTO marketplace_listing
               (pubkey, slot, lamports, listing_id, developer, asset_id, share_price,
                listed_share_amount, sold_share_amount, reserved_share_amount,
                tax_paid_by_developer, tax_bps, marketplace_fee_bps, investor_fee_bps,
                max_ownership_bps, listing_expiry, claiming_time, claim_deadline,
                legal_process_time, lawyer_voting_time, min_voting_quorum_bps,
                position_count, legal_deadline, deposit, developer_lawyer,
                developer_lawyer_costs, developer_lawyer_doc_status,
                developer_lawyer_documents_hash, spv_lawyer, spv_lawyer_costs,
                spv_lawyer_doc_status, spv_lawyer_documents_hash, second_attempt,
                developer_engaged, spv_costs_due, spv_costs_payee, collected,
                spv_election_expiry, spv_election_candidate_count, spv_election_round,
                status, bump)
           VALUES ($1, $8, 1000000, $9, $2, $10, 55, $12, $13, $14,
                   FALSE, 0, 0, 0, 10000, $15, 0, $16, 0, 0, 0,
                   0, 0, 0, $3, 0, 'PENDING', $4, $5, 0,
                   'PENDING', $6, FALSE, FALSE, 0, $7, '[]'::jsonb,
                   0, 0, 0, $11, 254)"#,
    )
    .bind(seed.pubkey.as_slice())
    .bind(vec![13u8; 32]) // developer
    .bind(vec![14u8; 32]) // developer_lawyer
    .bind(vec![0u8; 32]) // developer_lawyer_documents_hash
    .bind(vec![15u8; 32]) // spv_lawyer
    .bind(vec![0u8; 32]) // spv_lawyer_documents_hash
    .bind(vec![16u8; 32]) // spv_costs_payee
    .bind(seed.slot)
    .bind(seed.listing_id)
    .bind(seed.listing_id) // asset_id (dangling is fine: the resolver LEFT JOINs it)
    .bind(seed.status)
    .bind(seed.listed)
    .bind(seed.sold)
    .bind(seed.reserved)
    .bind(seed.listing_expiry)
    .bind(seed.claim_deadline)
    .execute(pool)
    .await
    .expect("seed marketplace_listing");
}

/// ADR-31: the mirrored 720x720 JPEGs are served as `metadata.propertyImageThumbnails`,
/// in `image_index` order — uploaded rows only, and rows past the metadata document's
/// current `propertyImages` array length are dropped as stale (0016).
#[sqlx::test(migrations = "../../migrations")]
async fn mirrored_thumbnails_are_served_through_property_assets(pool: PgPool) {
    let pubkey = vec![9u8; 32];
    seed_asset_with_metadata(
        &pool,
        &pubkey,
        r#"["https://img.example/0.png", "https://img.example/1.png"]"#,
    )
    .await;
    seed_image(&pool, &pubkey, 0, Some("https://cdn.example/t0.jpg")).await;
    seed_image(&pool, &pubkey, 1, Some("https://cdn.example/t1.jpg")).await;
    // Never uploaded: filtered out by the SQL's `thumb_uri IS NOT NULL`.
    seed_image(&pool, &pubkey, 2, None).await;
    // Uploaded but stale (the document now has only two images): dropped by
    // `with_thumbnails`.
    seed_image(&pool, &pubkey, 5, Some("https://cdn.example/stale.jpg")).await;

    let (data, errors) = juniper::execute(
        r#"{
          propertyAssets(first: 5) {
            totalCount
            nodes {
              id
              metadata { propertyImageThumbnails }
            }
          }
        }"#,
        None,
        &schema(),
        &juniper::Variables::new(),
        &context(pool),
    )
    .await
    .expect("GraphQL execute");

    assert!(errors.is_empty(), "{errors:?}");
    let expected_id = bs58::encode(&pubkey).into_string();
    assert_eq!(
        data,
        juniper::graphql_value!({
            "propertyAssets": {
                "totalCount": 1,
                "nodes": [{
                    "id": expected_id,
                    "metadata": {
                        "propertyImageThumbnails": [
                            "https://cdn.example/t0.jpg",
                            "https://cdn.example/t1.jpg",
                        ],
                    },
                }],
            },
        }),
    );
}

/// `null`, not an empty list or an error, while the mirror has uploaded nothing yet —
/// the field's contract for "disabled or still catching up" (ADR-31).
#[sqlx::test(migrations = "../../migrations")]
async fn thumbnails_are_null_until_the_first_upload(pool: PgPool) {
    let pubkey = vec![10u8; 32];
    seed_asset_with_metadata(&pool, &pubkey, r#"["https://img.example/0.png"]"#).await;

    let (data, errors) = juniper::execute(
        r#"{ propertyAssets(first: 5) { nodes { metadata { propertyImageThumbnails } } } }"#,
        None,
        &schema(),
        &juniper::Variables::new(),
        &context(pool),
    )
    .await
    .expect("GraphQL execute");

    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(
        data,
        juniper::graphql_value!({
            "propertyAssets": {
                "nodes": [{ "metadata": { "propertyImageThumbnails": null } }],
            },
        }),
    );
}

/// The same thumbnails reach the `listings` surface (the one the frontend lists
/// properties through) via the shared `listing_from_row!` attachment.
#[sqlx::test(migrations = "../../migrations")]
async fn mirrored_thumbnails_are_served_through_listings(pool: PgPool) {
    let asset_pubkey = vec![11u8; 32];
    seed_asset_with_metadata(&pool, &asset_pubkey, r#"["https://img.example/0.png"]"#).await;
    seed_image(&pool, &asset_pubkey, 0, Some("https://cdn.example/t0.jpg")).await;
    seed_listing(&pool, &[12u8; 32]).await;

    let (data, errors) = juniper::execute(
        r#"{
          listings(first: 5) {
            nodes { propertyAsset { metadata { propertyImageThumbnails } } }
          }
        }"#,
        None,
        &schema(),
        &juniper::Variables::new(),
        &context(pool),
    )
    .await
    .expect("GraphQL execute");

    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(
        data,
        juniper::graphql_value!({
            "listings": {
                "nodes": [{
                    "propertyAsset": {
                        "metadata": {
                            "propertyImageThumbnails": ["https://cdn.example/t0.jpg"],
                        },
                    },
                }],
            },
        }),
    );
}

/// ADR-39: `feedStates` filters the feed by the DERIVED listing states — status +
/// wall clock + availability, one row per partition cell below — the marketplace
/// page's state checkboxes. An empty list matches nothing; no argument, no filter.
/// PENDING_ASSETS is unreachable by every state on purpose (pre-sale rows never
/// belonged to the feed).
#[sqlx::test(migrations = "../../migrations")]
async fn listings_filter_by_feed_state_partitions_every_listing(pool: PgPool) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let future = now + 1_000_000;
    let past = now - 1_000_000;

    #[allow(clippy::too_many_arguments)]
    fn row(
        byte: u8,
        listing_id: i64,
        status: &'static str,
        listed: i64,
        sold: i64,
        reserved: i64,
        expiry: i64,
        claim: i64,
    ) -> ListingStateSeed {
        ListingStateSeed {
            pubkey: [byte; 32],
            listing_id,
            slot: listing_id, // ordering is slot DESC: highest listing id first
            status,
            listed,
            sold,
            reserved,
            listing_expiry: expiry,
            claim_deadline: claim,
        }
    }

    let seeds = [
        row(51, 51, "LISTED", 10, 2, 1, future, 0), // FOR_SALE
        row(52, 52, "LISTED", 10, 2, 1, past, 0),   // EXPIRED (sale window lapsed, uncranked)
        row(53, 53, "LISTED", 10, 6, 4, future, 0), // WAITING_FOR_CLAIM (fully committed, SPV pending)
        row(54, 54, "LISTED", 10, 6, 4, future, future), // WAITING_FOR_CLAIM (claim window open)
        row(55, 55, "LISTED", 10, 6, 4, future, past), // EXPIRED (claim period lapsed, unclaimed)
        row(56, 56, "SOLD_OUT", 10, 10, 0, past, past), // SOLD_OUT
        row(57, 57, "LEGAL", 10, 10, 0, past, past), // SOLD_OUT (legal/settlement phase)
        row(58, 58, "FINALIZED", 10, 10, 0, past, past), // FINALIZED
        row(59, 59, "CANCELLED", 10, 2, 1, future, 0), // CANCELLED_OR_REFUNDING
        row(60, 60, "REFUNDING", 10, 6, 4, past, past), // CANCELLED_OR_REFUNDING
        row(61, 61, "EXPIRED", 10, 2, 1, past, 0),  // EXPIRED (cranked on chain)
        row(62, 62, "PENDING_ASSETS", 10, 0, 0, future, 0), // unreachable by every state
    ];
    for seed in &seeds {
        seed_listing_state(&pool, seed).await;
    }

    // One query per state, asserting the exact member set (slot DESC = id DESC).
    let cases: [(&str, &[i64]); 6] = [
        ("FOR_SALE", &[51]),
        ("WAITING_FOR_CLAIM", &[54, 53]),
        ("SOLD_OUT", &[57, 56]),
        ("EXPIRED", &[61, 55, 52]),
        ("FINALIZED", &[58]),
        ("CANCELLED_OR_REFUNDING", &[60, 59]),
    ];

    for (state, expected_ids) in cases {
        let query = format!(
            r#"{{ listings(feedStates: [{state}], first: 20) {{
                    totalCount
                    nodes {{ listingId }}
                }} }}"#
        );
        let (data, errors) = juniper::execute(
            &query,
            None,
            &schema(),
            &juniper::Variables::new(),
            &context(pool.clone()),
        )
        .await
        .expect("GraphQL execute");

        assert!(errors.is_empty(), "{state}: {errors:?}");

        // graphql_value! cannot nest a computed Vec, so assert the decoded shape instead:
        // the node's listingId list, in order, plus the count.
        let listings = data
            .as_object_value()
            .and_then(|o| o.get_field_value("listings"))
            .and_then(|l| l.as_object_value())
            .expect("listings object");
        let ids: Vec<String> = listings
            .get_field_value("nodes")
            .and_then(|n| n.as_list_value())
            .expect("nodes list")
            .iter()
            .map(|n| {
                n.as_object_value()
                    .and_then(|o| o.get_field_value("listingId"))
                    .and_then(|v| v.as_scalar_value::<String>())
                    .expect("listingId string")
                    .clone()
            })
            .collect();
        let total = listings
            .get_field_value("totalCount")
            .and_then(|v| v.as_scalar_value::<i32>())
            .copied();

        let expected: Vec<String> = expected_ids.iter().map(|id| id.to_string()).collect();
        assert_eq!(ids, expected, "feed state {state}");
        assert_eq!(total, Some(expected_ids.len() as i32), "feed state {state}");
    }

    // Every state checked: the whole active feed except the PENDING_ASSETS row.
    let (data, errors) = juniper::execute(
        r#"{ listings(feedStates: [FOR_SALE, WAITING_FOR_CLAIM, SOLD_OUT, EXPIRED, FINALIZED, CANCELLED_OR_REFUNDING], first: 20) { totalCount } }"#,
        None,
        &schema(),
        &juniper::Variables::new(),
        &context(pool.clone()),
    )
    .await
    .expect("GraphQL execute");
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(
        data,
        juniper::graphql_value!({ "listings": { "totalCount": 11 } }),
    );

    // The "every state checkbox off" case: nothing matches.
    let (data, errors) = juniper::execute(
        r#"{ listings(feedStates: [], first: 20) { totalCount } }"#,
        None,
        &schema(),
        &juniper::Variables::new(),
        &context(pool.clone()),
    )
    .await
    .expect("GraphQL execute");
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(
        data,
        juniper::graphql_value!({ "listings": { "totalCount": 0 } }),
    );

    // No argument: the filter is off, every active listing comes back (12 rows).
    let (data, errors) = juniper::execute(
        r#"{ listings(first: 20) { totalCount } }"#,
        None,
        &schema(),
        &juniper::Variables::new(),
        &context(pool.clone()),
    )
    .await
    .expect("GraphQL execute");
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(
        data,
        juniper::graphql_value!({ "listings": { "totalCount": 12 } }),
    );

    // The first cut's plain multi-status filter stays for the clients built against it:
    // raw chain statuses, no wall-clock overlay - all five LISTED rows, whatever their
    // deadlines.
    let (data, errors) = juniper::execute(
        r#"{ listings(statuses: [LISTED], first: 20) { totalCount } }"#,
        None,
        &schema(),
        &juniper::Variables::new(),
        &context(pool),
    )
    .await
    .expect("GraphQL execute");
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(
        data,
        juniper::graphql_value!({ "listings": { "totalCount": 5 } }),
    );
}
