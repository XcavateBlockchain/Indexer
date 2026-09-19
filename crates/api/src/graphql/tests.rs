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

/// One `LISTED` marketplace_listing row pointing at asset_id 7 (the join key the
/// `listings` resolver uses to attach `propertyAsset`). All NOT NULL columns are seeded;
/// the CHECK-constrained TEXT columns take a valid enum value.
async fn seed_listing(pool: &PgPool, pubkey: &[u8]) {
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
           VALUES ($1, 101, 1000000, 7, $2, 7, 55, 10, 0, 0,
                   FALSE, 0, 0, 0, 10000, 0, 0, 0, 0, 0, 0,
                   0, 0, 0, $3, 0, 'PENDING', $4, $5, 0,
                   'PENDING', $6, FALSE, FALSE, 0, $7, '[]'::jsonb,
                   0, 0, 0, 'LISTED', 254)"#,
    )
    .bind(pubkey)
    .bind(vec![13u8; 32]) // developer
    .bind(vec![14u8; 32]) // developer_lawyer
    .bind(vec![0u8; 32]) // developer_lawyer_documents_hash
    .bind(vec![15u8; 32]) // spv_lawyer
    .bind(vec![0u8; 32]) // spv_lawyer_documents_hash
    .bind(vec![16u8; 32]) // spv_costs_payee
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
