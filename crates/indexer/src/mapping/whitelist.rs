//! The `xcavate_whitelist` mapping: the port of the old SubQuery `mappingHandlers.ts`.
//!
//! Per ruling R7 an instruction produces exactly two rows -- one `program_instructions`, one
//! `whitelist_actions` -- both idempotent and order-insensitive. The one exception (ruling
//! R11) is the three instructions that close a PDA: they additionally carry a
//! [`PendingClose`] so the account-state row gets soft-closed even though the closed account
//! will never appear in the owner-filtered account stream again.

use carbon_core::account::{AccountDecoder, DecodedAccount};
use carbon_core::instruction::{DecodedInstruction, InstructionMetadata};
use carbon_xcavate_whitelist_decoder::accounts::XcavateWhitelistAccount;
use carbon_xcavate_whitelist_decoder::instructions::XcavateWhitelistInstruction;
use carbon_xcavate_whitelist_decoder::types::{
    AccessPermission as ChainPermission, Role as ChainRole,
};
use carbon_xcavate_whitelist_decoder::{XcavateWhitelistDecoder, PROGRAM_ID};
use chrono::{DateTime, Utc};
use solana_account::Account;
use solana_pubkey::Pubkey;

use super::{
    account_at, close_at, instruction_row, ix_context, MappedInstruction, MappingError,
    PendingClose, ProgramMapper,
};
use crate::batcher::WriteOp;
use crate::db::close::StateTable;
use crate::db::models::{
    AccessPermission, ActionType, AdminAccount, ConfigAccount, NewAction, Role, RoleAccountRow,
};

/// The whitelist's [`ProgramMapper`] instantiation.
pub struct Whitelist;

impl ProgramMapper for Whitelist {
    type Ix = XcavateWhitelistInstruction;
    type Acc = XcavateWhitelistAccount;
    const NAME: &'static str = "xcavate_whitelist";

    fn map_instruction(
        metadata: &InstructionMetadata,
        decoded: &DecodedInstruction<Self::Ix>,
        block_time: DateTime<Utc>,
    ) -> Result<Option<MappedInstruction>, MappingError> {
        map_instruction(metadata, decoded, block_time)
    }

    fn account_write_op(
        pubkey: Pubkey,
        slot: i64,
        lamports: i64,
        decoded: &DecodedAccount<Self::Acc>,
    ) -> WriteOp {
        account_write_op(pubkey, slot, lamports, decoded)
    }
}

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

    let accounts = decoded.accounts.as_slice();
    let ctx = ix_context(name, metadata)?;
    let slot = ctx.slot;
    let instruction = instruction_row(
        &PROGRAM_ID,
        name,
        metadata,
        &ctx,
        accounts,
        &decoded.data,
        block_time,
    )?;

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
            close: Some(close_at(accounts, 2, name, StateTable::Admin, slot)?),
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
            close: Some(close_at(accounts, 4, name, StateTable::RoleAccount, slot)?),
        },
        XcavateWhitelistInstruction::RenounceRole(args) => {
            let user = account_at(accounts, 0, name)?;
            Fields {
                action_type: ActionType::RoleRenounced,
                actor: user.clone(),
                subject: Some(user),
                role: Some(role_from_chain(&args.role)),
                permission: None,
                close: Some(close_at(accounts, 2, name, StateTable::RoleAccount, slot)?),
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
        id: format!("{}-{}", ctx.tx_signature, ctx.index_str),
        action_type: f.action_type,
        subject: f.subject,
        role: f.role,
        permission: f.permission,
        actor: f.actor,
        slot,
        block_time,
        tx_signature: ctx.tx_signature,
        instruction_index: ctx.index_str,
    };

    Ok(Some(MappedInstruction {
        instruction,
        action: Some(action),
        closes: f.close.into_iter().collect(),
        webhook_events: vec![],
    }))
}

/// Decoded account -> state-table upsert. `lamports` comes from the raw account (the decoded
/// wrapper carries it too, but the raw account is the authoritative copy carbon received).
///
/// Shared with the `getProgramAccounts` snapshot loader (via [`snapshot_write_op`]): the
/// snapshot must produce byte-identical rows to the live account stream, and the only way to
/// guarantee that is to run the same mapping.
///
/// `closed_at_slot` is not a field here on purpose: the upserts hardcode `NULL` for it in the
/// `VALUES` list and include it in the `DO UPDATE SET` column list, so any live update at a
/// newer slot revives a soft-closed row. That is the correct behaviour for a PDA that is
/// closed and later re-created at the same address.
pub fn account_write_op(
    pubkey: Pubkey,
    slot: i64,
    lamports: i64,
    decoded: &DecodedAccount<XcavateWhitelistAccount>,
) -> WriteOp {
    let pubkey = pubkey.to_bytes().to_vec();
    match &decoded.data {
        XcavateWhitelistAccount::Config(config) => WriteOp::UpsertConfig(ConfigAccount {
            pubkey,
            slot,
            lamports,
            authority: config.authority.to_bytes().to_vec(),
            pending_authority: config.pending_authority.map(|p| p.to_bytes().to_vec()),
            bump: config.bump as i16,
        }),
        XcavateWhitelistAccount::Admin(admin) => WriteOp::UpsertAdmin(AdminAccount {
            pubkey,
            slot,
            lamports,
            admin: admin.admin.to_bytes().to_vec(),
            bump: admin.bump as i16,
        }),
        XcavateWhitelistAccount::RoleAccount(role) => WriteOp::UpsertRoleAccount(RoleAccountRow {
            pubkey,
            slot,
            lamports,
            user_pubkey: role.user.to_bytes().to_vec(),
            role: role_from_chain(&role.role),
            permission: permission_from_chain(&role.permission),
            rent_payer: role.rent_payer.to_bytes().to_vec(),
            bump: role.bump as i16,
        }),
    }
}

/// Decode one `getProgramAccounts` result with this program's decoder and map it exactly like
/// a live account update. `None` = owned by the program but undecodable (IDL drift).
pub fn snapshot_write_op(
    pubkey: Pubkey,
    slot: i64,
    lamports: i64,
    account: &Account,
) -> Option<WriteOp> {
    let decoded = XcavateWhitelistDecoder.decode_account(account)?;
    Some(account_write_op(pubkey, slot, lamports, &decoded))
}
