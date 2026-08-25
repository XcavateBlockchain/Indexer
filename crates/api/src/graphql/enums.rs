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

// --- sibling-program enums (migrations 0008..0010) ------------------------------------------
// Same convention as above: juniper's default SCREAMING_SNAKE_CASE rename equals the TEXT
// spelling the CHECK constraints store, so as_db_str/from_db_str are straight mirrors.

/// `marketplace_listing.status` (`ListingStatus` on chain; borsh order load-bearing there).
#[derive(GraphQLEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingStatus {
    PendingAssets,
    Listed,
    SoldOut,
    Legal,
    Finalized,
    Expired,
    Cancelled,
    Refunding,
}

impl ListingStatus {
    pub const fn as_db_str(self) -> &'static str {
        match self {
            ListingStatus::PendingAssets => "PENDING_ASSETS",
            ListingStatus::Listed => "LISTED",
            ListingStatus::SoldOut => "SOLD_OUT",
            ListingStatus::Legal => "LEGAL",
            ListingStatus::Finalized => "FINALIZED",
            ListingStatus::Expired => "EXPIRED",
            ListingStatus::Cancelled => "CANCELLED",
            ListingStatus::Refunding => "REFUNDING",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        Some(match s {
            "PENDING_ASSETS" => ListingStatus::PendingAssets,
            "LISTED" => ListingStatus::Listed,
            "SOLD_OUT" => ListingStatus::SoldOut,
            "LEGAL" => ListingStatus::Legal,
            "FINALIZED" => ListingStatus::Finalized,
            "EXPIRED" => ListingStatus::Expired,
            "CANCELLED" => ListingStatus::Cancelled,
            "REFUNDING" => ListingStatus::Refunding,
            _ => return None,
        })
    }
}

/// `marketplace_listing.developer_lawyer_doc_status` / `spv_lawyer_doc_status`
/// (`DocumentStatus` on chain).
#[derive(GraphQLEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentStatus {
    Pending,
    Approved,
    Rejected,
}

impl DocumentStatus {
    /// Unused today (no doc-status filter arg yet) but kept so every enum carries both
    /// directions of the DB mirror, like the whitelist enums above.
    #[allow(dead_code)]
    pub const fn as_db_str(self) -> &'static str {
        match self {
            DocumentStatus::Pending => "PENDING",
            DocumentStatus::Approved => "APPROVED",
            DocumentStatus::Rejected => "REJECTED",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        Some(match s {
            "PENDING" => DocumentStatus::Pending,
            "APPROVED" => DocumentStatus::Approved,
            "REJECTED" => DocumentStatus::Rejected,
            _ => return None,
        })
    }
}

/// `regions_region_state.status` (`RegionStatus` on chain).
#[derive(GraphQLEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionStatus {
    Proposing,
    Passed,
    Rejected,
}

impl RegionStatus {
    pub const fn as_db_str(self) -> &'static str {
        match self {
            RegionStatus::Proposing => "PROPOSING",
            RegionStatus::Passed => "PASSED",
            RegionStatus::Rejected => "REJECTED",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        Some(match s {
            "PROPOSING" => RegionStatus::Proposing,
            "PASSED" => RegionStatus::Passed,
            "REJECTED" => RegionStatus::Rejected,
            _ => return None,
        })
    }
}

/// `regions_vote_record.vote` (`Vote` on chain). Named `RegionVote` to leave room for other
/// programs' vote enums.
#[derive(GraphQLEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionVote {
    Yes,
    No,
    Abstain,
}

impl RegionVote {
    /// Unused today (no vote filter arg yet) but kept so every enum carries both directions
    /// of the DB mirror, like the whitelist enums above.
    #[allow(dead_code)]
    pub const fn as_db_str(self) -> &'static str {
        match self {
            RegionVote::Yes => "YES",
            RegionVote::No => "NO",
            RegionVote::Abstain => "ABSTAIN",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        Some(match s {
            "YES" => RegionVote::Yes,
            "NO" => RegionVote::No,
            "ABSTAIN" => RegionVote::Abstain,
            _ => return None,
        })
    }
}

/// `property_gov_vote.choice` (`VoteChoice` on chain; borsh order load-bearing there).
/// Named `GovVoteChoice` because the `GovVote` spelling is the entity type.
#[derive(GraphQLEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum GovVoteChoice {
    Yes,
    No,
    Abstain,
}

impl GovVoteChoice {
    /// Unused today (no choice filter arg yet) but kept so every enum carries both
    /// directions of the DB mirror, like the whitelist enums above.
    #[allow(dead_code)]
    pub const fn as_db_str(self) -> &'static str {
        match self {
            GovVoteChoice::Yes => "YES",
            GovVoteChoice::No => "NO",
            GovVoteChoice::Abstain => "ABSTAIN",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        Some(match s {
            "YES" => GovVoteChoice::Yes,
            "NO" => GovVoteChoice::No,
            "ABSTAIN" => GovVoteChoice::Abstain,
            _ => return None,
        })
    }
}

/// The four indexed programs, for `programInstructions(program: ...)` filtering and
/// attribution. `as_program_id_bytes` mirrors `addresses.json` / the indexer's registry.
#[derive(GraphQLEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramName {
    XcavateWhitelist,
    Regions,
    Marketplace,
    Property,
}

impl ProgramName {
    /// The program's base58 address (same values as `addresses.json`).
    pub const fn address(self) -> &'static str {
        match self {
            ProgramName::XcavateWhitelist => "7TrzjKpdrEhnfhxuw8tWdH1sjxadazscsG5HXCDPLmaY",
            ProgramName::Regions => "5iupkzVtWxee48UXh3s615V9sXXuYjsSr61VPuduXdPc",
            ProgramName::Marketplace => "dj9Q3CpHvDHwexCbkgJ5APDx4JsTxPssNebkvP15g1T",
            ProgramName::Property => "deCp9srk9C6P4BXJaFpjR5H6Jsm6DCq8AL2kk338dVq",
        }
    }

    pub const ALL: &'static [ProgramName] = &[
        ProgramName::XcavateWhitelist,
        ProgramName::Regions,
        ProgramName::Marketplace,
        ProgramName::Property,
    ];

    /// The indexer registry's snake_case spelling (the `PROGRAMS` env var vocabulary).
    pub const fn registry_name(self) -> &'static str {
        match self {
            ProgramName::XcavateWhitelist => "xcavate_whitelist",
            ProgramName::Regions => "regions",
            ProgramName::Marketplace => "marketplace",
            ProgramName::Property => "property",
        }
    }

    pub fn from_registry_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|p| p.registry_name() == name)
    }

    /// The 32 raw bytes `program_instructions.program_id` stores.
    pub fn as_program_id_bytes(self) -> Vec<u8> {
        bs58::decode(self.address())
            .into_vec()
            .expect("compiled-in program addresses are valid base58")
    }

    /// Attribution for rows read back out of the database.
    pub fn from_program_id_bytes(bytes: &[u8]) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|p| p.as_program_id_bytes() == bytes)
    }
}
