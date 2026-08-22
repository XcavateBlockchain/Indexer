//! End-to-end write-path tests: carbon processor -> batcher -> one Postgres transaction.
//!
//! `mapping::tests` proves the decoded-instruction -> row mapping in isolation and
//! `db::tests` proves the SQL. This module covers the seam between them -- the processors and
//! the batched writer -- which is where the ordering, idempotency and slot-guard behaviour
//! actually has to hold at runtime.
//!
//! The account fixtures are **real devnet bytes**, captured with `getProgramAccounts` against
//! `2vVARM46pPD4rcHdbXHnYA4vTGN14q6skQAzsQWcHUxn` on 2026-08-15 (see the Task 3 report for the
//! exact command and the full listing). Using real bytes rather than hand-built structs is the
//! point: it is the only thing in the test suite that would catch a borsh layout or
//! discriminator mismatch between the checked-in IDL and the deployed program. The expected
//! values below were decoded independently of this crate (by hand, from the hex) so the test
//! is not just asserting that the decoder agrees with itself.
//!
//! The account path cannot be exercised by a history crawl -- the RPC transaction crawler is
//! transaction-only -- and the live gRPC account stream only fires when an account actually
//! changes, which on this idle program may be never. That is why the account-state verification
//! lives here, and why the `snapshot` subcommand exists at all.

use std::sync::Arc;

use carbon_core::account::AccountMetadata;
use carbon_core::datasource::AccountDeletion;
use carbon_core::instruction::InstructionDecoder;
use carbon_core::metrics::MetricsCollection;
use carbon_core::processor::Processor;
use carbon_xcavate_whitelist_decoder::accounts::XcavateWhitelistAccount;
use carbon_xcavate_whitelist_decoder::instructions::{
    AssignRole, RemoveRole, XcavateWhitelistInstruction,
};
use carbon_xcavate_whitelist_decoder::types::Role as ChainRole;
use carbon_xcavate_whitelist_decoder::{XcavateWhitelistDecoder, PROGRAM_ID};
use solana_account::Account;
use solana_pubkey::Pubkey;
use sqlx::{PgPool, Row};
use std::str::FromStr;
use tokio_util::sync::CancellationToken;

use crate::batcher::{self, Batcher};
use crate::block_time::BlockTimeResolver;
use crate::pipeline;
use crate::processors::{AccountDeletionProcessor, AccountProcessor, InstructionProcessor};
use crate::test_fixtures::{decoded, instruction_metadata, sig_from, tx_metadata};

// --- real devnet fixtures --------------------------------------------------------------------

const CONFIG_PUBKEY: &str = "Djht1K3NorKGGD4qcUut6xxqUpcXdAdw4XXchZ25i27E";
const CONFIG_LAMPORTS: u64 = 1_405_920;
const CONFIG_DATA_HEX: &str = "9b0caae01efacc8261edeac2df8a832fa780abe6aa5ae9cd806a64fa6eb9f0e4a014bf13b4f36cef00fd0000000000000000000000000000000000000000000000000000000000000000";
/// Bytes 8..40 of `CONFIG_DATA_HEX`, base58-encoded.
const CONFIG_AUTHORITY: &str = "7bGxnDFi3zKLAbgeXtCANcf8MGSYob1EAmoWZY77qjp2";
/// Byte 41 of `CONFIG_DATA_HEX` (byte 40 is the `Option<Pubkey>` tag, `00` = None).
const CONFIG_BUMP: i16 = 253;

const ADMIN_PUBKEY: &str = "GgbAVFmC41aaRnE9yq9xp2xc2oJAuKS4SA8vGyDLmUsn";
const ADMIN_LAMPORTS: u64 = 1_176_240;
const ADMIN_DATA_HEX: &str =
    "f49edc4108490441b3eb8835730bbb1331e04298a9f8f514fcf20e7220d937c497b7ca16af06f9d4fd";
const ADMIN_ADMIN: &str = "D7LHTCvNtG37QsZSphsCTkJhLhg3SfpyjqMBwtfqbvaP";
const ADMIN_BUMP: i16 = 253;

const ROLE_PUBKEY: &str = "2faa6q7HBRsUvazW7KxgRdrLHYepf7SFxrYzY2BwDbzG";
const ROLE_LAMPORTS: u64 = 1_412_880;
const ROLE_DATA_HEX: &str = "8eec87c5d603f4e229a21b8b7bc27b41e16d26a3efa1bd058e48ad3953fbb5ea081cd68a96ff61dd0300b3eb8835730bbb1331e04298a9f8f514fcf20e7220d937c497b7ca16af06f9d4fe";
const ROLE_USER: &str = "3oX5ttHJvcqJDwbYh96tkShaa4bnWMM3JHc2N4kocSNY";
/// Byte 40 of `ROLE_DATA_HEX` is `03` = `Role::Lawyer`; byte 41 is `00` = `Compliant`.
const ROLE_ROLE: &str = "LAWYER";
const ROLE_PERMISSION: &str = "COMPLIANT";
const ROLE_RENT_PAYER: &str = "D7LHTCvNtG37QsZSphsCTkJhLhg3SfpyjqMBwtfqbvaP";
const ROLE_BUMP: i16 = 254;

/// The transaction that created `ROLE_PUBKEY` on devnet
/// (`FSAgM2tYh1SYDFspXUrBYsNGMrkEMxGRPx1J52kjiaf6tFHWMmMtASQkgr8nfH795zYoUhcc3CsURG2LSoKZiou`,
/// slot 483386945): `assign_role(Lawyer)` with accounts
/// `[admin_signer, admin, user, role_account, system_program]`. The full base64 instruction
/// data was `ffae7db4cb9bca8303` -- discriminator + one borsh byte for the role.
const ASSIGN_ROLE_ACCOUNTS: [&str; 5] = [
    "D7LHTCvNtG37QsZSphsCTkJhLhg3SfpyjqMBwtfqbvaP", // admin_signer
    "GgbAVFmC41aaRnE9yq9xp2xc2oJAuKS4SA8vGyDLmUsn", // admin PDA
    "3oX5ttHJvcqJDwbYh96tkShaa4bnWMM3JHc2N4kocSNY", // user
    "2faa6q7HBRsUvazW7KxgRdrLHYepf7SFxrYzY2BwDbzG", // role_account PDA
    "11111111111111111111111111111111",             // system_program
];
const ASSIGN_ROLE_SLOT: u64 = 483_386_945;
const ASSIGN_ROLE_BLOCK_TIME: i64 = 1_786_601_078;

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

fn key(s: &str) -> Pubkey {
    Pubkey::from_str(s).expect("valid base58 pubkey")
}

fn account(data_hex: &str, lamports: u64) -> Account {
    Account {
        lamports,
        data: hex(data_hex),
        owner: PROGRAM_ID,
        executable: false,
        rent_epoch: u64::MAX,
    }
}

/// A resolver that will never be asked to make a network call: every test supplies the block
/// time as a stream hint, which short-circuits before any RPC client is touched.
fn offline_block_time() -> Arc<BlockTimeResolver> {
    Arc::new(BlockTimeResolver::new(
        "http://127.0.0.1:1/never-called",
        "http://127.0.0.1:1/never-called",
    ))
}

fn metrics() -> Arc<MetricsCollection> {
    Arc::new(MetricsCollection::default())
}

/// Runs `body` with a live batcher, then drops it and waits for the final flush -- so when
/// this returns, everything `body` pushed is committed.
async fn with_batcher<F, Fut>(pool: &PgPool, body: F)
where
    F: FnOnce(Batcher) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (batcher, flusher) = batcher::spawn(pool.clone(), CancellationToken::new());
    body(batcher).await;
    flusher.await.expect("flusher must not panic");
}

/// Pushes one decoded account update through the real `AccountProcessor`.
async fn apply_account(pool: &PgPool, pubkey: &str, data_hex: &str, lamports: u64, slot: u64) {
    let tracked = pipeline::new_tracked_accounts();
    apply_account_tracked(pool, &tracked, pubkey, data_hex, lamports, slot).await;
}

async fn apply_account_tracked(
    pool: &PgPool,
    tracked: &crate::processors::TrackedAccounts,
    pubkey: &str,
    data_hex: &str,
    lamports: u64,
    slot: u64,
) {
    let raw = account(data_hex, lamports);
    let decoded: carbon_core::account::DecodedAccount<XcavateWhitelistAccount> =
        carbon_core::account::AccountDecoder::decode_account(&XcavateWhitelistDecoder, &raw)
            .expect("real devnet account data must decode against the checked-in IDL");

    let meta = AccountMetadata {
        slot,
        pubkey: key(pubkey),
        transaction_signature: None,
    };

    with_batcher(pool, |batcher| async move {
        let mut processor =
            AccountProcessor::<crate::mapping::whitelist::Whitelist>::new(batcher, tracked.clone());
        processor
            .process((meta, decoded, raw), metrics())
            .await
            .expect("account processing must succeed");
    })
    .await;
}

// --- account state ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../migrations")]
async fn real_devnet_accounts_land_in_the_state_tables_with_the_values_on_chain(pool: PgPool) {
    let tracked = pipeline::new_tracked_accounts();
    apply_account_tracked(
        &pool,
        &tracked,
        CONFIG_PUBKEY,
        CONFIG_DATA_HEX,
        CONFIG_LAMPORTS,
        ASSIGN_ROLE_SLOT,
    )
    .await;
    apply_account_tracked(
        &pool,
        &tracked,
        ADMIN_PUBKEY,
        ADMIN_DATA_HEX,
        ADMIN_LAMPORTS,
        ASSIGN_ROLE_SLOT,
    )
    .await;
    apply_account_tracked(
        &pool,
        &tracked,
        ROLE_PUBKEY,
        ROLE_DATA_HEX,
        ROLE_LAMPORTS,
        ASSIGN_ROLE_SLOT,
    )
    .await;

    let cfg = sqlx::query("SELECT * FROM config WHERE pubkey = $1")
        .bind(key(CONFIG_PUBKEY).to_bytes().to_vec())
        .fetch_one(&pool)
        .await
        .expect("config row");
    assert_eq!(
        bs58::encode(cfg.get::<Vec<u8>, _>("authority")).into_string(),
        CONFIG_AUTHORITY
    );
    assert_eq!(cfg.get::<Option<Vec<u8>>, _>("pending_authority"), None);
    assert_eq!(cfg.get::<i16, _>("bump"), CONFIG_BUMP);
    assert_eq!(cfg.get::<i64, _>("lamports"), CONFIG_LAMPORTS as i64);
    assert_eq!(cfg.get::<i64, _>("slot"), ASSIGN_ROLE_SLOT as i64);
    assert_eq!(cfg.get::<Option<i64>, _>("closed_at_slot"), None);

    let adm = sqlx::query("SELECT * FROM admin WHERE pubkey = $1")
        .bind(key(ADMIN_PUBKEY).to_bytes().to_vec())
        .fetch_one(&pool)
        .await
        .expect("admin row");
    assert_eq!(
        bs58::encode(adm.get::<Vec<u8>, _>("admin")).into_string(),
        ADMIN_ADMIN
    );
    assert_eq!(adm.get::<i16, _>("bump"), ADMIN_BUMP);

    let role = sqlx::query("SELECT * FROM role_account WHERE pubkey = $1")
        .bind(key(ROLE_PUBKEY).to_bytes().to_vec())
        .fetch_one(&pool)
        .await
        .expect("role_account row");
    assert_eq!(
        bs58::encode(role.get::<Vec<u8>, _>("user_pubkey")).into_string(),
        ROLE_USER
    );
    assert_eq!(role.get::<String, _>("role"), ROLE_ROLE);
    assert_eq!(role.get::<String, _>("permission"), ROLE_PERMISSION);
    assert_eq!(
        bs58::encode(role.get::<Vec<u8>, _>("rent_payer")).into_string(),
        ROLE_RENT_PAYER
    );
    assert_eq!(role.get::<i16, _>("bump"), ROLE_BUMP);

    // config_view reads `authority` straight from the state table (it is on-chain state, not
    // something folded out of the action log), so this is the check that the authority the
    // API will serve equals the authority on chain.
    let view = sqlx::query("SELECT authority, pending_authority FROM config_view")
        .fetch_one(&pool)
        .await
        .expect("config_view row");
    assert_eq!(
        bs58::encode(view.get::<Vec<u8>, _>("authority")).into_string(),
        CONFIG_AUTHORITY
    );
    assert_eq!(view.get::<Option<Vec<u8>>, _>("pending_authority"), None);

    // Every PDA the account pipe sees must become deletion-tracked, or the datasource will
    // never synthesise an AccountDeletion for it.
    let tracked = tracked.read().await;
    assert!(tracked.contains(&key(CONFIG_PUBKEY)));
    assert!(tracked.contains(&key(ADMIN_PUBKEY)));
    assert!(tracked.contains(&key(ROLE_PUBKEY)));
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_stale_account_update_cannot_walk_the_state_row_backwards(pool: PgPool) {
    apply_account(&pool, ADMIN_PUBKEY, ADMIN_DATA_HEX, 500, 200).await;
    // Same account, older slot, different lamports: the slot guard must reject the whole row.
    apply_account(&pool, ADMIN_PUBKEY, ADMIN_DATA_HEX, 999, 100).await;

    let row = sqlx::query("SELECT slot, lamports FROM admin WHERE pubkey = $1")
        .bind(key(ADMIN_PUBKEY).to_bytes().to_vec())
        .fetch_one(&pool)
        .await
        .expect("admin row");
    assert_eq!(row.get::<i64, _>("slot"), 200);
    assert_eq!(row.get::<i64, _>("lamports"), 500);
}

// --- instructions ----------------------------------------------------------------------------

/// Builds the real on-chain `assign_role` instruction (discriminator + borsh role byte) and
/// pushes it through the decoder and the real `InstructionProcessor`.
async fn apply_assign_role(pool: &PgPool, signature_byte: u8) {
    let accounts: Vec<Pubkey> = ASSIGN_ROLE_ACCOUNTS.iter().map(|s| key(s)).collect();
    let mut data = vec![255, 174, 125, 180, 203, 155, 202, 131];
    data.push(3); // Role::Lawyer
    let raw = solana_instruction::Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts
            .iter()
            .map(|p| solana_instruction::AccountMeta {
                pubkey: *p,
                is_signer: false,
                is_writable: false,
            })
            .collect(),
        data,
    };
    let decoded_ix = XcavateWhitelistDecoder
        .decode_instruction(&raw)
        .expect("real on-chain assign_role data must decode");

    let meta = instruction_metadata(
        tx_metadata(
            sig_from(signature_byte),
            ASSIGN_ROLE_SLOT,
            Some(ASSIGN_ROLE_BLOCK_TIME),
        ),
        &[0],
    );

    with_batcher(pool, |batcher| async move {
        let mut processor = InstructionProcessor::<crate::mapping::whitelist::Whitelist>::new(
            batcher,
            offline_block_time(),
        );
        processor
            .process((meta, decoded_ix, Default::default(), raw), metrics())
            .await
            .expect("instruction processing must succeed");
    })
    .await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_instruction_writes_both_rows_and_reprocessing_it_is_a_no_op(pool: PgPool) {
    apply_assign_role(&pool, 7).await;
    apply_assign_role(&pool, 7).await; // exactly what a stream reconnect or a re-run does

    let counts = sqlx::query(
        "SELECT (SELECT count(*) FROM program_instructions) AS ins,
                (SELECT count(*) FROM whitelist_actions)    AS act",
    )
    .fetch_one(&pool)
    .await
    .expect("counts");
    assert_eq!(counts.get::<i64, _>("ins"), 1);
    assert_eq!(counts.get::<i64, _>("act"), 1);

    let action = sqlx::query("SELECT * FROM whitelist_actions")
        .fetch_one(&pool)
        .await
        .expect("action row");
    assert_eq!(action.get::<String, _>("type"), "ROLE_ASSIGNED");
    assert_eq!(action.get::<String, _>("actor"), ASSIGN_ROLE_ACCOUNTS[0]);
    assert_eq!(
        action.get::<Option<String>, _>("subject").as_deref(),
        Some(ASSIGN_ROLE_ACCOUNTS[2])
    );
    assert_eq!(
        action.get::<Option<String>, _>("role").as_deref(),
        Some("LAWYER")
    );
    assert_eq!(action.get::<i64, _>("slot"), ASSIGN_ROLE_SLOT as i64);
    assert_eq!(action.get::<String, _>("instruction_index"), "0");

    let ins = sqlx::query("SELECT * FROM program_instructions")
        .fetch_one(&pool)
        .await
        .expect("instruction row");
    assert_eq!(ins.get::<String, _>("ix_name"), "assign_role");
    assert_eq!(ins.get::<i16, _>("ix_index"), 0);
    assert_eq!(ins.get::<i16, _>("inner_index"), -1);
    let stored_accounts: Vec<Vec<u8>> = ins.get("accounts");
    assert_eq!(
        stored_accounts
            .iter()
            .map(|a| bs58::encode(a).into_string())
            .collect::<Vec<_>>(),
        ASSIGN_ROLE_ACCOUNTS.to_vec()
    );
    // Block time comes from the datasource hint, unchanged, in UTC.
    assert_eq!(
        ins.get::<chrono::DateTime<chrono::Utc>, _>("block_time")
            .timestamp(),
        ASSIGN_ROLE_BLOCK_TIME
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn remove_role_soft_closes_the_role_account_state_row(pool: PgPool) {
    // The account pipe saw the PDA created...
    apply_account(&pool, ROLE_PUBKEY, ROLE_DATA_HEX, ROLE_LAMPORTS, 1_000).await;

    // ...then a remove_role instruction closes it. Accounts are
    // [admin_signer, admin, user, rent_payer, role_account] -- index 4 is the PDA.
    let accounts = [
        key(ASSIGN_ROLE_ACCOUNTS[0]),
        key(ASSIGN_ROLE_ACCOUNTS[1]),
        key(ROLE_USER),
        key(ROLE_RENT_PAYER),
        key(ROLE_PUBKEY),
    ];
    let decoded_ix = decoded(
        XcavateWhitelistInstruction::RemoveRole(RemoveRole {
            role: ChainRole::Lawyer,
        }),
        &accounts,
    );
    let meta = instruction_metadata(tx_metadata(sig_from(9), 1_001, Some(1_786_601_078)), &[0]);
    let raw = solana_instruction::Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![],
        data: vec![],
    };

    with_batcher(&pool, |batcher| async move {
        let mut processor = InstructionProcessor::<crate::mapping::whitelist::Whitelist>::new(
            batcher,
            offline_block_time(),
        );
        processor
            .process((meta, decoded_ix, Default::default(), raw), metrics())
            .await
            .expect("instruction processing must succeed");
    })
    .await;

    let row = sqlx::query("SELECT slot, closed_at_slot FROM role_account WHERE pubkey = $1")
        .bind(key(ROLE_PUBKEY).to_bytes().to_vec())
        .fetch_one(&pool)
        .await
        .expect("role_account row");
    assert_eq!(row.get::<Option<i64>, _>("closed_at_slot"), Some(1_001));
    assert_eq!(row.get::<i64, _>("slot"), 1_001);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_close_and_the_upsert_it_closes_may_arrive_in_one_batch(pool: PgPool) {
    // Both writes go into a single flush. The batcher's phase ordering (upserts before closes)
    // is what makes this deterministic; without it the close could run against a row that does
    // not exist yet and be silently lost.
    let raw = account(ROLE_DATA_HEX, ROLE_LAMPORTS);
    let decoded_acct: carbon_core::account::DecodedAccount<XcavateWhitelistAccount> =
        carbon_core::account::AccountDecoder::decode_account(&XcavateWhitelistDecoder, &raw)
            .expect("decodes");
    let meta = AccountMetadata {
        slot: 1_000,
        pubkey: key(ROLE_PUBKEY),
        transaction_signature: None,
    };

    with_batcher(&pool, |batcher| async move {
        // Deletion first, account update second -- the *wrong* order on the wire.
        let mut deletions = AccountDeletionProcessor::new(batcher.clone());
        deletions
            .process(
                AccountDeletion {
                    pubkey: key(ROLE_PUBKEY),
                    slot: 1_001,
                    transaction_signature: None,
                },
                metrics(),
            )
            .await
            .expect("deletion processing must succeed");

        let mut accounts = AccountProcessor::<crate::mapping::whitelist::Whitelist>::new(
            batcher,
            pipeline::new_tracked_accounts(),
        );
        accounts
            .process((meta, decoded_acct, raw), metrics())
            .await
            .expect("account processing must succeed");
    })
    .await;

    let row = sqlx::query("SELECT slot, closed_at_slot FROM role_account WHERE pubkey = $1")
        .bind(key(ROLE_PUBKEY).to_bytes().to_vec())
        .fetch_one(&pool)
        .await
        .expect("role_account row");
    assert_eq!(row.get::<Option<i64>, _>("closed_at_slot"), Some(1_001));
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_deletion_for_an_unknown_pubkey_is_a_no_op_not_an_error(pool: PgPool) {
    // AccountDeletion carries no type information, so the close is attempted against all three
    // tables; the two that do not own the pubkey (and, here, all three) must simply do nothing.
    with_batcher(&pool, |batcher| async move {
        let mut deletions = AccountDeletionProcessor::new(batcher);
        deletions
            .process(
                AccountDeletion {
                    pubkey: key(CONFIG_PUBKEY),
                    slot: 42,
                    transaction_signature: None,
                },
                metrics(),
            )
            .await
            .expect("deletion processing must succeed");
    })
    .await;

    let counts = sqlx::query(
        "SELECT (SELECT count(*) FROM config) c,
                (SELECT count(*) FROM admin) a,
                (SELECT count(*) FROM role_account) r",
    )
    .fetch_one(&pool)
    .await
    .expect("counts");
    assert_eq!(counts.get::<i64, _>("c"), 0);
    assert_eq!(counts.get::<i64, _>("a"), 0);
    assert_eq!(counts.get::<i64, _>("r"), 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn open_account_pubkeys_returns_exactly_the_unclosed_rows(pool: PgPool) {
    // This is what seeds the deletion tracker at startup, so a closed PDA must not come back.
    apply_account(&pool, CONFIG_PUBKEY, CONFIG_DATA_HEX, CONFIG_LAMPORTS, 100).await;
    apply_account(&pool, ADMIN_PUBKEY, ADMIN_DATA_HEX, ADMIN_LAMPORTS, 100).await;
    apply_account(&pool, ROLE_PUBKEY, ROLE_DATA_HEX, ROLE_LAMPORTS, 100).await;
    crate::db::accounts::close_admin(&pool, &key(ADMIN_PUBKEY).to_bytes(), 200)
        .await
        .expect("close");

    let open = crate::db::accounts::open_account_pubkeys(&pool)
        .await
        .expect("query");
    let open: Vec<String> = open.iter().map(|b| bs58::encode(b).into_string()).collect();
    assert_eq!(open.len(), 2, "{open:?}");
    assert!(open.contains(&CONFIG_PUBKEY.to_string()));
    assert!(open.contains(&ROLE_PUBKEY.to_string()));
    assert!(!open.contains(&ADMIN_PUBKEY.to_string()));
}

// --- sanity on the fixtures themselves ---------------------------------------------------------

#[test]
fn the_assign_role_fixture_matches_the_role_account_it_created() {
    // The instruction fixture and the account fixture were captured independently (one from
    // getTransaction, one from getProgramAccounts); this pins them together so a future edit
    // to one of them cannot quietly desynchronise the story the tests tell.
    assert_eq!(ASSIGN_ROLE_ACCOUNTS[2], ROLE_USER);
    assert_eq!(ASSIGN_ROLE_ACCOUNTS[3], ROLE_PUBKEY);
    assert_eq!(ASSIGN_ROLE_ACCOUNTS[1], ADMIN_PUBKEY);
    assert_eq!(ASSIGN_ROLE_ACCOUNTS[0], ADMIN_ADMIN);
    assert_eq!(ROLE_RENT_PAYER, ADMIN_ADMIN);
}

#[test]
fn every_fixture_decodes_to_the_variant_it_claims() {
    let decode = |hex_data: &str| {
        carbon_core::account::AccountDecoder::decode_account(
            &XcavateWhitelistDecoder,
            &account(hex_data, 1),
        )
        .map(|d: carbon_core::account::DecodedAccount<XcavateWhitelistAccount>| d.data)
    };
    assert!(matches!(
        decode(CONFIG_DATA_HEX),
        Some(XcavateWhitelistAccount::Config(_))
    ));
    assert!(matches!(
        decode(ADMIN_DATA_HEX),
        Some(XcavateWhitelistAccount::Admin(_))
    ));
    assert!(matches!(
        decode(ROLE_DATA_HEX),
        Some(XcavateWhitelistAccount::RoleAccount(_))
    ));

    // And the instruction fixture is genuinely an AssignRole(Lawyer).
    let mut data = vec![255, 174, 125, 180, 203, 155, 202, 131];
    data.push(3);
    let raw = solana_instruction::Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![],
        data,
    };
    let d = XcavateWhitelistDecoder
        .decode_instruction(&raw)
        .expect("decodes");
    assert_eq!(
        d.data,
        XcavateWhitelistInstruction::AssignRole(AssignRole {
            role: ChainRole::Lawyer
        })
    );
}

// --- the backfill cursor's commit ordering ---------------------------------------------------

/// The resume cursor travels through the batcher with the rows it vouches for, and is sorted
/// last, so it can never be committed ahead of them. This is what makes an interrupted backfill
/// safe to resume from: a cursor that exists always describes rows that exist.
#[sqlx::test(migrations = "../../migrations")]
async fn the_backfill_cursor_commits_together_with_the_rows_it_vouches_for(pool: PgPool) {
    let ix = crate::db::models::NewInstruction {
        program_id: PROGRAM_ID.to_bytes().to_vec(),
        signature: sig_from(77).as_ref().to_vec(),
        ix_index: 0,
        inner_index: -1,
        slot: 483_386_945,
        block_time: chrono::DateTime::from_timestamp(ASSIGN_ROLE_BLOCK_TIME, 0).unwrap(),
        ix_name: "assign_role".into(),
        accounts: vec![],
        data: serde_json::json!({"type": "assign_role"}),
    };

    with_batcher(&pool, |batcher| async move {
        // Deliberately pushed in the "wrong" order -- cursor first -- to prove the batcher's
        // phase ordering, not the caller's discipline, is what guarantees the invariant.
        batcher
            .push(batcher::WriteOp::SetBackfillCursor {
                program_id: PROGRAM_ID.to_bytes().to_vec(),
                signature: "SigOfTheOldestSignatureInThePage".into(),
                slot: 483_386_945,
            })
            .await
            .unwrap();
        batcher
            .push(batcher::WriteOp::InsertInstruction(ix))
            .await
            .unwrap();
    })
    .await;

    let cursor = crate::db::backfill_cursor::get_cursor(&pool, &PROGRAM_ID.to_bytes())
        .await
        .expect("cursor read")
        .expect("cursor row written");
    assert_eq!(cursor.signature, "SigOfTheOldestSignatureInThePage");
    assert_eq!(cursor.slot, 483_386_945);

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM program_instructions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1, "the row the cursor vouches for is committed too");
}

// --- the snapshot writes exactly what the stream would --------------------------------------

/// The `getProgramAccounts` snapshot decodes with the same decoder and maps with the same
/// function as the live account pipe, so a snapshotted row and a streamed row for the same
/// account bytes at the same slot must be identical. If this ever diverges, a database seeded by
/// a snapshot would differ from one seeded by the stream -- silently.
#[sqlx::test(migrations = "../../migrations")]
async fn a_snapshot_row_is_identical_to_the_row_the_account_stream_would_write(pool: PgPool) {
    const SLOT: u64 = 484_000_000;

    // 1. The stream path: real account bytes through the real `AccountProcessor`.
    apply_account(&pool, ROLE_PUBKEY, ROLE_DATA_HEX, ROLE_LAMPORTS, SLOT).await;
    let streamed = fetch_role_row(&pool).await;

    // 2. The snapshot path: the same bytes through `decode_account` + `account_write_op`, which
    //    is exactly what `snapshot::run` does with each `getProgramAccounts` entry.
    let raw = account(ROLE_DATA_HEX, ROLE_LAMPORTS);
    let decoded =
        carbon_core::account::AccountDecoder::decode_account(&XcavateWhitelistDecoder, &raw)
            .expect("real devnet account data must decode");
    let op = crate::mapping::whitelist::account_write_op(
        key(ROLE_PUBKEY),
        SLOT as i64,
        raw.lamports as i64,
        &decoded,
    );

    // Wipe the streamed row and replay the snapshot op through the same batcher.
    sqlx::query("DELETE FROM role_account")
        .execute(&pool)
        .await
        .unwrap();
    with_batcher(&pool, |batcher| async move {
        batcher.push(op).await.unwrap();
    })
    .await;
    let snapshotted = fetch_role_row(&pool).await;

    assert_eq!(streamed, snapshotted);
}

/// Every column of the single `role_account` row, as a comparable tuple.
async fn fetch_role_row(
    pool: &PgPool,
) -> (Vec<u8>, i64, i64, Vec<u8>, String, String, Vec<u8>, i16) {
    let row = sqlx::query(
        "SELECT pubkey, slot, lamports, user_pubkey, role, permission, rent_payer, bump \
         FROM role_account",
    )
    .fetch_one(pool)
    .await
    .expect("exactly one role_account row");
    (
        row.get("pubkey"),
        row.get("slot"),
        row.get("lamports"),
        row.get("user_pubkey"),
        row.get("role"),
        row.get("permission"),
        row.get("rent_payer"),
        row.get("bump"),
    )
}

// --- program-upgrade recording (ADR-24) ------------------------------------------------------

/// Pushes one BPFLoaderUpgradeable `Upgrade` of the whitelist program through the real
/// decoder + recorder pair, exactly as either datasource would deliver it.
async fn apply_whitelist_upgrade(pool: &PgPool, slot: u64, signature_byte: u8) {
    let filler = |n: u8| Pubkey::new_from_array([n; 32]);
    // [programdata, program, buffer, spill, rent, clock, authority] -- the target at index 1.
    let accounts = [
        filler(41),
        PROGRAM_ID,
        filler(42),
        filler(43),
        filler(44),
        filler(45),
        filler(46),
    ];
    let raw = solana_instruction::Instruction {
        program_id: crate::upgrades::BPF_LOADER_UPGRADEABLE_ID,
        accounts: accounts
            .iter()
            .map(|p| solana_instruction::AccountMeta {
                pubkey: *p,
                is_signer: false,
                is_writable: true,
            })
            .collect(),
        // bincode: `Upgrade` is unit variant 3 of UpgradeableLoaderInstruction.
        data: vec![3, 0, 0, 0],
    };
    let decoded_ix = crate::upgrades::LoaderUpgradeDecoder
        .decode_instruction(&raw)
        .expect("a loader upgrade of a registry program must decode");

    let meta = instruction_metadata(tx_metadata(sig_from(signature_byte), slot, Some(1)), &[0]);

    with_batcher(pool, |batcher| async move {
        let mut recorder = crate::upgrades::UpgradeRecorder::new(batcher);
        recorder
            .process((meta, decoded_ix, Default::default(), raw), metrics())
            .await
            .expect("upgrade recording must succeed");
    })
    .await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_observed_upgrade_lands_in_program_upgrades_and_replays_are_no_ops(pool: PgPool) {
    let slot: u64 = 483_600_000;
    apply_whitelist_upgrade(&pool, slot, 21).await;
    // A backfill re-walk re-delivers the same transaction; the row must not duplicate.
    apply_whitelist_upgrade(&pool, slot, 21).await;

    let rows = sqlx::query("SELECT * FROM program_upgrades WHERE program_id = $1")
        .bind(PROGRAM_ID.to_bytes().to_vec())
        .fetch_all(&pool)
        .await
        .expect("program_upgrades rows");
    assert_eq!(
        rows.len(),
        1,
        "one boundary, however often it is re-observed"
    );
    let row = &rows[0];
    assert_eq!(row.get::<i64, _>("upgrade_slot"), slot as i64);
    assert_eq!(row.get::<String, _>("source"), "chain");
    assert_eq!(
        row.get::<String, _>("signature"),
        sig_from(21).to_string(),
        "the recorded signature is the upgrade transaction's"
    );
}
