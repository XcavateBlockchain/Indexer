//! Typed row shapes for the account-state tables (`config`, `admin`, `role_account`) and the
//! Rust-side mirrors of the two on-chain enums those tables store as `TEXT` + `CHECK`
//! (see `migrations/0002_account_state.sql`).

use chrono::{DateTime, Utc};

/// Mirrors the on-chain `Role` enum (`idls/xcavate_whitelist.json`). The borsh variant index
/// is load-bearing (it's what's actually stored in `RoleAccount.role` and baked into the
/// role-assignment PDA seed) -- the discriminants below must stay in this exact order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    RegionalOperator = 0,
    RealEstateInvestor = 1,
    RealEstateDeveloper = 2,
    Lawyer = 3,
    LettingAgent = 4,
    SpvConfirmation = 5,
}

impl Role {
    /// The spelling stored in `role_account.role` / `whitelist_actions.role` (old
    /// `schema.graphql` `Role` enum spellings).
    pub const fn as_db_str(self) -> &'static str {
        match self {
            Role::RegionalOperator => "REGIONAL_OPERATOR",
            Role::RealEstateInvestor => "REAL_ESTATE_INVESTOR",
            Role::RealEstateDeveloper => "REAL_ESTATE_DEVELOPER",
            Role::Lawyer => "LAWYER",
            Role::LettingAgent => "LETTING_AGENT",
            Role::SpvConfirmation => "SPV_CONFIRMATION",
        }
    }

    /// The borsh variant index / PDA seed byte.
    pub const fn borsh_index(self) -> u8 {
        self as u8
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        Some(match s {
            "REGIONAL_OPERATOR" => Role::RegionalOperator,
            "REAL_ESTATE_INVESTOR" => Role::RealEstateInvestor,
            "REAL_ESTATE_DEVELOPER" => Role::RealEstateDeveloper,
            "LAWYER" => Role::Lawyer,
            "LETTING_AGENT" => Role::LettingAgent,
            "SPV_CONFIRMATION" => Role::SpvConfirmation,
            _ => return None,
        })
    }
}

/// Mirrors the on-chain `AccessPermission` enum. Same load-bearing-index caveat as `Role`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessPermission {
    Compliant = 0,
    Revoked = 1,
}

impl AccessPermission {
    pub const fn as_db_str(self) -> &'static str {
        match self {
            AccessPermission::Compliant => "COMPLIANT",
            AccessPermission::Revoked => "REVOKED",
        }
    }

    pub const fn borsh_index(self) -> u8 {
        self as u8
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        Some(match s {
            "COMPLIANT" => AccessPermission::Compliant,
            "REVOKED" => AccessPermission::Revoked,
            _ => return None,
        })
    }
}

/// The nine `whitelist_actions.type` values (old `schema.graphql` `ActionType` enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionType {
    ConfigInitialized,
    AuthorityUpdateProposed,
    AuthorityUpdated,
    AdminAdded,
    AdminRemoved,
    RoleAssigned,
    RoleRemoved,
    RoleRenounced,
    PermissionUpdated,
}

impl ActionType {
    pub const fn as_db_str(self) -> &'static str {
        match self {
            ActionType::ConfigInitialized => "CONFIG_INITIALIZED",
            ActionType::AuthorityUpdateProposed => "AUTHORITY_UPDATE_PROPOSED",
            ActionType::AuthorityUpdated => "AUTHORITY_UPDATED",
            ActionType::AdminAdded => "ADMIN_ADDED",
            ActionType::AdminRemoved => "ADMIN_REMOVED",
            ActionType::RoleAssigned => "ROLE_ASSIGNED",
            ActionType::RoleRemoved => "ROLE_REMOVED",
            ActionType::RoleRenounced => "ROLE_RENOUNCED",
            ActionType::PermissionUpdated => "PERMISSION_UPDATED",
        }
    }
}

/// Row for the `config` table (mirrors the on-chain `Config` PDA; singleton in practice, but
/// nothing in the schema enforces that -- there's exactly one `Config` PDA per program).
#[derive(Debug, Clone)]
pub struct ConfigAccount {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub authority: Vec<u8>,
    pub pending_authority: Option<Vec<u8>>,
    pub bump: i16,
}

/// Row for the `admin` table (mirrors the on-chain `Admin` PDA).
#[derive(Debug, Clone)]
pub struct AdminAccount {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub admin: Vec<u8>,
    pub bump: i16,
}

/// Row for the `role_account` table (mirrors the on-chain `RoleAccount` PDA). `user_pubkey`
/// is the Rust-side name for the on-chain `user` field, renamed in the schema to dodge the
/// SQL reserved word.
#[derive(Debug, Clone)]
pub struct RoleAccountRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub user_pubkey: Vec<u8>,
    pub role: Role,
    pub permission: AccessPermission,
    pub rent_payer: Vec<u8>,
    pub bump: i16,
}

/// One row to insert into `program_instructions`. `inner_index` is `-1` for a top-level
/// instruction, or the CPI position within its transaction otherwise.
#[derive(Debug, Clone)]
pub struct NewInstruction {
    pub signature: Vec<u8>,
    pub ix_index: i16,
    pub inner_index: i16,
    pub slot: i64,
    pub block_time: DateTime<Utc>,
    pub ix_name: String,
    pub accounts: Vec<Vec<u8>>,
    pub data: serde_json::Value,
}

/// One row to insert into `whitelist_actions`. `role`/`permission` are `None` for action
/// types that don't carry them (e.g. `ADMIN_ADDED`).
#[derive(Debug, Clone)]
pub struct NewAction {
    pub id: String,
    pub action_type: ActionType,
    pub subject: Option<String>,
    pub role: Option<Role>,
    pub permission: Option<AccessPermission>,
    pub actor: String,
    pub slot: i64,
    pub block_time: DateTime<Utc>,
    pub tx_signature: String,
    pub instruction_index: String,
}
