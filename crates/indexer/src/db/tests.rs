//! Storage-layer tests. Two of these are the correctness lynchpins of the whole migration:
//! `slot_guard_*` (never let a stale write clobber fresher state) and `*_fold_is_order_insensitive`
//! (the derived views must not care whether live-stream or backfill wrote a row first).
//!
//! Run with a live Postgres reachable via `DATABASE_URL` (see
//! `.superpowers/sdd/carbon-migration-spec/task-2-report.md` for the exact command);
//! `#[sqlx::test]` creates and migrates a fresh throwaway database per test.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

use super::accounts::{
    close_admin, close_role_account, upsert_admin, upsert_config, upsert_role_account,
};
use super::actions::insert_action;
use super::backfill_cursor::{clear_cursor, get_cursor, set_cursor};
use super::instructions::insert_instruction;
use super::models::{
    AccessPermission, ActionType, AdminAccount, ConfigAccount, NewAction, NewInstruction, Role,
    RoleAccountRow,
};
use super::sync_state::{
    advance_last_contiguous_slot, get_sync_state, init_sync_state, set_backfill_complete,
    set_snapshot_slot,
};

fn bt(slot: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000 + slot, 0).unwrap()
}

fn pk(byte: u8) -> Vec<u8> {
    vec![byte; 32]
}

#[allow(clippy::too_many_arguments)] // test fixture builder, not production API
fn action(
    id: &str,
    action_type: ActionType,
    subject: Option<&str>,
    role: Option<Role>,
    permission: Option<AccessPermission>,
    actor: &str,
    slot: i64,
    tx_signature: &str,
    instruction_index: &str,
) -> NewAction {
    NewAction {
        id: id.to_string(),
        action_type,
        subject: subject.map(str::to_string),
        role,
        permission,
        actor: actor.to_string(),
        slot,
        block_time: bt(slot),
        tx_signature: tx_signature.to_string(),
        instruction_index: instruction_index.to_string(),
    }
}

// ============================================================================================
// Slot guard -- mandatory per spec §5.2: "apply slot 200, then apply slot 100, assert the row
// still reads slot 200." Run against all three account tables: each has its own hand-written
// WHERE clause and each must be checked independently.
// ============================================================================================

#[sqlx::test(migrations = "../../migrations")]
async fn slot_guard_config(pool: PgPool) -> sqlx::Result<()> {
    let pubkey = pk(1);
    upsert_config(
        &pool,
        ConfigAccount {
            pubkey: pubkey.clone(),
            slot: 200,
            lamports: 1_000,
            authority: pk(2),
            pending_authority: None,
            bump: 254,
        },
    )
    .await?;
    upsert_config(
        &pool,
        ConfigAccount {
            pubkey: pubkey.clone(),
            slot: 100,
            lamports: 1,
            authority: pk(9),
            pending_authority: Some(pk(8)),
            bump: 1,
        },
    )
    .await?;

    let row = sqlx::query("SELECT slot, lamports, authority FROM config WHERE pubkey = $1")
        .bind(&pubkey)
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.get::<i64, _>("slot"), 200);
    assert_eq!(row.get::<i64, _>("lamports"), 1_000);
    assert_eq!(row.get::<Vec<u8>, _>("authority"), pk(2));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
async fn slot_guard_admin(pool: PgPool) -> sqlx::Result<()> {
    let pubkey = pk(1);
    upsert_admin(
        &pool,
        AdminAccount {
            pubkey: pubkey.clone(),
            slot: 200,
            lamports: 1_000,
            admin: pk(2),
            bump: 1,
        },
    )
    .await?;
    upsert_admin(
        &pool,
        AdminAccount {
            pubkey: pubkey.clone(),
            slot: 100,
            lamports: 1,
            admin: pk(9),
            bump: 9,
        },
    )
    .await?;

    let row = sqlx::query("SELECT slot, lamports, admin FROM admin WHERE pubkey = $1")
        .bind(&pubkey)
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.get::<i64, _>("slot"), 200);
    assert_eq!(row.get::<i64, _>("lamports"), 1_000);
    assert_eq!(row.get::<Vec<u8>, _>("admin"), pk(2));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
async fn slot_guard_role_account(pool: PgPool) -> sqlx::Result<()> {
    let pubkey = pk(1);
    upsert_role_account(
        &pool,
        RoleAccountRow {
            pubkey: pubkey.clone(),
            slot: 200,
            lamports: 1_000,
            user_pubkey: pk(2),
            role: Role::Lawyer,
            permission: AccessPermission::Compliant,
            rent_payer: pk(3),
            bump: 1,
        },
    )
    .await?;
    upsert_role_account(
        &pool,
        RoleAccountRow {
            pubkey: pubkey.clone(),
            slot: 100,
            lamports: 1,
            user_pubkey: pk(9),
            role: Role::SpvConfirmation,
            permission: AccessPermission::Revoked,
            rent_payer: pk(9),
            bump: 9,
        },
    )
    .await?;

    let row = sqlx::query("SELECT slot, role, permission FROM role_account WHERE pubkey = $1")
        .bind(&pubkey)
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.get::<i64, _>("slot"), 200);
    assert_eq!(row.get::<String, _>("role"), "LAWYER");
    assert_eq!(row.get::<String, _>("permission"), "COMPLIANT");
    Ok(())
}

// ============================================================================================
// Soft close: guarded the same way as the upsert; a re-create at a higher slot clears
// `closed_at_slot`; a stale re-create below the close slot does nothing. "Test both
// directions" (spec §5.2).
// ============================================================================================

#[sqlx::test(migrations = "../../migrations")]
async fn soft_close_admin_guards_and_clears_on_recreate(pool: PgPool) -> sqlx::Result<()> {
    let pubkey = pk(1);
    upsert_admin(
        &pool,
        AdminAccount {
            pubkey: pubkey.clone(),
            slot: 100,
            lamports: 1,
            admin: pk(2),
            bump: 1,
        },
    )
    .await?;

    // Close at slot 200.
    let res = close_admin(&pool, &pubkey, 200).await?;
    assert_eq!(res.rows_affected(), 1);
    let row = sqlx::query("SELECT slot, closed_at_slot FROM admin WHERE pubkey = $1")
        .bind(&pubkey)
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.get::<i64, _>("slot"), 200);
    assert_eq!(row.get::<Option<i64>, _>("closed_at_slot"), Some(200));

    // Stale close attempt at a slot below the current one is a guarded no-op.
    let res = close_admin(&pool, &pubkey, 150).await?;
    assert_eq!(res.rows_affected(), 0);

    // Stale re-create below the close slot does nothing (the row is still closed at 200).
    upsert_admin(
        &pool,
        AdminAccount {
            pubkey: pubkey.clone(),
            slot: 150,
            lamports: 2,
            admin: pk(4),
            bump: 4,
        },
    )
    .await?;
    let row = sqlx::query("SELECT slot, closed_at_slot FROM admin WHERE pubkey = $1")
        .bind(&pubkey)
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.get::<i64, _>("slot"), 200);
    assert_eq!(row.get::<Option<i64>, _>("closed_at_slot"), Some(200));

    // Re-create above the close slot clears closed_at_slot back to NULL.
    upsert_admin(
        &pool,
        AdminAccount {
            pubkey: pubkey.clone(),
            slot: 300,
            lamports: 3,
            admin: pk(5),
            bump: 5,
        },
    )
    .await?;
    let row = sqlx::query("SELECT slot, closed_at_slot FROM admin WHERE pubkey = $1")
        .bind(&pubkey)
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.get::<i64, _>("slot"), 300);
    assert_eq!(row.get::<Option<i64>, _>("closed_at_slot"), None);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
async fn soft_close_role_account_guards_and_clears_on_recreate(pool: PgPool) -> sqlx::Result<()> {
    let pubkey = pk(1);
    let base = RoleAccountRow {
        pubkey: pubkey.clone(),
        slot: 100,
        lamports: 1,
        user_pubkey: pk(2),
        role: Role::Lawyer,
        permission: AccessPermission::Compliant,
        rent_payer: pk(3),
        bump: 1,
    };
    upsert_role_account(&pool, base.clone()).await?;

    let res = close_role_account(&pool, &pubkey, 200).await?;
    assert_eq!(res.rows_affected(), 1);

    // Stale re-create below the close slot does nothing.
    let mut stale = base.clone();
    stale.slot = 150;
    upsert_role_account(&pool, stale).await?;
    let row = sqlx::query("SELECT slot, closed_at_slot FROM role_account WHERE pubkey = $1")
        .bind(&pubkey)
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.get::<i64, _>("slot"), 200);
    assert_eq!(row.get::<Option<i64>, _>("closed_at_slot"), Some(200));

    // Re-create above the close slot clears closed_at_slot.
    let mut fresh = base;
    fresh.slot = 300;
    upsert_role_account(&pool, fresh).await?;
    let row = sqlx::query("SELECT slot, closed_at_slot FROM role_account WHERE pubkey = $1")
        .bind(&pubkey)
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.get::<i64, _>("slot"), 300);
    assert_eq!(row.get::<Option<i64>, _>("closed_at_slot"), None);
    Ok(())
}

// ============================================================================================
// Append-only inserts: reprocessing the same row is a no-op.
// ============================================================================================

#[sqlx::test(migrations = "../../migrations")]
async fn insert_instruction_is_idempotent(pool: PgPool) -> sqlx::Result<()> {
    let row = NewInstruction {
        signature: vec![7; 64],
        ix_index: 0,
        inner_index: -1,
        slot: 10,
        block_time: bt(10),
        ix_name: "add_admin".to_string(),
        accounts: vec![pk(1), pk(2)],
        data: serde_json::json!({"admin": "AAAA"}),
    };
    let r1 = insert_instruction(&pool, row.clone()).await?;
    assert_eq!(r1.rows_affected(), 1);
    let r2 = insert_instruction(&pool, row).await?;
    assert_eq!(r2.rows_affected(), 0);

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM program_instructions")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 1);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
async fn insert_action_is_idempotent(pool: PgPool) -> sqlx::Result<()> {
    let row = action(
        "sig1-0",
        ActionType::AdminAdded,
        Some("ADMIN_X"),
        None,
        None,
        "AUTHORITY",
        10,
        "sig1",
        "0",
    );
    let r1 = insert_action(&pool, row.clone()).await?;
    assert_eq!(r1.rows_affected(), 1);
    let r2 = insert_action(&pool, row).await?;
    assert_eq!(r2.rows_affected(), 0);

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM whitelist_actions")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 1);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
async fn instruction_and_action_insert_share_one_transaction(pool: PgPool) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    insert_instruction(
        &mut *tx,
        NewInstruction {
            signature: vec![1; 64],
            ix_index: 0,
            inner_index: -1,
            slot: 10,
            block_time: bt(10),
            ix_name: "add_admin".to_string(),
            accounts: vec![pk(1)],
            data: serde_json::json!({}),
        },
    )
    .await?;
    insert_action(
        &mut *tx,
        action(
            "sig-0",
            ActionType::AdminAdded,
            Some("ADMIN_X"),
            None,
            None,
            "AUTH",
            10,
            "sig",
            "0",
        ),
    )
    .await?;
    tx.commit().await?;

    let ix_count: i64 = sqlx::query_scalar("SELECT count(*) FROM program_instructions")
        .fetch_one(&pool)
        .await?;
    let action_count: i64 = sqlx::query_scalar("SELECT count(*) FROM whitelist_actions")
        .fetch_one(&pool)
        .await?;
    assert_eq!(ix_count, 1);
    assert_eq!(action_count, 1);
    Ok(())
}

// ============================================================================================
// sync_state helpers
// ============================================================================================

#[sqlx::test(migrations = "../../migrations")]
async fn sync_state_lifecycle(pool: PgPool) -> sqlx::Result<()> {
    // Uninitialized: no row yet.
    assert!(get_sync_state(&pool).await?.is_none());

    init_sync_state(&pool, 1_000).await?;
    let s = get_sync_state(&pool).await?.expect("row after init");
    assert_eq!(s.last_contiguous_slot, 1_000);
    assert_eq!(s.backfill_floor_slot, 1_000);
    assert!(!s.backfill_complete);
    assert_eq!(s.snapshot_slot, None);

    // A second init is a no-op (ON CONFLICT DO NOTHING) -- floor doesn't change.
    init_sync_state(&pool, 9_999).await?;
    let s = get_sync_state(&pool).await?.unwrap();
    assert_eq!(s.backfill_floor_slot, 1_000);

    advance_last_contiguous_slot(&pool, 1_500).await?;
    let s = get_sync_state(&pool).await?.unwrap();
    assert_eq!(s.last_contiguous_slot, 1_500);

    // Regression is guarded: advancing "backwards" does nothing.
    advance_last_contiguous_slot(&pool, 1_200).await?;
    let s = get_sync_state(&pool).await?.unwrap();
    assert_eq!(s.last_contiguous_slot, 1_500);

    set_snapshot_slot(&pool, 1_800).await?;
    set_backfill_complete(&pool, true).await?;
    let s = get_sync_state(&pool).await?.unwrap();
    assert_eq!(s.snapshot_slot, Some(1_800));
    assert!(s.backfill_complete);
    Ok(())
}

// ============================================================================================
// View folds. Fetched with runtime `Row::get` (not the `query!`/`query_as!` macros) so these
// assertions don't depend on sqlx's compile-time nullability inference over view columns,
// which is unrelated to what's actually being tested here.
// ============================================================================================

#[derive(Debug, Clone, PartialEq)]
struct AdminView {
    active: bool,
    added_by: String,
    added_at_slot: i64,
    added_in_tx: String,
    removed_at_slot: Option<i64>,
    removed_at: Option<DateTime<Utc>>,
    removed_in_tx: Option<String>,
}

impl AdminView {
    fn from_row(row: &PgRow) -> Self {
        Self {
            active: row.get("active"),
            added_by: row.get("added_by"),
            added_at_slot: row.get("added_at_slot"),
            added_in_tx: row.get("added_in_tx"),
            removed_at_slot: row.get("removed_at_slot"),
            removed_at: row.get("removed_at"),
            removed_in_tx: row.get("removed_in_tx"),
        }
    }
}

async fn fetch_admin_view(pool: &PgPool, id: &str) -> Option<AdminView> {
    sqlx::query("SELECT * FROM admins_view WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
        .as_ref()
        .map(AdminView::from_row)
}

#[sqlx::test(migrations = "../../migrations")]
async fn admins_view_add_only_is_active(pool: PgPool) -> sqlx::Result<()> {
    insert_action(
        &pool,
        action(
            "sig1-0",
            ActionType::AdminAdded,
            Some("ADMIN_X"),
            None,
            None,
            "AUTH",
            10,
            "sig1",
            "0",
        ),
    )
    .await?;

    let v = fetch_admin_view(&pool, "ADMIN_X")
        .await
        .expect("row present");
    assert!(v.active);
    assert_eq!(v.added_by, "AUTH");
    assert_eq!(v.added_at_slot, 10);
    assert_eq!(v.removed_at_slot, None);
    assert_eq!(v.removed_in_tx, None);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
async fn admins_view_add_then_remove_is_inactive(pool: PgPool) -> sqlx::Result<()> {
    insert_action(
        &pool,
        action(
            "sig1-0",
            ActionType::AdminAdded,
            Some("ADMIN_X"),
            None,
            None,
            "AUTH",
            10,
            "sig1",
            "0",
        ),
    )
    .await?;
    insert_action(
        &pool,
        action(
            "sig2-0",
            ActionType::AdminRemoved,
            Some("ADMIN_X"),
            None,
            None,
            "AUTH",
            20,
            "sig2",
            "0",
        ),
    )
    .await?;

    let v = fetch_admin_view(&pool, "ADMIN_X")
        .await
        .expect("row present");
    assert!(!v.active);
    assert_eq!(v.removed_at_slot, Some(20));
    assert_eq!(v.removed_in_tx.as_deref(), Some("sig2"));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
async fn admins_view_add_remove_readd_is_active_again(pool: PgPool) -> sqlx::Result<()> {
    insert_action(
        &pool,
        action(
            "sig1-0",
            ActionType::AdminAdded,
            Some("ADMIN_X"),
            None,
            None,
            "AUTH",
            10,
            "sig1",
            "0",
        ),
    )
    .await?;
    insert_action(
        &pool,
        action(
            "sig2-0",
            ActionType::AdminRemoved,
            Some("ADMIN_X"),
            None,
            None,
            "AUTH",
            20,
            "sig2",
            "0",
        ),
    )
    .await?;
    insert_action(
        &pool,
        action(
            "sig3-0",
            ActionType::AdminAdded,
            Some("ADMIN_X"),
            None,
            None,
            "AUTH",
            30,
            "sig3",
            "0",
        ),
    )
    .await?;

    let v = fetch_admin_view(&pool, "ADMIN_X")
        .await
        .expect("row present");
    assert!(v.active);
    assert_eq!(v.added_at_slot, 30);
    assert_eq!(v.removed_at_slot, None);
    assert_eq!(v.removed_in_tx, None);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
async fn admins_view_fold_is_order_insensitive(pool: PgPool) -> sqlx::Result<()> {
    let events = [
        action(
            "sig1-0",
            ActionType::AdminAdded,
            Some("ADMIN_X"),
            None,
            None,
            "AUTH",
            10,
            "sig1",
            "0",
        ),
        action(
            "sig2-0",
            ActionType::AdminRemoved,
            Some("ADMIN_X"),
            None,
            None,
            "AUTH",
            20,
            "sig2",
            "0",
        ),
        action(
            "sig3-0",
            ActionType::AdminAdded,
            Some("ADMIN_X"),
            None,
            None,
            "AUTH",
            30,
            "sig3",
            "0",
        ),
    ];

    for e in &events {
        insert_action(&pool, e.clone()).await?;
    }
    let forward = fetch_admin_view(&pool, "ADMIN_X")
        .await
        .expect("row present");

    sqlx::query("DELETE FROM whitelist_actions")
        .execute(&pool)
        .await?;

    // Reverse insertion order: backfill (walking backwards) racing/overtaking a live stream.
    for e in events.iter().rev() {
        insert_action(&pool, e.clone()).await?;
    }
    let reversed = fetch_admin_view(&pool, "ADMIN_X")
        .await
        .expect("row present");

    assert_eq!(forward, reversed);
    assert!(forward.active);
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
struct RoleAssignmentView {
    user_pubkey: String,
    role: String,
    active: bool,
    permission: String,
    rent_payer: String,
    assigned_by: String,
    assigned_at_slot: i64,
    assigned_in_tx: String,
    updated_at_slot: i64,
    removed_at_slot: Option<i64>,
    removed_in_tx: Option<String>,
    removed_by: Option<String>,
    removal_kind: Option<String>,
}

impl RoleAssignmentView {
    fn from_row(row: &PgRow) -> Self {
        Self {
            user_pubkey: row.get("user_pubkey"),
            role: row.get("role"),
            active: row.get("active"),
            permission: row.get("permission"),
            rent_payer: row.get("rent_payer"),
            assigned_by: row.get("assigned_by"),
            assigned_at_slot: row.get("assigned_at_slot"),
            assigned_in_tx: row.get("assigned_in_tx"),
            updated_at_slot: row.get("updated_at_slot"),
            removed_at_slot: row.get("removed_at_slot"),
            removed_in_tx: row.get("removed_in_tx"),
            removed_by: row.get("removed_by"),
            removal_kind: row.get("removal_kind"),
        }
    }
}

async fn fetch_role_assignment_view(pool: &PgPool, id: &str) -> Option<RoleAssignmentView> {
    sqlx::query("SELECT * FROM role_assignments_view WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
        .as_ref()
        .map(RoleAssignmentView::from_row)
}

const LAWYER_ID: &str = "USER1-3"; // Role::Lawyer borsh index = 3

#[sqlx::test(migrations = "../../migrations")]
async fn role_assignments_view_assign_only_is_compliant_active(pool: PgPool) -> sqlx::Result<()> {
    insert_action(
        &pool,
        action(
            "sigA-0",
            ActionType::RoleAssigned,
            Some("USER1"),
            Some(Role::Lawyer),
            None,
            "ADMIN_X",
            10,
            "sigA",
            "0",
        ),
    )
    .await?;

    let v = fetch_role_assignment_view(&pool, LAWYER_ID)
        .await
        .expect("row present");
    assert!(v.active);
    assert_eq!(v.permission, "COMPLIANT");
    assert_eq!(v.rent_payer, "ADMIN_X");
    assert_eq!(v.assigned_by, "ADMIN_X");
    assert_eq!(v.removal_kind, None);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
async fn role_assignments_view_assign_then_permission_update_revoked(
    pool: PgPool,
) -> sqlx::Result<()> {
    insert_action(
        &pool,
        action(
            "sigA-0",
            ActionType::RoleAssigned,
            Some("USER1"),
            Some(Role::Lawyer),
            None,
            "ADMIN_X",
            10,
            "sigA",
            "0",
        ),
    )
    .await?;
    insert_action(
        &pool,
        action(
            "sigB-0",
            ActionType::PermissionUpdated,
            Some("USER1"),
            Some(Role::Lawyer),
            Some(AccessPermission::Revoked),
            "ADMIN_X",
            20,
            "sigB",
            "0",
        ),
    )
    .await?;

    let v = fetch_role_assignment_view(&pool, LAWYER_ID)
        .await
        .expect("row present");
    assert!(v.active);
    assert_eq!(v.permission, "REVOKED");
    assert_eq!(v.updated_at_slot, 20);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
async fn role_assignments_view_assign_then_renounce(pool: PgPool) -> sqlx::Result<()> {
    insert_action(
        &pool,
        action(
            "sigA-0",
            ActionType::RoleAssigned,
            Some("USER1"),
            Some(Role::Lawyer),
            None,
            "ADMIN_X",
            10,
            "sigA",
            "0",
        ),
    )
    .await?;
    insert_action(
        &pool,
        action(
            "sigB-0",
            ActionType::RoleRenounced,
            Some("USER1"),
            Some(Role::Lawyer),
            None,
            "USER1",
            20,
            "sigB",
            "0",
        ),
    )
    .await?;

    let v = fetch_role_assignment_view(&pool, LAWYER_ID)
        .await
        .expect("row present");
    assert!(!v.active);
    assert_eq!(v.removal_kind.as_deref(), Some("RENOUNCED"));
    assert_eq!(v.removed_by.as_deref(), Some("USER1"));
    assert_eq!(v.removed_at_slot, Some(20));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
async fn role_assignments_view_assign_then_remove(pool: PgPool) -> sqlx::Result<()> {
    insert_action(
        &pool,
        action(
            "sigA-0",
            ActionType::RoleAssigned,
            Some("USER1"),
            Some(Role::Lawyer),
            None,
            "ADMIN_X",
            10,
            "sigA",
            "0",
        ),
    )
    .await?;
    insert_action(
        &pool,
        action(
            "sigB-0",
            ActionType::RoleRemoved,
            Some("USER1"),
            Some(Role::Lawyer),
            None,
            "ADMIN_X",
            20,
            "sigB",
            "0",
        ),
    )
    .await?;

    let v = fetch_role_assignment_view(&pool, LAWYER_ID)
        .await
        .expect("row present");
    assert!(!v.active);
    assert_eq!(v.removal_kind.as_deref(), Some("REMOVED"));
    assert_eq!(v.removed_by.as_deref(), Some("ADMIN_X"));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
async fn role_assignments_view_fold_is_order_insensitive(pool: PgPool) -> sqlx::Result<()> {
    let events = [
        action(
            "sigA-0",
            ActionType::RoleAssigned,
            Some("USER1"),
            Some(Role::Lawyer),
            None,
            "ADMIN_X",
            10,
            "sigA",
            "0",
        ),
        action(
            "sigB-0",
            ActionType::PermissionUpdated,
            Some("USER1"),
            Some(Role::Lawyer),
            Some(AccessPermission::Revoked),
            "ADMIN_X",
            20,
            "sigB",
            "0",
        ),
        action(
            "sigC-0",
            ActionType::RoleRemoved,
            Some("USER1"),
            Some(Role::Lawyer),
            None,
            "ADMIN_X",
            30,
            "sigC",
            "0",
        ),
    ];

    for e in &events {
        insert_action(&pool, e.clone()).await?;
    }
    let forward = fetch_role_assignment_view(&pool, LAWYER_ID)
        .await
        .expect("row present");

    sqlx::query("DELETE FROM whitelist_actions")
        .execute(&pool)
        .await?;

    for e in events.iter().rev() {
        insert_action(&pool, e.clone()).await?;
    }
    let reversed = fetch_role_assignment_view(&pool, LAWYER_ID)
        .await
        .expect("row present");

    assert_eq!(forward, reversed);
    assert!(!forward.active);
    assert_eq!(forward.permission, "REVOKED");
    assert_eq!(forward.removal_kind.as_deref(), Some("REMOVED"));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
async fn config_view_reflects_state_table_and_latest_action(pool: PgPool) -> sqlx::Result<()> {
    upsert_config(
        &pool,
        ConfigAccount {
            pubkey: pk(1),
            slot: 5,
            lamports: 100,
            authority: pk(2),
            pending_authority: None,
            bump: 254,
        },
    )
    .await?;
    insert_action(
        &pool,
        action(
            "sig1-0",
            ActionType::ConfigInitialized,
            None,
            None,
            None,
            "AUTH",
            5,
            "sig1",
            "0",
        ),
    )
    .await?;

    let row = sqlx::query(
        "SELECT authority, pending_authority, updated_at_slot, updated_in_tx FROM config_view",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(row.get::<Vec<u8>, _>("authority"), pk(2));
    assert_eq!(row.get::<Option<Vec<u8>>, _>("pending_authority"), None);
    assert_eq!(row.get::<i64, _>("updated_at_slot"), 5);
    assert_eq!(row.get::<String, _>("updated_in_tx"), "sig1");

    // A later AUTHORITY_UPDATE_PROPOSED moves updated_at_* without touching the (still
    // slot-guarded) account-state columns unless the account row itself is upserted too.
    insert_action(
        &pool,
        action(
            "sig2-0",
            ActionType::AuthorityUpdateProposed,
            None,
            None,
            None,
            "AUTH",
            15,
            "sig2",
            "0",
        ),
    )
    .await?;
    let row = sqlx::query("SELECT updated_at_slot, updated_in_tx FROM config_view")
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.get::<i64, _>("updated_at_slot"), 15);
    assert_eq!(row.get::<String, _>("updated_in_tx"), "sig2");
    Ok(())
}

/// The backfill resume cursor: written per committed page, overwritten as the walk descends,
/// deleted when the walk finishes. "A cursor exists" must mean exactly "an interrupted walk is
/// waiting to be resumed".
#[sqlx::test(migrations = "../../migrations")]
async fn backfill_cursor_lifecycle(pool: PgPool) -> sqlx::Result<()> {
    assert!(
        get_cursor(&pool).await?.is_none(),
        "a fresh database has no cursor"
    );

    set_cursor(&pool, "sigA", 500).await?;
    let c = get_cursor(&pool)
        .await?
        .expect("cursor after the first page");
    assert_eq!((c.signature.as_str(), c.slot), ("sigA", 500));

    // The walk descends: the second page overwrites the first (singleton row).
    set_cursor(&pool, "sigB", 400).await?;
    let c = get_cursor(&pool)
        .await?
        .expect("cursor after the second page");
    assert_eq!((c.signature.as_str(), c.slot), ("sigB", 400));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM backfill_cursor")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 1, "the cursor is a singleton");

    // A re-run from the tip legitimately moves the cursor back UP: it is a walk position, not a
    // high-water mark, so it must NOT be guarded the way `last_contiguous_slot` is.
    set_cursor(&pool, "sigZ", 900).await?;
    assert_eq!(get_cursor(&pool).await?.unwrap().slot, 900);

    clear_cursor(&pool).await?;
    assert!(
        get_cursor(&pool).await?.is_none(),
        "a finished walk leaves no cursor"
    );
    Ok(())
}
