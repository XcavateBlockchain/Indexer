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

/// The columns the ADR-43 lawyer-directory filters read, per row: the two lawyer seats
/// (the all-zero vacant-seat sentinel = unassigned), the raw chain status and
/// `legal_deadline` (0 = unset). `listed`/`sold` seed as 10/10 so the row is a plausible
/// SOLD_OUT; every other column takes a valid default, `asset_id` dangles (LEFT JOIN).
struct LawyerListingSeed {
    pubkey: [u8; 32],
    listing_id: i64,
    slot: i64,
    status: &'static str,
    legal_deadline: i64,
    developer_lawyer: [u8; 32],
    spv_lawyer: [u8; 32],
}

async fn seed_lawyer_listing(pool: &PgPool, seed: &LawyerListingSeed) {
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
           VALUES ($1, $3, 1000000, $4, $2, $4, 55, 10, 10, 0,
                   FALSE, 0, 0, 0, 10000, 0, 0, 0, 0, 0, 0,
                   0, $8, 0, $5, 0, 'PENDING', $6, $7, 0,
                   'PENDING', $6, FALSE, FALSE, 0, $2, '[]'::jsonb,
                   0, 0, 0, $9, 254)"#,
    )
    .bind(seed.pubkey.as_slice())
    .bind(vec![13u8; 32]) // developer (also spv_costs_payee — unread by these tests)
    .bind(seed.slot)
    .bind(seed.listing_id) // listing_id and asset_id (dangling is fine: LEFT JOIN)
    .bind(seed.developer_lawyer.as_slice())
    .bind(vec![0u8; 32]) // lawyer_documents_hash (both seats)
    .bind(seed.spv_lawyer.as_slice())
    .bind(seed.legal_deadline)
    .bind(seed.status)
    .execute(pool)
    .await
    .expect("seed marketplace_listing");
}

/// One `sold_out_notifications` row (0018): the durable "this listing sold out" record the
/// `assignableTo` vacancy feed orders by (`slot` = the coordinate of the upsert that first
/// showed the listing SOLD_OUT, NOT the listing's own mirror slot).
async fn seed_sold_out_notification(
    pool: &PgPool,
    listing_pubkey: &[u8],
    listing_id: i64,
    slot: i64,
) {
    sqlx::query(
        r#"INSERT INTO sold_out_notifications (event_id, listing_id, asset_id, slot)
           VALUES ($1, $2, $2, $3)"#,
    )
    .bind(format!(
        "listing_sold_out:{}",
        bs58::encode(listing_pubkey).into_string()
    ))
    .bind(listing_id)
    .bind(slot)
    .execute(pool)
    .await
    .expect("seed sold_out_notifications");
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

// ============================================================================================
// ADR-43: the lawyer-directory filters — `assignedLawyer` (the "Under Instruction" list) and
// `assignableTo` (the "Newly listed" vacancy feed). Both take a base58 pubkey and compose by
// AND with every existing filter; absent = filter off.
// ============================================================================================

/// Decode the `listings { totalCount, nodes { listingId } }` response shape the lawyer
/// filter tests assert on: the ordered id list plus the count.
fn listings_page(data: &juniper::Value<juniper::DefaultScalarValue>) -> (Vec<String>, i32) {
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
        .copied()
        .expect("totalCount int");
    (ids, total)
}

async fn run_listings_query(pool: &PgPool, query: &str) -> (Vec<String>, i32) {
    let (data, errors) = juniper::execute(
        query,
        None,
        &schema(),
        &juniper::Variables::new(),
        &context(pool.clone()),
    )
    .await
    .expect("GraphQL execute");
    assert!(errors.is_empty(), "{errors:?}");
    listings_page(&data)
}

/// `assignedLawyer` matches a lawyer sitting in EITHER seat — developer or SPV — and
/// nothing else; the all-zero vacant seat is nobody's key.
#[sqlx::test(migrations = "../../migrations")]
async fn listings_filter_by_assigned_lawyer_matches_both_slots(pool: PgPool) {
    let lawyer_a = [20u8; 32];
    let lawyer_b = [21u8; 32];
    let vacant = [0u8; 32];
    let seeds = [
        LawyerListingSeed {
            pubkey: [1; 32],
            listing_id: 1,
            slot: 101,
            status: "LISTED",
            legal_deadline: 0,
            developer_lawyer: lawyer_a,
            spv_lawyer: lawyer_b,
        },
        LawyerListingSeed {
            pubkey: [2; 32],
            listing_id: 2,
            slot: 102,
            status: "SOLD_OUT",
            legal_deadline: 0,
            developer_lawyer: lawyer_b,
            spv_lawyer: lawyer_a,
        },
        LawyerListingSeed {
            pubkey: [3; 32],
            listing_id: 3,
            slot: 103,
            status: "LISTED",
            legal_deadline: 0,
            developer_lawyer: vacant,
            spv_lawyer: vacant,
        },
    ];
    for seed in &seeds {
        seed_lawyer_listing(&pool, seed).await;
    }

    let (ids, total) = run_listings_query(
        &pool,
        &format!(
            r#"{{ listings(assignedLawyer: "{}", first: 10) {{ totalCount nodes {{ listingId }} }} }}"#,
            b58key(20),
        ),
    )
    .await;
    // Default ordering (slot DESC): L2 then L1; the all-vacant L3 stays out.
    assert_eq!(ids, ["2", "1"]);
    assert_eq!(total, 2);
}

/// The `assignableTo` vacancy feed: SOLD_OUT rows with the legal window still open
/// (`legal_deadline` unset or in the future) and at least one seat vacant. Both seats
/// filled, not-yet-sold-out, and lapsed-deadline rows all stay out.
#[sqlx::test(migrations = "../../migrations")]
async fn listings_filter_by_assignable_to_returns_vacant_sold_out_only(pool: PgPool) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let future = now + 1_000_000;
    let past = now - 1_000_000;
    let vacant = [0u8; 32];
    let other = [31u8; 32];
    let third = [32u8; 32];
    // The asking lawyer sits in no seat here (exclusion is the next test's subject).
    let seeds = [
        LawyerListingSeed {
            pubkey: [10; 32],
            listing_id: 10,
            slot: 110,
            status: "SOLD_OUT",
            legal_deadline: 0, // unset = window open
            developer_lawyer: vacant,
            spv_lawyer: other,
        },
        LawyerListingSeed {
            pubkey: [11; 32],
            listing_id: 11,
            slot: 111,
            status: "SOLD_OUT",
            legal_deadline: future,
            developer_lawyer: other,
            spv_lawyer: vacant,
        },
        LawyerListingSeed {
            pubkey: [12; 32],
            listing_id: 12,
            slot: 112,
            status: "SOLD_OUT",
            legal_deadline: future,
            developer_lawyer: other,
            spv_lawyer: third, // both seats filled
        },
        LawyerListingSeed {
            pubkey: [13; 32],
            listing_id: 13,
            slot: 113,
            status: "LISTED", // not sold out yet
            legal_deadline: 0,
            developer_lawyer: vacant,
            spv_lawyer: vacant,
        },
        LawyerListingSeed {
            pubkey: [14; 32],
            listing_id: 14,
            slot: 114,
            status: "SOLD_OUT",
            legal_deadline: past, // legal window lapsed
            developer_lawyer: vacant,
            spv_lawyer: vacant,
        },
    ];
    for seed in &seeds {
        seed_lawyer_listing(&pool, seed).await;
    }

    let (ids, total) = run_listings_query(
        &pool,
        &format!(
            r#"{{ listings(assignableTo: "{}", first: 10) {{ totalCount nodes {{ listingId }} }} }}"#,
            b58key(30),
        ),
    )
    .await;
    // No notification rows seeded: sold-out recency is all NULLS LAST, so the listing
    // slot tiebreak orders 11 ahead of 10.
    assert_eq!(ids, ["11", "10"]);
    assert_eq!(total, 2);
}

/// A lawyer never sees their OWN listings in the vacancy feed: a seat they already occupy
/// excludes the row for them, but not for a lawyer in neither seat.
#[sqlx::test(migrations = "../../migrations")]
async fn listings_assignable_to_excludes_lawyers_own_listings(pool: PgPool) {
    let lawyer_a = [20u8; 32];
    let vacant = [0u8; 32];
    seed_lawyer_listing(
        &pool,
        &LawyerListingSeed {
            pubkey: [20; 32],
            listing_id: 20,
            slot: 120,
            status: "SOLD_OUT",
            legal_deadline: 0,
            developer_lawyer: lawyer_a, // A holds the developer seat; SPV seat is free
            spv_lawyer: vacant,
        },
    )
    .await;

    let (_ids, total) = run_listings_query(
        &pool,
        &format!(
            r#"{{ listings(assignableTo: "{}", first: 10) {{ totalCount nodes {{ listingId }} }} }}"#,
            b58key(20),
        ),
    )
    .await;
    assert_eq!(total, 0);

    let (ids, total) = run_listings_query(
        &pool,
        &format!(
            r#"{{ listings(assignableTo: "{}", first: 10) {{ totalCount nodes {{ listingId }} }} }}"#,
            b58key(21),
        ),
    )
    .await;
    assert_eq!(ids, ["20"]);
    assert_eq!(total, 1);
}

/// With `assignableTo` the ordering switches to SOLD-OUT RECENCY: the
/// `sold_out_notifications` coordinate slot DESC, NULLS LAST (a listing whose sold-out
/// event was never recorded sorts behind every recorded one), then the listing mirror
/// slot and pubkey as tiebreaks. The default ordering is untouched without the argument.
#[sqlx::test(migrations = "../../migrations")]
async fn listings_assignable_to_orders_by_sold_out_slot_desc(pool: PgPool) {
    let vacant = [0u8; 32];
    let other = [31u8; 32];
    let seed = |byte: u8, listing_id: i64, slot: i64| LawyerListingSeed {
        pubkey: [byte; 32],
        listing_id,
        slot,
        status: "SOLD_OUT",
        legal_deadline: 0,
        developer_lawyer: vacant,
        spv_lawyer: other,
    };
    // Mirror slots deliberately disagree with the sold-out slots: a plain slot-DESC
    // ordering would return 32, 30, 31.
    seed_lawyer_listing(&pool, &seed(40, 30, 300)).await;
    seed_lawyer_listing(&pool, &seed(41, 31, 200)).await;
    seed_lawyer_listing(&pool, &seed(42, 32, 400)).await; // never recorded sold-out
    seed_sold_out_notification(&pool, &[40u8; 32], 30, 100).await;
    seed_sold_out_notification(&pool, &[41u8; 32], 31, 200).await;

    let (ids, total) = run_listings_query(
        &pool,
        &format!(
            r#"{{ listings(assignableTo: "{}", first: 10) {{ totalCount nodes {{ listingId }} }} }}"#,
            b58key(30),
        ),
    )
    .await;
    assert_eq!(ids, ["31", "30", "32"]);
    assert_eq!(total, 3);

    // No argument: the default `slot DESC, pubkey ASC` ordering, notification-free.
    let (ids, total) = run_listings_query(
        &pool,
        r#"{ listings(first: 10) { totalCount nodes { listingId } } }"#,
    )
    .await;
    assert_eq!(ids, ["32", "30", "31"]);
    assert_eq!(total, 3);
}

// ============================================================================================
// ADR-40 derived reads: `claimableIncome` / `pendingFinalizations`. The gate boundary
// semantics (inclusive expiries, the tie rule, the clamps) are pinned by
// `programs::derived`'s pure-core unit tests with a frozen clock; these DB-seeded tests
// prove the wiring: rows land in the right snapshots, the JSONB columns parse, and the
// results surface through GraphQL.
// ============================================================================================

fn b58key(byte: u8) -> String {
    bs58::encode([byte; 32]).into_string()
}

/// One active `property_income` row (the property's rental income ledger; `streams` is the
/// migration-0012 JSONB shape, `per_share` a u128 decimal string).
async fn seed_income_ledger(pool: &PgPool, asset_id: i64, streams: &str) {
    sqlx::query(
        r#"INSERT INTO property_income (pubkey, slot, lamports, asset_id, streams, rent_payer, bump)
           VALUES ($1, 100, 1000000, $2, $3::jsonb, $4, 255)"#,
    )
    .bind(vec![50u8; 32])
    .bind(asset_id)
    .bind(streams)
    .bind(vec![51u8; 32])
    .execute(pool)
    .await
    .expect("seed property_income");
}

/// One active `property_income_checkpoint` row (`entries[i]` tracking `streams[i]`).
async fn seed_income_checkpoint(pool: &PgPool, asset_id: i64, owner: &[u8], entries: &str) {
    sqlx::query(
        r#"INSERT INTO property_income_checkpoint
               (pubkey, slot, lamports, asset_id, owner, entries, rent_payer, bump)
           VALUES ($1, 100, 1000000, $2, $3, $4::jsonb, $5, 255)"#,
    )
    .bind(vec![52u8; 32])
    .bind(asset_id)
    .bind(owner)
    .bind(entries)
    .bind(vec![53u8; 32])
    .execute(pool)
    .await
    .expect("seed property_income_checkpoint");
}

/// One active `marketplace_share_holding` row (the holder's current shares).
async fn seed_share_holding(pool: &PgPool, asset_id: i64, owner: &[u8], amount: i64) {
    sqlx::query(
        r#"INSERT INTO marketplace_share_holding (pubkey, slot, lamports, asset_id, owner, amount, bump)
           VALUES ($1, 100, 1000000, $2, $3, $4, 255)"#,
    )
    .bind(vec![54u8; 32])
    .bind(asset_id)
    .bind(owner)
    .bind(amount)
    .execute(pool)
    .await
    .expect("seed marketplace_share_holding");
}

/// One active `property_letting` row with caller-chosen governance counters and election
/// state (the crank gates read exactly these columns).
#[allow(clippy::too_many_arguments)]
async fn seed_letting(
    pool: &PgPool,
    asset_id: i64,
    active_proposal: i64,
    active_challenge: i64,
    election_round: i64,
    election_expiry: i64,
    election_candidate_count: i64,
) {
    sqlx::query(
        r#"INSERT INTO property_letting
               (pubkey, slot, lamports, asset_id, agent, election_expiry,
                election_candidate_count, election_round, election_quorum_bps,
                governance_active_proposal, governance_active_challenge, rent_payer, bump)
           VALUES ($1, 100, 1000000, $2, $3, $4, $5, $6, 0, $7, $8, $9, 255)"#,
    )
    .bind(vec![55u8; 32])
    .bind(asset_id)
    .bind(vec![56u8; 32]) // agent
    .bind(election_expiry)
    .bind(election_candidate_count)
    .bind(election_round)
    .bind(active_proposal)
    .bind(active_challenge)
    .bind(vec![57u8; 32]) // rent_payer
    .execute(pool)
    .await
    .expect("seed property_letting");
}

/// One open `property_proposal` row.
async fn seed_proposal(pool: &PgPool, asset_id: i64, id: i64, expiry: i64, rent_payer: &[u8]) {
    sqlx::query(
        r#"INSERT INTO property_proposal
               (pubkey, slot, lamports, asset_id, id, proposer, amount, details_hash, expiry,
                tally_yes, tally_no, tally_abstain, quorum_bps, threshold_bps, rent_payer, bump)
           VALUES ($1, 100, 1000000, $2, $3, $4, 5, $5, $6,
                   0, 0, 0, 0, 0, $7, 255)"#,
    )
    .bind(vec![58u8; 32])
    .bind(asset_id)
    .bind(id)
    .bind(vec![59u8; 32]) // proposer
    .bind(vec![0u8; 32]) // details_hash
    .bind(expiry)
    .bind(rent_payer)
    .execute(pool)
    .await
    .expect("seed property_proposal");
}

/// One open `property_challenge` row.
#[allow(clippy::too_many_arguments)]
async fn seed_challenge(
    pool: &PgPool,
    asset_id: i64,
    id: i64,
    expiry: i64,
    rent_payer: &[u8],
    challenger: &[u8],
    struck_agent: &[u8],
) {
    sqlx::query(
        r#"INSERT INTO property_challenge
               (pubkey, slot, lamports, asset_id, id, challenger, agent, deposit, expiry,
                tally_yes, tally_no, tally_abstain, quorum_bps, rent_payer, bump)
           VALUES ($1, 100, 1000000, $2, $3, $4, $5, 10, $6,
                   0, 0, 0, 0, $7, 255)"#,
    )
    .bind(vec![60u8; 32])
    .bind(asset_id)
    .bind(id)
    .bind(challenger)
    .bind(struck_agent)
    .bind(expiry)
    .bind(rent_payer)
    .execute(pool)
    .await
    .expect("seed property_challenge");
}

/// One `property_agent_candidacy` row; `closed_at_slot: Some(1)` seeds a closed candidacy
/// (the winner fold must skip it).
#[allow(clippy::too_many_arguments)]
async fn seed_agent_candidacy(
    pool: &PgPool,
    pubkey: &[u8],
    asset_id: i64,
    round: i64,
    slot: i64,
    agent: &[u8],
    vote_power: i64,
    closed: bool,
) {
    sqlx::query(
        r#"INSERT INTO property_agent_candidacy
               (pubkey, slot, lamports, closed_at_slot, asset_id, round, agent, vote_power,
                rent_payer, bump)
           VALUES ($1, $2, 1000000, CASE WHEN $3 THEN 1 ELSE NULL END, $4, $5, $6, $7, $8, 255)"#,
    )
    .bind(pubkey)
    .bind(slot)
    .bind(closed)
    .bind(asset_id)
    .bind(round)
    .bind(agent)
    .bind(vote_power)
    .bind(vec![63u8; 32]) // rent_payer
    .execute(pool)
    .await
    .expect("seed property_agent_candidacy");
}

/// One open `marketplace_lawyer_candidacy` row.
#[allow(clippy::too_many_arguments)]
async fn seed_lawyer_candidacy(
    pool: &PgPool,
    pubkey: &[u8],
    listing_id: i64,
    round: i64,
    slot: i64,
    lawyer: &[u8],
    vote_power: i64,
) {
    sqlx::query(
        r#"INSERT INTO marketplace_lawyer_candidacy
               (pubkey, slot, lamports, listing_id, round, lawyer, costs, vote_power,
                rent_payer, bump)
           VALUES ($1, $2, 1000000, $3, $4, $5, 0, $6, $7, 255)"#,
    )
    .bind(pubkey)
    .bind(slot)
    .bind(listing_id)
    .bind(round)
    .bind(lawyer)
    .bind(vote_power)
    .bind(vec![64u8; 32]) // rent_payer
    .execute(pool)
    .await
    .expect("seed marketplace_lawyer_candidacy");
}

/// The marketplace singleton config row (only the mints/treasury the crank reads vary).
async fn seed_marketplace_config(pool: &PgPool, xcav_mint: &[u8], treasury: &[u8], accepted: &str) {
    sqlx::query(
        r#"INSERT INTO marketplace_config
               (pubkey, slot, lamports, authority, xcav_mint, treasury, rent_collector,
                accepted_payment_mints, listing_deposit, lawyer_deposit, min_property_shares,
                max_property_shares, marketplace_fee_bps, investor_fee_bps, max_ownership_bps,
                claiming_time, legal_process_time, lawyer_voting_time, min_voting_quorum_bps,
                next_listing_id, bump)
           VALUES ($1, 100, 1000000, $2, $3, $4, $5, $6::jsonb,
                   0, 0, 1, 1000, 0, 0, 0, 0, 0, 0, 0, 1, 255)"#,
    )
    .bind(vec![65u8; 32])
    .bind(vec![66u8; 32]) // authority
    .bind(xcav_mint)
    .bind(treasury)
    .bind(vec![67u8; 32]) // rent_collector
    .bind(accepted)
    .execute(pool)
    .await
    .expect("seed marketplace_config");
}

/// The columns the ADR-40 crank gates read off `marketplace_listing`, per row; every other
/// column takes a valid constant (same approach as `ListingStateSeed`).
#[allow(clippy::too_many_arguments)]
struct CrankListingSeed {
    pubkey: [u8; 32],
    listing_id: i64,
    asset_id: i64,
    status: &'static str,
    legal_deadline: i64,
    legal_process_time: i64,
    developer_lawyer_doc_status: &'static str,
    spv_lawyer_doc_status: &'static str,
    spv_lawyer: [u8; 32],
    spv_election_round: i64,
    spv_election_expiry: i64,
    spv_election_candidate_count: i64,
    collected: String,
}

async fn seed_crank_listing(pool: &PgPool, seed: &CrankListingSeed) {
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
           VALUES ($1, 100, 1000000, $2, $3, $4, 55, 10, 10, 0,
                   FALSE, 0, 0, 0, 10000, 0, 0, 0, $5, 0, 0,
                   0, $6, 0, $7, 0, $8, $9, $10, 0,
                   $11, $12, FALSE, FALSE, 0, $13, $14::jsonb,
                   $15, $16, $17, $18, 254)"#,
    )
    .bind(seed.pubkey.as_slice())
    .bind(seed.listing_id)
    .bind(vec![13u8; 32]) // developer
    .bind(seed.asset_id)
    .bind(seed.legal_process_time)
    .bind(seed.legal_deadline)
    .bind(vec![14u8; 32]) // developer_lawyer
    .bind(seed.developer_lawyer_doc_status)
    .bind(vec![0u8; 32]) // developer_lawyer_documents_hash
    .bind(seed.spv_lawyer.as_slice())
    .bind(seed.spv_lawyer_doc_status)
    .bind(vec![0u8; 32]) // spv_lawyer_documents_hash
    .bind(vec![16u8; 32]) // spv_costs_payee
    .bind(seed.collected.as_str())
    .bind(seed.spv_election_expiry)
    .bind(seed.spv_election_candidate_count)
    .bind(seed.spv_election_round)
    .bind(seed.status)
    .execute(pool)
    .await
    .expect("seed marketplace_listing");
}

/// One `marketplace_property_asset` row (only the region matters to the crank gates).
async fn seed_asset_region(pool: &PgPool, pubkey: &[u8], asset_id: i64, region_id: i32) {
    sqlx::query(
        r#"INSERT INTO marketplace_property_asset
               (pubkey, slot, lamports, asset_id, name, metadata_uri, share_mint,
                region_id, location, share_amount, spv_created, finalized, holder_count, bump)
           VALUES ($1, 100, 1000000, $2, 'Test House', 'https://md.example/7.json', $3,
                   $4, $5, 1000, TRUE, FALSE, 3, 255)"#,
    )
    .bind(pubkey)
    .bind(asset_id)
    .bind(vec![8u8; 32])
    .bind(region_id)
    .bind(b"London".as_slice())
    .execute(pool)
    .await
    .expect("seed marketplace_property_asset");
}

/// `claimableIncome` settles each stream against the holder's checkpoint at their current
/// holding: index-paired entries, a short entry list reading as zero cursors, u128
/// per_share values beyond u64::MAX staying exact, and base-unit amounts shipped as
/// decimal strings.
#[sqlx::test(migrations = "../../migrations")]
async fn claimable_income_settles_streams_against_checkpoint_and_holding(pool: PgPool) {
    let owner = vec![70u8; 32];
    let mint_a = b58key(71);
    let mint_b = b58key(72);
    let big = (u64::MAX as u128) + 1_000;
    seed_income_ledger(
        &pool,
        7,
        &format!(
            r#"[{{"mint": "{mint_a}", "per_share": "{big}", "dust": 0}},
                {{"mint": "{mint_b}", "per_share": "500", "dust": 1}}]"#
        ),
    )
    .await;
    // One entry for two streams: stream B's missing entry reads as a zero cursor.
    seed_income_checkpoint(
        &pool,
        7,
        &owner,
        &format!(r#"[{{"per_share": "{}", "pending": 42}}]"#, u64::MAX),
    )
    .await;
    seed_share_holding(&pool, 7, &owner, 3).await;

    let query = format!(
        r#"{{ claimableIncome(owner: "{}", assetId: "7") {{
                mint claimableBaseUnits pendingBaseUnits
            }} }}"#,
        bs58::encode(&owner).into_string()
    );
    let (data, errors) = juniper::execute(
        &query,
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
            "claimableIncome": [
                // (big - u64::MAX) * 3 + 42 = 3042; pending rides along verbatim.
                { "mint": mint_a, "claimableBaseUnits": "3042", "pendingBaseUnits": "42" },
                // (500 - 0) * 3 + 0: the missing entry accrued from the stream's start.
                { "mint": mint_b, "claimableBaseUnits": "1500", "pendingBaseUnits": "0" },
            ],
        }),
    );
}

/// No checkpoint row at all = "held since before every stream opened": full accrual from
/// each stream's start (`sync_entries`' zeroed entries).
#[sqlx::test(migrations = "../../migrations")]
async fn claimable_income_full_accrual_without_checkpoint(pool: PgPool) {
    let owner = vec![70u8; 32];
    let mint_a = b58key(71);
    seed_income_ledger(
        &pool,
        7,
        &format!(r#"[{{"mint": "{mint_a}", "per_share": "700", "dust": 0}}]"#),
    )
    .await;
    seed_share_holding(&pool, 7, &owner, 2).await;

    let query = format!(
        r#"{{ claimableIncome(owner: "{}", assetId: "7") {{
                mint claimableBaseUnits pendingBaseUnits
            }} }}"#,
        bs58::encode(&owner).into_string()
    );
    let (data, errors) = juniper::execute(
        &query,
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
            "claimableIncome": [
                { "mint": mint_a, "claimableBaseUnits": "1400", "pendingBaseUnits": "0" },
            ],
        }),
    );
}

/// No active holding row = zero shares (on-chain a closed holding reads the same way):
/// only the banked pending stays claimable.
#[sqlx::test(migrations = "../../migrations")]
async fn claimable_income_zero_shares_keep_only_banked_pending(pool: PgPool) {
    let owner = vec![70u8; 32];
    let mint_a = b58key(71);
    seed_income_ledger(
        &pool,
        7,
        &format!(r#"[{{"mint": "{mint_a}", "per_share": "700", "dust": 0}}]"#),
    )
    .await;
    seed_income_checkpoint(&pool, 7, &owner, r#"[{"per_share": "0", "pending": 9}]"#).await;

    let query = format!(
        r#"{{ claimableIncome(owner: "{}", assetId: "7") {{
                mint claimableBaseUnits pendingBaseUnits
            }} }}"#,
        bs58::encode(&owner).into_string()
    );
    let (data, errors) = juniper::execute(
        &query,
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
            "claimableIncome": [
                { "mint": mint_a, "claimableBaseUnits": "9", "pendingBaseUnits": "9" },
            ],
        }),
    );
}

/// No income ledger (and nothing else) for the asset: the empty list, not an error.
#[sqlx::test(migrations = "../../migrations")]
async fn claimable_income_empty_without_an_income_ledger(pool: PgPool) {
    let owner = vec![70u8; 32];
    let query = format!(
        r#"{{ claimableIncome(owner: "{}", assetId: "7") {{
                mint claimableBaseUnits pendingBaseUnits
            }} }}"#,
        bs58::encode(&owner).into_string()
    );
    let (data, errors) = juniper::execute(
        &query,
        None,
        &schema(),
        &juniper::Variables::new(),
        &context(pool),
    )
    .await
    .expect("GraphQL execute");

    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(data, juniper::graphql_value!({ "claimableIncome": [] }));
}

/// Nothing seeded at all: every crank gate is off, the flags false — one object, all null.
#[sqlx::test(migrations = "../../migrations")]
async fn pending_finalizations_on_an_empty_database_turns_every_gate_off(pool: PgPool) {
    let (data, errors) = juniper::execute(
        r#"{ pendingFinalizations(assetId: "7", listingId: "7") {
                proposal { proposalId }
                challenge { challengeId }
                agentElection { round }
                spvElection { round }
                silentVerdictOverdue
                executeDeal { regionId }
            } }"#,
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
            "pendingFinalizations": {
                "proposal": null,
                "challenge": null,
                "agentElection": null,
                "spvElection": null,
                "silentVerdictOverdue": false,
                "executeDeal": null,
            },
        }),
    );
}

/// The property-side gates end to end: an expired active proposal is due (id + rent payer),
/// an unexpired challenge is not, and an expired election round with a tied top reports the
/// crank with a `null` winner — over the CURRENT round's OPEN candidacies only (the stale
/// round-1 row and the closed round-2 row stay out of both the fold and the candidates).
#[sqlx::test(migrations = "../../migrations")]
async fn pending_finalizations_proposal_due_challenge_waiting_election_tied(pool: PgPool) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let past = now - 1_000_000;
    let future = now + 1_000_000;

    seed_letting(&pool, 7, 11, 12, 2, past, 2).await;
    seed_proposal(&pool, 7, 11, past, &[21u8; 32]).await;
    seed_challenge(&pool, 7, 12, future, &[22u8; 32], &[23u8; 32], &[24u8; 32]).await;
    // Current round (2): two candidacies tied at the top, plus one closed row the fold
    // must skip. Stale round 1: a strong row that must not leak into round 2.
    seed_agent_candidacy(&pool, &[1u8; 32], 7, 2, 101, &[61u8; 32], 10, false).await;
    seed_agent_candidacy(&pool, &[2u8; 32], 7, 2, 100, &[62u8; 32], 10, false).await;
    seed_agent_candidacy(&pool, &[3u8; 32], 7, 2, 102, &[64u8; 32], 50, true).await;
    seed_agent_candidacy(&pool, &[4u8; 32], 7, 1, 99, &[63u8; 32], 99, false).await;

    let (data, errors) = juniper::execute(
        r#"{ pendingFinalizations(assetId: "7", listingId: "7") {
                proposal { proposalId rentPayer }
                challenge { challengeId }
                agentElection { winner round candidates }
                spvElection { round }
                silentVerdictOverdue
                executeDeal { regionId }
            } }"#,
        None,
        &schema(),
        &juniper::Variables::new(),
        &context(pool),
    )
    .await
    .expect("GraphQL execute");

    assert!(errors.is_empty(), "{errors:?}");
    let rent_payer = b58key(21);
    let candidate_a = b58key(61);
    let candidate_b = b58key(62);
    assert_eq!(
        data,
        juniper::graphql_value!({
            "pendingFinalizations": {
                "proposal": { "proposalId": "11", "rentPayer": rent_payer },
                "challenge": null,
                "agentElection": {
                    "winner": null, // tie at the top: the seat reopens
                    "round": "2",
                    // slot DESC settles the power tie in the candidates list.
                    "candidates": [candidate_a, candidate_b],
                },
                "spvElection": null,
                "silentVerdictOverdue": false,
                "executeDeal": null,
            },
        }),
    );
}

/// The challenge gate due: id, rent payer, challenger, and the struck agent; the election
/// with a strictly-unique leader reports that winner, strongest first in the candidates.
#[sqlx::test(migrations = "../../migrations")]
async fn pending_finalizations_challenge_due_and_a_unique_election_winner(pool: PgPool) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let past = now - 1_000_000;

    seed_letting(&pool, 7, 0, 12, 2, past, 2).await;
    seed_challenge(&pool, 7, 12, past, &[22u8; 32], &[23u8; 32], &[24u8; 32]).await;
    seed_agent_candidacy(&pool, &[1u8; 32], 7, 2, 101, &[61u8; 32], 7, false).await;
    seed_agent_candidacy(&pool, &[2u8; 32], 7, 2, 100, &[62u8; 32], 10, false).await;

    let (data, errors) = juniper::execute(
        r#"{ pendingFinalizations(assetId: "7", listingId: "7") {
                proposal { proposalId }
                challenge { challengeId rentPayer challenger struckAgent }
                agentElection { winner round candidates }
            } }"#,
        None,
        &schema(),
        &juniper::Variables::new(),
        &context(pool),
    )
    .await
    .expect("GraphQL execute");

    assert!(errors.is_empty(), "{errors:?}");
    let rent_payer = b58key(22);
    let challenger = b58key(23);
    let struck_agent = b58key(24);
    let winner = b58key(62);
    let runner_up = b58key(61);
    assert_eq!(
        data,
        juniper::graphql_value!({
            "pendingFinalizations": {
                "proposal": null,
                "challenge": {
                    "challengeId": "12",
                    "rentPayer": rent_payer,
                    "challenger": challenger,
                    "struckAgent": struck_agent,
                },
                "agentElection": {
                    "winner": winner.clone(),
                    "round": "2",
                    "candidates": [winner.clone(), runner_up],
                },
            },
        }),
    );
}

/// The marketplace-side gates: the expired SPV election over the vacant seat reports its
/// unique winner (summed `vote_power` read off the candidacy rows — no vote-row summing),
/// and the silent-verdict final stretch flags while the developer's lawyer stays silent.
#[sqlx::test(migrations = "../../migrations")]
async fn pending_finalizations_spv_election_and_silent_verdict(pool: PgPool) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let past = now - 1_000_000;

    // SOLD_OUT, developer silent, SPV ruled, seat vacant, election expired: legal_deadline
    // now+100 with legal_process_time 500 puts the final stretch's start at seed time, so
    // the resolver's own (later) `now` sits inside the window.
    seed_crank_listing(
        &pool,
        &CrankListingSeed {
            pubkey: [80u8; 32],
            listing_id: 7,
            asset_id: 7,
            status: "SOLD_OUT",
            legal_deadline: now + 100,
            legal_process_time: 500,
            developer_lawyer_doc_status: "PENDING",
            spv_lawyer_doc_status: "APPROVED",
            spv_lawyer: [0u8; 32], // the vacant-seat sentinel
            spv_election_round: 1,
            spv_election_expiry: past,
            spv_election_candidate_count: 2,
            collected: "[]".to_string(),
        },
    )
    .await;
    seed_lawyer_candidacy(&pool, &[5u8; 32], 7, 1, 100, &[61u8; 32], 5).await;
    seed_lawyer_candidacy(&pool, &[6u8; 32], 7, 1, 101, &[62u8; 32], 9).await;

    let (data, errors) = juniper::execute(
        r#"{ pendingFinalizations(assetId: "7", listingId: "7") {
                agentElection { round }
                spvElection { winner round candidates }
                silentVerdictOverdue
                executeDeal { regionId }
            } }"#,
        None,
        &schema(),
        &juniper::Variables::new(),
        &context(pool),
    )
    .await
    .expect("GraphQL execute");

    assert!(errors.is_empty(), "{errors:?}");
    let winner = b58key(62);
    let runner_up = b58key(61);
    assert_eq!(
        data,
        juniper::graphql_value!({
            "pendingFinalizations": {
                "agentElection": null, // no letting row for this asset
                "spvElection": {
                    "winner": winner.clone(),
                    "round": "1",
                    "candidates": [winner.clone(), runner_up],
                },
                "silentVerdictOverdue": true,
                "executeDeal": null, // SOLD_OUT, not LEGAL
            },
        }),
    );
}

/// `executeDeal` in the LEGAL window: the parties, the region from the asset join, the
/// config's XCAV mint / treasury / accepted mints, and the collected payment mints parsed
/// out of the listing's raw JSON `collected` column.
#[sqlx::test(migrations = "../../migrations")]
async fn pending_finalizations_execute_deal_carries_config_and_collected_mints(pool: PgPool) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let future = now + 1_000_000;
    let mint_a = b58key(71);
    let mint_b = b58key(72);

    seed_crank_listing(
        &pool,
        &CrankListingSeed {
            pubkey: [80u8; 32],
            listing_id: 7,
            asset_id: 7,
            status: "LEGAL",
            legal_deadline: future,
            legal_process_time: 500,
            developer_lawyer_doc_status: "APPROVED",
            spv_lawyer_doc_status: "APPROVED",
            spv_lawyer: [33u8; 32],
            spv_election_round: 0,
            spv_election_expiry: 0,
            spv_election_candidate_count: 0,
            collected: r#"[{"mint": "MINT_A", "funds": 5, "fee": 1, "tax": 1},
                           {"mint": "MINT_B", "funds": 3, "fee": 0, "tax": 0}]"#
                .replace("MINT_A", &mint_a)
                .replace("MINT_B", &mint_b),
        },
    )
    .await;
    seed_asset_region(&pool, &[81u8; 32], 7, 3).await;
    seed_marketplace_config(
        &pool,
        &[41u8; 32],
        &[42u8; 32],
        &format!(r#"["{mint_a}", "{mint_b}"]"#),
    )
    .await;

    let (data, errors) = juniper::execute(
        r#"{ pendingFinalizations(assetId: "7", listingId: "7") {
                silentVerdictOverdue
                executeDeal {
                    regionId developer developerLawyer spvLawyer
                    xcavMint treasury acceptedPaymentMints collectedMints
                }
            } }"#,
        None,
        &schema(),
        &juniper::Variables::new(),
        &context(pool),
    )
    .await
    .expect("GraphQL execute");

    assert!(errors.is_empty(), "{errors:?}");
    let developer = b58key(13);
    let developer_lawyer = b58key(14);
    let spv_lawyer = b58key(33);
    let xcav_mint = b58key(41);
    let treasury = b58key(42);
    assert_eq!(
        data,
        juniper::graphql_value!({
            "pendingFinalizations": {
                "silentVerdictOverdue": false,
                "executeDeal": {
                    "regionId": 3,
                    "developer": developer,
                    "developerLawyer": developer_lawyer,
                    "spvLawyer": spv_lawyer,
                    "xcavMint": xcav_mint,
                    "treasury": treasury,
                    "acceptedPaymentMints": [mint_a.clone(), mint_b.clone()],
                    "collectedMints": [mint_a, mint_b],
                },
            },
        }),
    );
}
