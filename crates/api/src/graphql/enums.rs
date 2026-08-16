//! The four old `schema.graphql` enums (spec: "Enums keep old spellings"). `juniper::GraphQLEnum`
//! renames PascalCase Rust variants to SCREAMING_SNAKE_CASE by default, which is exactly the
//! spelling `migrations/0002_account_state.sql` / `0004_whitelist_actions.sql` store as `TEXT`
//! (see their `CHECK` constraints) -- so `as_db_str`/`from_db_str` below are a straight mirror,
//! not a remapping.

use juniper::GraphQLEnum;

#[derive(GraphQLEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    RegionalOperator,
    RealEstateInvestor,
    RealEstateDeveloper,
    Lawyer,
    LettingAgent,
    SpvConfirmation,
}

impl Role {
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

#[derive(GraphQLEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Compliant,
    Revoked,
}

impl Permission {
    pub const fn as_db_str(self) -> &'static str {
        match self {
            Permission::Compliant => "COMPLIANT",
            Permission::Revoked => "REVOKED",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        Some(match s {
            "COMPLIANT" => Permission::Compliant,
            "REVOKED" => Permission::Revoked,
            _ => return None,
        })
    }
}

#[derive(GraphQLEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalKind {
    /// Removed by an admin (`remove_role`).
    Removed,
    /// Given up by the holder (`renounce_role`).
    Renounced,
}

impl RemovalKind {
    pub fn from_db_str(s: &str) -> Option<Self> {
        Some(match s {
            "REMOVED" => RemovalKind::Removed,
            "RENOUNCED" => RemovalKind::Renounced,
            _ => return None,
        })
    }
}

#[derive(GraphQLEnum, Debug, Clone, Copy, PartialEq, Eq)]
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

    pub fn from_db_str(s: &str) -> Option<Self> {
        Some(match s {
            "CONFIG_INITIALIZED" => ActionType::ConfigInitialized,
            "AUTHORITY_UPDATE_PROPOSED" => ActionType::AuthorityUpdateProposed,
            "AUTHORITY_UPDATED" => ActionType::AuthorityUpdated,
            "ADMIN_ADDED" => ActionType::AdminAdded,
            "ADMIN_REMOVED" => ActionType::AdminRemoved,
            "ROLE_ASSIGNED" => ActionType::RoleAssigned,
            "ROLE_REMOVED" => ActionType::RoleRemoved,
            "ROLE_RENOUNCED" => ActionType::RoleRenounced,
            "PERMISSION_UPDATED" => ActionType::PermissionUpdated,
            _ => return None,
        })
    }
}

/// A DB value that should be one of the fixed spellings above but is not -- this should only be
/// reachable if the database and this binary's enum tables have drifted (e.g. a migration added
/// a variant this code doesn't know about yet), never from normal operation.
pub fn unknown_enum_value(kind: &str, value: &str) -> juniper::FieldError {
    juniper::FieldError::new(
        format!("unrecognised {kind} value in the database: {value:?}"),
        juniper::Value::null(),
    )
}
