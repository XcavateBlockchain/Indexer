//! The mapping contract, asserted variant by variant.
//!
//! These are the parity tests: every value here was read off the old
//! `src/mappings/mappingHandlers.ts`, so a change that breaks parity with the SubQuery
//! database breaks a test rather than showing up as a quiet difference in query results
//! months later.

use carbon_core::instruction::InstructionMetadata;
use carbon_xcavate_whitelist_decoder::instructions::{
    AcceptAuthority, AddAdmin, AssignRole, InitializeConfig, RemoveAdmin, RemoveRole, RenounceRole,
    SetPermission, UpdateAuthority, XcavateWhitelistInstruction,
};
use carbon_xcavate_whitelist_decoder::types::{
    AccessPermission as ChainPermission, Role as ChainRole,
};
use chrono::{DateTime, TimeZone, Utc};
use solana_instruction::AccountMeta;
use solana_pubkey::Pubkey;

use super::*;
use crate::db::models::{AccessPermission, ActionType, Role};
use crate::test_fixtures::{decoded, pk, sig, SLOT};

fn block_time() -> DateTime<Utc> {
    Utc.timestamp_opt(1_750_000_000, 0).unwrap()
}

/// `block_time: None` on the transaction metadata is deliberate: the Yellowstone stream leaves
/// it empty, and the mapper must not read it -- the timestamp is resolved by the caller and
/// passed in separately.
fn metadata(absolute_path: &[u8]) -> InstructionMetadata {
    crate::test_fixtures::instruction_metadata(
        crate::test_fixtures::tx_metadata(sig(), SLOT, None),
        absolute_path,
    )
}

/// Maps a top-level (path `[0]`) instruction and unwraps the result.
fn map_top_level(data: XcavateWhitelistInstruction, accounts: &[Pubkey]) -> MappedInstruction {
    map_instruction(&metadata(&[0]), &decoded(data, accounts), block_time())
        .expect("mapping must succeed")
        .expect("this variant must produce rows")
}

// --- one test per instruction variant ------------------------------------------------------

#[test]
fn initialize_config_actor_and_subject_are_the_authority() {
    let accounts = [pk(1), pk(2), pk(3), pk(4), pk(5)];
    let m = map_top_level(
        XcavateWhitelistInstruction::InitializeConfig(InitializeConfig {}),
        &accounts,
    );

    assert_eq!(m.action.action_type, ActionType::ConfigInitialized);
    assert_eq!(m.action.actor, pk(1).to_string());
    assert_eq!(
        m.action.subject.as_deref(),
        Some(pk(1).to_string().as_str())
    );
    assert_eq!(m.action.role, None);
    assert_eq!(m.action.permission, None);
    assert_eq!(m.instruction.ix_name, "initialize_config");
    assert!(m.close.is_none());
}

#[test]
fn update_authority_subject_is_the_argument_not_an_account() {
    // The proposed authority never appears in the account list -- reading it from accounts
    // would silently record the config PDA instead.
    let accounts = [pk(1), pk(2)];
    let proposed = pk(99);
    let m = map_top_level(
        XcavateWhitelistInstruction::UpdateAuthority(UpdateAuthority {
            new_authority: proposed,
        }),
        &accounts,
    );

    assert_eq!(m.action.action_type, ActionType::AuthorityUpdateProposed);
    assert_eq!(m.action.actor, pk(1).to_string());
    assert_eq!(
        m.action.subject.as_deref(),
        Some(proposed.to_string().as_str())
    );
    assert_eq!(m.instruction.ix_name, "update_authority");
    assert!(m.close.is_none());
}

#[test]
fn accept_authority_actor_and_subject_are_the_new_authority() {
    let accounts = [pk(4), pk(2)];
    let m = map_top_level(
        XcavateWhitelistInstruction::AcceptAuthority(AcceptAuthority {}),
        &accounts,
    );

    assert_eq!(m.action.action_type, ActionType::AuthorityUpdated);
    assert_eq!(m.action.actor, pk(4).to_string());
    assert_eq!(
        m.action.subject.as_deref(),
        Some(pk(4).to_string().as_str())
    );
    assert_eq!(m.instruction.ix_name, "accept_authority");
}

#[test]
fn add_admin_subject_is_account_index_2() {
    let accounts = [pk(1), pk(2), pk(3), pk(4), pk(5)];
    let m = map_top_level(
        XcavateWhitelistInstruction::AddAdmin(AddAdmin {}),
        &accounts,
    );

    assert_eq!(m.action.action_type, ActionType::AdminAdded);
    assert_eq!(m.action.actor, pk(1).to_string());
    assert_eq!(
        m.action.subject.as_deref(),
        Some(pk(3).to_string().as_str())
    );
    assert_eq!(m.instruction.ix_name, "add_admin");
    assert!(m.close.is_none());
}

#[test]
fn remove_admin_subject_is_the_argument_and_it_closes_the_admin_pda() {
    // accounts = [authority, config, admin_pda]; the removed admin's *wallet* is the argument,
    // accounts[2] is that admin's PDA -- the row that has to be soft-closed.
    let accounts = [pk(1), pk(2), pk(3)];
    let removed = pk(88);
    let m = map_top_level(
        XcavateWhitelistInstruction::RemoveAdmin(RemoveAdmin { admin_key: removed }),
        &accounts,
    );

    assert_eq!(m.action.action_type, ActionType::AdminRemoved);
    assert_eq!(m.action.actor, pk(1).to_string());
    assert_eq!(
        m.action.subject.as_deref(),
        Some(removed.to_string().as_str())
    );
    assert_eq!(
        m.close,
        Some(PendingClose::Admin {
            pubkey: pk(3).to_bytes().to_vec(),
            slot: SLOT as i64,
        })
    );
}

#[test]
fn assign_role_carries_the_role_argument_and_subject_index_2() {
    let accounts = [pk(1), pk(2), pk(3), pk(4), pk(5)];
    let m = map_top_level(
        XcavateWhitelistInstruction::AssignRole(AssignRole {
            role: ChainRole::Lawyer,
        }),
        &accounts,
    );

    assert_eq!(m.action.action_type, ActionType::RoleAssigned);
    assert_eq!(m.action.actor, pk(1).to_string());
    assert_eq!(
        m.action.subject.as_deref(),
        Some(pk(3).to_string().as_str())
    );
    assert_eq!(m.action.role, Some(Role::Lawyer));
    // The old handler defaulted the *entity* to COMPLIANT but never set `permission` on the
    // action row itself; the derived view supplies the default instead.
    assert_eq!(m.action.permission, None);
    assert!(m.close.is_none());
}

#[test]
fn remove_role_closes_the_role_account_at_index_4() {
    // accounts = [admin_signer, admin, user, rent_payer, role_account]
    let accounts = [pk(1), pk(2), pk(3), pk(4), pk(5)];
    let m = map_top_level(
        XcavateWhitelistInstruction::RemoveRole(RemoveRole {
            role: ChainRole::LettingAgent,
        }),
        &accounts,
    );

    assert_eq!(m.action.action_type, ActionType::RoleRemoved);
    assert_eq!(m.action.actor, pk(1).to_string());
    assert_eq!(
        m.action.subject.as_deref(),
        Some(pk(3).to_string().as_str())
    );
    assert_eq!(m.action.role, Some(Role::LettingAgent));
    assert_eq!(
        m.close,
        Some(PendingClose::RoleAccount {
            pubkey: pk(5).to_bytes().to_vec(),
            slot: SLOT as i64,
        })
    );
}

#[test]
fn renounce_role_is_self_acted_and_closes_the_role_account_at_index_2() {
    // accounts = [user, rent_payer, role_account]
    let accounts = [pk(1), pk(2), pk(3)];
    let m = map_top_level(
        XcavateWhitelistInstruction::RenounceRole(RenounceRole {
            role: ChainRole::SpvConfirmation,
        }),
        &accounts,
    );

    assert_eq!(m.action.action_type, ActionType::RoleRenounced);
    assert_eq!(m.action.actor, pk(1).to_string());
    assert_eq!(
        m.action.subject.as_deref(),
        Some(pk(1).to_string().as_str())
    );
    assert_eq!(m.action.role, Some(Role::SpvConfirmation));
    assert_eq!(
        m.close,
        Some(PendingClose::RoleAccount {
            pubkey: pk(3).to_bytes().to_vec(),
            slot: SLOT as i64,
        })
    );
}

#[test]
fn set_permission_carries_both_role_and_permission() {
    let accounts = [pk(1), pk(2), pk(3), pk(4)];
    let m = map_top_level(
        XcavateWhitelistInstruction::SetPermission(SetPermission {
            role: ChainRole::RealEstateInvestor,
            permission: ChainPermission::Revoked,
        }),
        &accounts,
    );

    assert_eq!(m.action.action_type, ActionType::PermissionUpdated);
    assert_eq!(m.action.actor, pk(1).to_string());
    assert_eq!(
        m.action.subject.as_deref(),
        Some(pk(3).to_string().as_str())
    );
    assert_eq!(m.action.role, Some(Role::RealEstateInvestor));
    assert_eq!(m.action.permission, Some(AccessPermission::Revoked));
    assert!(m.close.is_none());
}

#[test]
fn cpi_event_writes_nothing() {
    use carbon_xcavate_whitelist_decoder::events::config_initialized::ConfigInitializedEvent;
    use carbon_xcavate_whitelist_decoder::instructions::CpiEvent;

    let result = map_instruction(
        &metadata(&[0]),
        &decoded(
            XcavateWhitelistInstruction::CpiEvent(CpiEvent::ConfigInitialized(
                ConfigInitializedEvent { authority: pk(1) },
            )),
            &[pk(1), pk(2)],
        ),
        block_time(),
    )
    .expect("must not be an error");

    assert!(result.is_none(), "CpiEvent must produce no rows");
}

// --- common columns ------------------------------------------------------------------------

#[test]
fn common_columns_match_the_contract() {
    let accounts = [pk(1), pk(2), pk(3), pk(4), pk(5)];
    let m = map_top_level(
        XcavateWhitelistInstruction::AddAdmin(AddAdmin {}),
        &accounts,
    );

    // whitelist_actions
    assert_eq!(m.action.id, format!("{}-0", sig()));
    assert_eq!(m.action.tx_signature, sig().to_string());
    assert_eq!(m.action.instruction_index, "0");
    assert_eq!(m.action.slot, SLOT as i64);
    assert_eq!(m.action.block_time, block_time());

    // program_instructions
    assert_eq!(m.instruction.signature, sig().as_ref().to_vec());
    assert_eq!(m.instruction.ix_index, 0);
    assert_eq!(
        m.instruction.inner_index, -1,
        "top-level instructions use -1"
    );
    assert_eq!(m.instruction.slot, SLOT as i64);
    assert_eq!(m.instruction.block_time, block_time());
    assert_eq!(
        m.instruction.accounts,
        accounts
            .iter()
            .map(|p| p.to_bytes().to_vec())
            .collect::<Vec<_>>(),
        "accounts are stored in instruction order, as raw 32-byte keys"
    );
    // `data` is the natural serde output of the generated enum (externally tagged via
    // `#[serde(tag = "type", content = "data")]`). Nothing reads it -- the views are built
    // from whitelist_actions -- so only its presence and shape are asserted.
    assert_eq!(m.instruction.data["type"], "AddAdmin");
}

#[test]
fn nested_instructions_keep_the_full_path_in_the_action_and_collapse_to_two_levels_in_the_row() {
    let accounts = [pk(1), pk(2), pk(3), pk(4), pk(5)];

    // A CPI at path 3.1 -- second inner instruction under top-level instruction 3.
    let m = map_instruction(
        &metadata(&[3, 1]),
        &decoded(
            XcavateWhitelistInstruction::AssignRole(AssignRole {
                role: ChainRole::RegionalOperator,
            }),
            &accounts,
        ),
        block_time(),
    )
    .unwrap()
    .unwrap();

    assert_eq!(m.action.instruction_index, "3.1");
    assert_eq!(m.action.id, format!("{}-3.1", sig()));
    assert_eq!(m.instruction.ix_index, 3);
    assert_eq!(m.instruction.inner_index, 1);

    // Deeper nesting keeps the full dotted path in the action id (the old SubQuery identity)
    // while the composite key collapses onto the first two levels.
    let deep = map_instruction(
        &metadata(&[3, 1, 2]),
        &decoded(
            XcavateWhitelistInstruction::AssignRole(AssignRole {
                role: ChainRole::RegionalOperator,
            }),
            &accounts,
        ),
        block_time(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(deep.action.instruction_index, "3.1.2");
    assert_eq!(deep.instruction.ix_index, 3);
    assert_eq!(deep.instruction.inner_index, 1);
}

#[test]
fn instruction_index_formats_like_the_old_ix_path() {
    assert_eq!(instruction_index(&[0]), "0");
    assert_eq!(instruction_index(&[12]), "12");
    assert_eq!(instruction_index(&[3, 1]), "3.1");
    assert_eq!(instruction_index(&[3, 1, 2]), "3.1.2");
}

#[test]
fn every_role_maps_to_its_old_schema_spelling() {
    let cases = [
        (ChainRole::RegionalOperator, "REGIONAL_OPERATOR", 0u8),
        (ChainRole::RealEstateInvestor, "REAL_ESTATE_INVESTOR", 1),
        (ChainRole::RealEstateDeveloper, "REAL_ESTATE_DEVELOPER", 2),
        (ChainRole::Lawyer, "LAWYER", 3),
        (ChainRole::LettingAgent, "LETTING_AGENT", 4),
        (ChainRole::SpvConfirmation, "SPV_CONFIRMATION", 5),
    ];
    for (chain, spelling, borsh_index) in cases {
        let mapped = role_from_chain(&chain);
        assert_eq!(mapped.as_db_str(), spelling);
        // The borsh index is the PDA seed byte and the stored discriminant; a reorder here
        // would reinterpret every existing assignment on chain.
        assert_eq!(mapped.borsh_index(), borsh_index, "{spelling}");
    }

    assert_eq!(
        permission_from_chain(&ChainPermission::Compliant).as_db_str(),
        "COMPLIANT"
    );
    assert_eq!(
        permission_from_chain(&ChainPermission::Revoked).as_db_str(),
        "REVOKED"
    );
}

// --- failure modes ---------------------------------------------------------------------------

#[test]
fn a_short_account_list_is_a_loud_mapping_error() {
    // add_admin needs accounts[2]; give it two.
    let err = map_instruction(
        &metadata(&[0]),
        &decoded(
            XcavateWhitelistInstruction::AddAdmin(AddAdmin {}),
            &[pk(1), pk(2)],
        ),
        block_time(),
    )
    .expect_err("must not silently produce a row");

    assert_eq!(err.reason(), "missing_account");
    let msg = err.to_string();
    assert!(msg.contains("add_admin"), "{msg}");
    assert!(msg.contains('2'), "{msg}");
}

#[test]
fn an_empty_absolute_path_is_a_loud_mapping_error() {
    let mut md = metadata(&[0]);
    md.absolute_path = vec![];
    let err = map_instruction(
        &md,
        &decoded(
            XcavateWhitelistInstruction::AddAdmin(AddAdmin {}),
            &[pk(1), pk(2), pk(3)],
        ),
        block_time(),
    )
    .expect_err("must not invent an index");

    assert_eq!(err.reason(), "empty_absolute_path");
}

// --- end-to-end through the real decoder ------------------------------------------------------

#[test]
fn a_real_borsh_encoded_instruction_decodes_and_maps() {
    use carbon_core::instruction::InstructionDecoder;
    use carbon_xcavate_whitelist_decoder::XcavateWhitelistDecoder;

    // Discriminator + borsh-encoded `Role::Lawyer` (variant index 3), exactly what the chain
    // carries. Anchor's discriminator for `assign_role`, taken from the generated decoder.
    let mut data = vec![255, 174, 125, 180, 203, 155, 202, 131];
    data.extend_from_slice(
        &carbon_core::borsh::to_vec(&AssignRole {
            role: ChainRole::Lawyer,
        })
        .expect("borsh encode"),
    );

    let accounts: Vec<AccountMeta> = [pk(1), pk(2), pk(3), pk(4), pk(5)]
        .iter()
        .map(|p| AccountMeta {
            pubkey: *p,
            is_signer: false,
            is_writable: false,
        })
        .collect();

    let raw = solana_instruction::Instruction {
        program_id: carbon_xcavate_whitelist_decoder::PROGRAM_ID,
        accounts,
        data,
    };

    let decoded = XcavateWhitelistDecoder
        .decode_instruction(&raw)
        .expect("the decoder must recognise a well-formed assign_role");

    let m = map_instruction(&metadata(&[0]), &decoded, block_time())
        .unwrap()
        .unwrap();

    assert_eq!(m.instruction.ix_name, "assign_role");
    assert_eq!(m.action.action_type, ActionType::RoleAssigned);
    assert_eq!(m.action.role, Some(Role::Lawyer));
    assert_eq!(
        m.action.subject.as_deref(),
        Some(pk(3).to_string().as_str())
    );
}
