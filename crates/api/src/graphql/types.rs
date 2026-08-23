//! GraphQL object types, kept "recognisably close to the old `schema.graphql`" (task-5-brief.md):
//! same field names/case as the old SubQuery schema, except the slot renames ruling R8 mandates
//! (`updatedAtBlock` -> `updatedAtSlot` etc.) -- see `crates/api/src/graphql/query.rs` for where
//! each field is sourced from Task 2's views.
//!
//! Every field here is plain, already-resolved data (no per-field DB access), so these are
//! `#[derive(GraphQLObject)]` structs rather than hand-written `#[graphql_object]` impls; juniper
//! renames `snake_case` struct fields to `camelCase` by default, which is exactly the casing the
//! old schema used.
//!
//! Slot/big-integer fields use `carbon_core::graphql::primitives::I64` (a string-serialized
//! scalar) rather than a bare `i64`, because juniper's built-in `Int` scalar is `i32`-only --
//! reusing carbon-core's own GraphQL primitive here is both the path of least resistance and
//! consistent with the brief's "use Carbon's graphql module" instruction.

use carbon_core::graphql::primitives::I64;
use chrono::{DateTime, Utc};
use juniper::{GraphQLObject, ID};

use super::enums::{ActionType, Permission, ProgramName, RemovalKind, Role};

/// Singleton (`id = "config"`): the sudo authority that manages whitelist admins.
#[derive(GraphQLObject, Clone, Debug)]
pub struct Config {
    pub id: ID,
    pub authority: String,
    pub pending_authority: Option<String>,
    pub updated_at_slot: I64,
    pub updated_at: DateTime<Utc>,
    pub updated_in_tx: String,
}

/// A whitelist admin (`id` = admin address). Removal soft-deletes (`active = false`).
#[derive(GraphQLObject, Clone, Debug)]
pub struct Admin {
    pub id: ID,
    pub active: bool,
    pub added_by: String,
    pub added_at_slot: I64,
    pub added_at: DateTime<Utc>,
    pub added_in_tx: String,
    pub removed_at_slot: Option<I64>,
    pub removed_at: Option<DateTime<Utc>>,
    pub removed_in_tx: Option<String>,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct AdminConnection {
    pub nodes: Vec<Admin>,
    pub total_count: i32,
}

/// One `(user, role)` assignment. Removal soft-deletes; a later re-assignment reactivates the
/// row and resets its audit fields -- full history is in `WhitelistAction`.
#[derive(GraphQLObject, Clone, Debug)]
pub struct RoleAssignment {
    pub id: ID,
    pub user: String,
    pub role: Role,
    pub permission: Permission,
    pub active: bool,
    pub rent_payer: String,
    pub assigned_by: String,
    pub assigned_at_slot: I64,
    pub assigned_at: DateTime<Utc>,
    pub assigned_in_tx: String,
    pub updated_at_slot: I64,
    pub updated_at: DateTime<Utc>,
    pub removed_at_slot: Option<I64>,
    pub removed_at: Option<DateTime<Utc>>,
    pub removed_in_tx: Option<String>,
    pub removal_kind: Option<RemovalKind>,
    pub removed_by: Option<String>,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct RoleAssignmentConnection {
    pub nodes: Vec<RoleAssignment>,
    pub total_count: i32,
}

/// Append-only audit log entry: one row per successfully indexed whitelist instruction.
#[derive(GraphQLObject, Clone, Debug)]
pub struct WhitelistAction {
    pub id: ID,
    #[graphql(name = "type")]
    pub action_type: ActionType,
    pub subject: Option<String>,
    pub role: Option<Role>,
    pub permission: Option<Permission>,
    pub actor: String,
    pub slot: I64,
    pub block_time: DateTime<Utc>,
    pub tx_signature: String,
    pub instruction_index: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct WhitelistActionConnection {
    pub nodes: Vec<WhitelistAction>,
    pub total_count: i32,
}

/// `checkAccess(user, role)` (ruling R17): preserves the dropped grpc-api's primary integration
/// query. `hasRole` = an active assignment exists; `compliant` = `hasRole` AND
/// `permission == COMPLIANT`.
#[derive(GraphQLObject, Clone, Debug)]
pub struct AccessCheck {
    pub has_role: bool,
    pub compliant: bool,
}

/// Replaces the old SubQuery `_metadata` surface. With four programs indexed, the top-level
/// fields are fleet aggregates -- the stack is only as caught-up as its laggiest program:
/// `lastContiguousSlot` is the minimum across programs, `backfillComplete` is true only when
/// every program's backfill is complete, `snapshotSlot` is the oldest snapshot (null if any
/// program has never been snapshotted). Per-program detail is in `programs`.
#[derive(GraphQLObject, Clone, Debug)]
pub struct SyncStatus {
    pub last_contiguous_slot: I64,
    pub backfill_complete: bool,
    pub snapshot_slot: Option<I64>,
    pub chain_tip_slot: I64,
    pub slot_lag: I64,
    pub programs: Vec<ProgramSyncStatus>,
}

/// One program's `sync_state` row.
#[derive(GraphQLObject, Clone, Debug)]
pub struct ProgramSyncStatus {
    pub program: ProgramName,
    pub last_contiguous_slot: I64,
    pub backfill_complete: bool,
    pub backfill_floor_slot: I64,
    pub snapshot_slot: Option<I64>,
}

/// One recorded version boundary of a program (`program_upgrades`, ADR-24): a slot at which
/// bytecode became live for that program.
#[derive(GraphQLObject, Clone, Debug)]
pub struct ProgramUpgrade {
    pub program: ProgramName,
    /// Slot the version's bytecode became live.
    pub upgrade_slot: I64,
    /// base58 signature of the upgrade transaction; null for the seeded deploy row.
    pub signature: Option<String>,
    /// `"deploy"` (the seeded initial deploy slot) or `"chain"` (an observed
    /// BPFLoaderUpgradeable upgrade). A plain string, not a GraphQL enum, on purpose: it is
    /// an internal provenance tag with no on-chain borsh order to mirror, and keeping it a
    /// string means widening the migration's CHECK never needs an api release in lockstep.
    pub source: String,
    pub detected_at: DateTime<Utc>,
}

/// One row of the shared `program_instructions` history: every successfully indexed
/// instruction of every indexed program, with its decoded args as JSON. `id` is
/// `"<txSignature>-<ixIndex>"` for a top-level instruction and
/// `"<txSignature>-<ixIndex>.<innerIndex>"` for a CPI.
#[derive(GraphQLObject, Clone, Debug)]
pub struct ProgramInstruction {
    pub id: ID,
    pub program: ProgramName,
    pub tx_signature: String,
    pub ix_index: i32,
    pub inner_index: i32,
    pub slot: I64,
    pub block_time: DateTime<Utc>,
    pub ix_name: String,
    /// The instruction's account list, in order, base58.
    pub accounts: Vec<String>,
    /// The decoded instruction args as JSON text (the decoder enum's serde shape).
    pub data: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct ProgramInstructionConnection {
    pub nodes: Vec<ProgramInstruction>,
    pub total_count: i32,
}
