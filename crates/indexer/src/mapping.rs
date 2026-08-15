//! The port of the old SubQuery `mappingHandlers.ts`: decoded instruction -> database rows.
//!
//! Deliberately pure and side-effect free (no DB, no clock, no RPC) so the whole mapping
//! contract is unit-testable without a database or a chain -- see the tests at the bottom,
//! which cover all nine instruction variants plus the nested-CPI path formatting.
//!
//! Per ruling R7 an instruction produces exactly two rows -- one `program_instructions`, one
//! `whitelist_actions` -- both idempotent and order-insensitive. The one exception (ruling
//! R11) is the three instructions that close a PDA: they additionally carry a [`PendingClose`]
//! so the account-state row gets soft-closed even though the closed account will never appear
//! in the owner-filtered account stream again.

use carbon_core::instruction::{DecodedInstruction, InstructionMetadata};
use carbon_xcavate_whitelist_decoder::instructions::XcavateWhitelistInstruction;
use carbon_xcavate_whitelist_decoder::types::{
    AccessPermission as ChainPermission, Role as ChainRole,
};
use chrono::{DateTime, Utc};
use solana_instruction::AccountMeta;

use crate::db::models::{AccessPermission, ActionType, NewAction, NewInstruction, Role};

/// What one decoded instruction turns into.
#[derive(Debug, Clone)]
pub struct MappedInstruction {
    pub instruction: NewInstruction,
    pub action: NewAction,
    /// Set only for `remove_admin` / `remove_role` / `renounce_role` (ruling R11).
    pub close: Option<PendingClose>,
}

/// A slot-guarded soft close implied by an instruction. The pubkey is taken from the
/// instruction's own account list rather than re-derived from PDA seeds -- the account being
/// closed is always present in the instruction, so there is nothing to derive and nothing to
/// get wrong. `debug_assert`ing the derivation would need the program id and a `find_program_
/// address` call per instruction; the account-list route is what the brief prefers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingClose {
    /// The `admin` PDA closed by `remove_admin` (accounts[2]).
    Admin { pubkey: Vec<u8>, slot: i64 },
    /// The `role_account` PDA closed by `remove_role` (accounts[4]) or `renounce_role`
    /// (accounts[2]).
    RoleAccount { pubkey: Vec<u8>, slot: i64 },
}

/// A whitelist instruction that decoded but could not be turned into rows.
///
/// This is always a loud error, never a silent drop: the old indexer's stance was that for a
/// compliance registry, data integrity beats liveness, and it carries over. Instructions that
/// fail to *decode* never reach this module -- they show up in carbon's `updates_failed`.
#[derive(Debug)]
pub enum MappingError {
    /// The instruction's account list is shorter than the mapping contract requires. Only
    /// possible if the on-chain program's account order changed without the IDL being
    /// regenerated.
    MissingAccount {
        ix_name: &'static str,
        position: usize,
        available: usize,
    },
    /// carbon always sets `absolute_path` to at least one element; an empty one means the
    /// upstream transformer changed shape.
    EmptyAbsolutePath { ix_name: &'static str },
    /// The decoded args would not serialize to JSON for `program_instructions.data`.
    Serialize {
        ix_name: &'static str,
        source: serde_json::Error,
    },
}

impl MappingError {
    /// Low-cardinality label for `decode_skipped_total`. Never contains a signature or a
    /// pubkey -- those go in the error log line, not in a metric label.
    pub fn reason(&self) -> &'static str {
        match self {
            MappingError::MissingAccount { .. } => "missing_account",
            MappingError::EmptyAbsolutePath { .. } => "empty_absolute_path",
            MappingError::Serialize { .. } => "serialize",
        }
    }
}

impl std::fmt::Display for MappingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MappingError::MissingAccount {
                ix_name,
                position,
                available,
            } => write!(
                f,
                "{ix_name}: account position {position} is out of range ({available} accounts present)"
            ),
            MappingError::EmptyAbsolutePath { ix_name } => {
                write!(f, "{ix_name}: instruction metadata has an empty absolute_path")
            }
            MappingError::Serialize { ix_name, source } => {
                write!(f, "{ix_name}: failed to serialize decoded args to JSON: {source}")
            }
        }
    }
}

impl std::error::Error for MappingError {}

/// The IDL spelling of an instruction, used verbatim as `program_instructions.ix_name`.
pub fn ix_name(ix: &XcavateWhitelistInstruction) -> &'static str {
    match ix {
        XcavateWhitelistInstruction::InitializeConfig(_) => "initialize_config",
        XcavateWhitelistInstruction::UpdateAuthority(_) => "update_authority",
        XcavateWhitelistInstruction::AcceptAuthority(_) => "accept_authority",
        XcavateWhitelistInstruction::AddAdmin(_) => "add_admin",
        XcavateWhitelistInstruction::RemoveAdmin(_) => "remove_admin",
        XcavateWhitelistInstruction::AssignRole(_) => "assign_role",
        XcavateWhitelistInstruction::RemoveRole(_) => "remove_role",
        XcavateWhitelistInstruction::RenounceRole(_) => "renounce_role",
        XcavateWhitelistInstruction::SetPermission(_) => "set_permission",
        XcavateWhitelistInstruction::CpiEvent(_) => "cpi_event",
    }
}

/// carbon's `absolute_path` -> the old SubQuery `ix.index.join(".")` format: `"3"` for a
/// top-level instruction, `"3.1"` for the second CPI under instruction 3, and so on. This is
/// half of the `whitelist_actions.id`, so the formatting is load-bearing for parity with the
/// old database.
pub fn instruction_index(absolute_path: &[u8]) -> String {
    absolute_path
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

/// On-chain borsh variant -> the old schema's spelling.
pub fn role_from_chain(role: &ChainRole) -> Role {
    match role {
        ChainRole::RegionalOperator => Role::RegionalOperator,
        ChainRole::RealEstateInvestor => Role::RealEstateInvestor,
        ChainRole::RealEstateDeveloper => Role::RealEstateDeveloper,
        ChainRole::Lawyer => Role::Lawyer,
        ChainRole::LettingAgent => Role::LettingAgent,
        ChainRole::SpvConfirmation => Role::SpvConfirmation,
    }
}

pub fn permission_from_chain(permission: &ChainPermission) -> AccessPermission {
    match permission {
        ChainPermission::Compliant => AccessPermission::Compliant,
        ChainPermission::Revoked => AccessPermission::Revoked,
    }
}

/// Map one decoded whitelist instruction to its rows.
///
/// `Ok(None)` means "nothing to write, and that is correct" -- the only such case is the
/// decoder's synthetic `CpiEvent` variant, which this program never emits (it uses log-based
/// `emit!`, not `emit_cpi!`).
///
/// `block_time` is resolved by the caller (see `block_time::BlockTimeResolver`) rather than
/// read off the metadata here, because the Yellowstone transaction stream leaves
/// `TransactionMetadata::block_time` at `None` and it has to come from an RPC lookup.
pub fn map_instruction(
    metadata: &InstructionMetadata,
    decoded: &DecodedInstruction<XcavateWhitelistInstruction>,
    block_time: DateTime<Utc>,
) -> Result<Option<MappedInstruction>, MappingError> {
    let name = ix_name(&decoded.data);

    if matches!(decoded.data, XcavateWhitelistInstruction::CpiEvent(_)) {
        return Ok(None);
    }

    let tx = &metadata.transaction_metadata;
    let slot = tx.slot as i64;
    let tx_signature = tx.signature.to_string();
    let accounts = decoded.accounts.as_slice();

    let path = metadata.absolute_path.as_slice();
    if path.is_empty() {
        return Err(MappingError::EmptyAbsolutePath { ix_name: name });
    }
    let index_str = instruction_index(path);

    // `inner_index` is -1 for a top-level instruction, else the position within the enclosing
    // instruction's CPI list. Nesting deeper than one level collapses onto the second element
    // of the path; acceptable here because this program performs no self-CPI, so a whitelist
    // instruction is only ever at depth 1 or 2. `instruction_index` above keeps the full path,
    // so no information is lost overall -- only `program_instructions`' composite key is
    // coarser than the path.
    let ix_index = path[0] as i16;
    let inner_index = if path.len() == 1 { -1 } else { path[1] as i16 };

    let instruction = NewInstruction {
        signature: tx.signature.as_ref().to_vec(),
        ix_index,
        inner_index,
        slot,
        block_time,
        ix_name: name.to_string(),
        accounts: accounts
            .iter()
            .map(|a| a.pubkey.to_bytes().to_vec())
            .collect(),
        data: serde_json::to_value(&decoded.data).map_err(|source| MappingError::Serialize {
            ix_name: name,
            source,
        })?,
    };

    // Fields that differ per instruction; everything else is common.
    struct Fields {
        action_type: ActionType,
        actor: String,
        subject: Option<String>,
        role: Option<Role>,
        permission: Option<AccessPermission>,
        close: Option<PendingClose>,
    }

    let f = match &decoded.data {
        XcavateWhitelistInstruction::InitializeConfig(_) => {
            let authority = account_at(accounts, 0, name)?;
            Fields {
                action_type: ActionType::ConfigInitialized,
                actor: authority.clone(),
                subject: Some(authority),
                role: None,
                permission: None,
                close: None,
            }
        }
        XcavateWhitelistInstruction::UpdateAuthority(args) => Fields {
            action_type: ActionType::AuthorityUpdateProposed,
            actor: account_at(accounts, 0, name)?,
            // Subject is the *proposed* authority, which is an instruction argument -- it is
            // not in the account list at all.
            subject: Some(args.new_authority.to_string()),
            role: None,
            permission: None,
            close: None,
        },
        XcavateWhitelistInstruction::AcceptAuthority(_) => {
            let new_authority = account_at(accounts, 0, name)?;
            Fields {
                action_type: ActionType::AuthorityUpdated,
                actor: new_authority.clone(),
                subject: Some(new_authority),
                role: None,
                permission: None,
                close: None,
            }
        }
        XcavateWhitelistInstruction::AddAdmin(_) => Fields {
            action_type: ActionType::AdminAdded,
            actor: account_at(accounts, 0, name)?,
            subject: Some(account_at(accounts, 2, name)?),
            role: None,
            permission: None,
            close: None,
        },
        XcavateWhitelistInstruction::RemoveAdmin(args) => Fields {
            action_type: ActionType::AdminRemoved,
            actor: account_at(accounts, 0, name)?,
            // Same shape as update_authority: the removed admin is an argument, while
            // accounts[2] is that admin's *PDA* (which is what gets closed).
            subject: Some(args.admin_key.to_string()),
            role: None,
            permission: None,
            close: Some(PendingClose::Admin {
                pubkey: account_bytes_at(accounts, 2, name)?,
                slot,
            }),
        },
        XcavateWhitelistInstruction::AssignRole(args) => Fields {
            action_type: ActionType::RoleAssigned,
            actor: account_at(accounts, 0, name)?,
            subject: Some(account_at(accounts, 2, name)?),
            role: Some(role_from_chain(&args.role)),
            permission: None,
            close: None,
        },
        XcavateWhitelistInstruction::RemoveRole(args) => Fields {
            action_type: ActionType::RoleRemoved,
            actor: account_at(accounts, 0, name)?,
            subject: Some(account_at(accounts, 2, name)?),
            role: Some(role_from_chain(&args.role)),
            permission: None,
            close: Some(PendingClose::RoleAccount {
                pubkey: account_bytes_at(accounts, 4, name)?,
                slot,
            }),
        },
        XcavateWhitelistInstruction::RenounceRole(args) => {
            let user = account_at(accounts, 0, name)?;
            Fields {
                action_type: ActionType::RoleRenounced,
                actor: user.clone(),
                subject: Some(user),
                role: Some(role_from_chain(&args.role)),
                permission: None,
                close: Some(PendingClose::RoleAccount {
                    pubkey: account_bytes_at(accounts, 2, name)?,
                    slot,
                }),
            }
        }
        XcavateWhitelistInstruction::SetPermission(args) => Fields {
            action_type: ActionType::PermissionUpdated,
            actor: account_at(accounts, 0, name)?,
            subject: Some(account_at(accounts, 2, name)?),
            role: Some(role_from_chain(&args.role)),
            permission: Some(permission_from_chain(&args.permission)),
            close: None,
        },
        // Handled by the early return above; repeated here so adding a variant to the decoder
        // enum is a compile error rather than a silent skip.
        XcavateWhitelistInstruction::CpiEvent(_) => return Ok(None),
    };

    let action = NewAction {
        id: format!("{tx_signature}-{index_str}"),
        action_type: f.action_type,
        subject: f.subject,
        role: f.role,
        permission: f.permission,
        actor: f.actor,
        slot,
        block_time,
        tx_signature,
        instruction_index: index_str,
    };

    Ok(Some(MappedInstruction {
        instruction,
        action,
        close: f.close,
    }))
}

fn account_at(
    accounts: &[AccountMeta],
    position: usize,
    ix_name: &'static str,
) -> Result<String, MappingError> {
    accounts
        .get(position)
        .map(|a| a.pubkey.to_string())
        .ok_or(MappingError::MissingAccount {
            ix_name,
            position,
            available: accounts.len(),
        })
}

fn account_bytes_at(
    accounts: &[AccountMeta],
    position: usize,
    ix_name: &'static str,
) -> Result<Vec<u8>, MappingError> {
    accounts
        .get(position)
        .map(|a| a.pubkey.to_bytes().to_vec())
        .ok_or(MappingError::MissingAccount {
            ix_name,
            position,
            available: accounts.len(),
        })
}

#[cfg(test)]
mod tests;
