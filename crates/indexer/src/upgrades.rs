//! Detection of on-chain upgrades of the indexed programs (ADR-24).
//!
//! Every one of the five programs is owned by the BPF upgradeable loader, so a redeploy of
//! its bytecode is not an out-of-band event -- it is a transaction executing the loader's
//! `Upgrade` instruction with the program account in its account list. That transaction
//! therefore already passes the per-program Yellowstone filters (`account_required` matches
//! the program account) and already appears in `getSignaturesForAddress(program)` -- both
//! data paths deliver it today; it just decodes to nothing. This module gives it a decoder.
//!
//! [`LoaderUpgradeDecoder`] is a hand-written carbon decoder (the one non-generated decoder
//! in the workspace -- there is no IDL for the native loader, and the interesting surface is
//! a single instruction). It self-filters exactly like the generated ones: `None` unless the
//! instruction is the loader's, is `Upgrade`, and targets a program in
//! [`crate::programs::PROGRAMS`]. [`UpgradeRecorder`] then pushes one
//! [`WriteOp::RecordProgramUpgrade`]; the batcher commits it into `program_upgrades`
//! idempotently and fires the detection metric/log only for boundaries it had never seen
//! (crawl re-walks re-deliver historical upgrade transactions by design).
//!
//! Registered on `pipeline::common_pipes` unconditionally, so it rides both the live stream
//! and every backfill/reconcile crawl: an upgrade landing while the indexer is down is
//! re-observed by the next crawl over that range, and a full `indexer backfill` re-walk
//! recovers the complete upgrade history from nothing. The module-doc caveat in
//! [`crate::pipeline`] about registering unconfigured programs' pairs does not apply here:
//! this pipe writes only `program_upgrades` facts, never `program_instructions` history, so
//! recording an upgrade of a not-currently-configured registry program (conceivable only in
//! a transaction that upgrades two programs at once) is strictly informative.
//!
//! What this module deliberately does NOT do: swap any decoder. With a single generated
//! decoder per program, a recorded upgrade means "the checked-in IDL may no longer match the
//! deployed program" -- a fact for the maintenance loop (the ProgramUpgradeDetected alert,
//! the startup warning in `main::start`, and `agent/skills/`' procedures) to act on. The
//! slot boundaries recorded here are what a future versioned-decoder router keys on (ADR-25).

use async_trait::async_trait;
use carbon_core::error::{CarbonResult, Error as CarbonError};
use carbon_core::instruction::{
    DecodedInstruction, InstructionDecoder, InstructionProcessorInputType,
};
use carbon_core::metrics::MetricsCollection;
use carbon_core::processor::Processor;
use solana_pubkey::Pubkey;
use std::sync::Arc;

use crate::batcher::{Batcher, WriteOp};
use crate::programs;

/// The BPF upgradeable loader's program id, from the canonical constants crate rather than a
/// re-typed base58 literal.
pub const BPF_LOADER_UPGRADEABLE_ID: Pubkey = solana_sdk_ids::bpf_loader_upgradeable::ID;

/// `UpgradeableLoaderInstruction` is bincode-serialized: a unit-variant instruction is
/// exactly its 4-byte little-endian discriminant, and `Upgrade` is variant 3 with no
/// fields. The on-chain loader parses instruction data with
/// `limited_deserialize` + `allow_trailing_bytes`, so `[3,0,0,0]` followed by ANY junk
/// still executes as a valid `Upgrade` -- the decoder therefore prefix-matches rather
/// than requiring exact equality, or a hand-built transaction (precisely the evasive
/// case the detection exists for) would upgrade the program without being recorded.
const UPGRADE_INSTRUCTION_DATA: [u8; 4] = [3, 0, 0, 0];

/// `Upgrade`'s account order is fixed by the loader:
/// `[programdata, program, buffer, spill, rent sysvar, clock sysvar, authority]` -- the
/// program being upgraded is account index 1.
const UPGRADE_TARGET_ACCOUNT_INDEX: usize = 1;

/// One decoded fact: this transaction upgraded a registry program's bytecode. Carries the
/// registry name and address (not a `&ProgramSpec` -- the spec holds fn pointers and derives
/// nothing) so the processor needs no second registry lookup.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramUpgraded {
    /// Registry name of the upgraded program.
    pub program: &'static str,
    /// Its address: guaranteed by the decoder to be a registry program's.
    pub id: Pubkey,
}

pub struct LoaderUpgradeDecoder;

impl InstructionDecoder<'_> for LoaderUpgradeDecoder {
    type InstructionType = ProgramUpgraded;

    fn decode_instruction(
        &self,
        instruction: &solana_instruction::Instruction,
    ) -> Option<DecodedInstruction<Self::InstructionType>> {
        if !instruction.program_id.eq(&BPF_LOADER_UPGRADEABLE_ID) {
            return None;
        }
        if !instruction.data.starts_with(&UPGRADE_INSTRUCTION_DATA) {
            return None;
        }
        let target_key = &instruction
            .accounts
            .get(UPGRADE_TARGET_ACCOUNT_INDEX)?
            .pubkey;
        // Upgrades of programs outside the registry can reach this decoder only inside a
        // transaction that ALSO touches a registry program (that is what the datasource
        // filters select on); they are none of our business either way.
        let target = programs::by_id(target_key)?;
        Some(DecodedInstruction {
            program_id: instruction.program_id,
            data: ProgramUpgraded {
                program: target.name,
                id: target.id,
            },
            accounts: instruction.accounts.clone(),
        })
    }
}

/// Thin, like every processor: converts the decoded upgrade into one `WriteOp` and returns.
pub struct UpgradeRecorder {
    batcher: Batcher,
}

impl UpgradeRecorder {
    pub fn new(batcher: Batcher) -> Self {
        Self { batcher }
    }
}

#[async_trait]
impl Processor for UpgradeRecorder {
    type InputType = InstructionProcessorInputType<ProgramUpgraded>;

    async fn process(
        &mut self,
        (metadata, decoded, _nested, _raw): Self::InputType,
        _metrics: Arc<MetricsCollection>,
    ) -> CarbonResult<()> {
        let tx = &metadata.transaction_metadata;
        let up = &decoded.data;
        // info!, not warn!: crawl re-walks re-observe every historical upgrade. The one-time
        // "a NEW boundary was recorded" warning (and the metric) comes from the batcher,
        // after the row's transaction has committed.
        log::info!(
            "observed BPFLoaderUpgradeable upgrade of {} ({}) at slot {} (tx {})",
            up.program,
            up.id,
            tx.slot,
            tx.signature,
        );
        self.batcher
            .push(WriteOp::RecordProgramUpgrade {
                program: up.program,
                program_id: up.id.to_bytes().to_vec(),
                upgrade_slot: tx.slot as i64,
                signature: tx.signature.to_string(),
            })
            .await
            .map_err(|e| CarbonError::Custom(format!("batcher channel closed: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_instruction::{AccountMeta, Instruction};

    fn meta(pubkey: Pubkey) -> AccountMeta {
        AccountMeta {
            pubkey,
            is_signer: false,
            is_writable: true,
        }
    }

    /// `[programdata, program, buffer, spill, rent, clock, authority]` with `program` = target.
    fn upgrade_instruction(program_id: Pubkey, target: Pubkey, data: Vec<u8>) -> Instruction {
        let filler = |n: u8| Pubkey::new_from_array([n; 32]);
        Instruction {
            program_id,
            accounts: vec![
                meta(filler(1)),
                meta(target),
                meta(filler(2)),
                meta(filler(3)),
                meta(filler(4)),
                meta(filler(5)),
                meta(filler(6)),
            ],
            data,
        }
    }

    #[test]
    fn the_loader_id_is_the_canonical_one() {
        // A typo'd loader id would silently decode nothing forever; pin the base58 spelling.
        assert_eq!(
            BPF_LOADER_UPGRADEABLE_ID.to_string(),
            "BPFLoaderUpgradeab1e11111111111111111111111"
        );
    }

    #[test]
    fn an_upgrade_of_a_registry_program_decodes_to_its_spec() {
        for spec in programs::PROGRAMS {
            let ix = upgrade_instruction(
                BPF_LOADER_UPGRADEABLE_ID,
                spec.id,
                UPGRADE_INSTRUCTION_DATA.to_vec(),
            );
            let decoded = LoaderUpgradeDecoder
                .decode_instruction(&ix)
                .unwrap_or_else(|| panic!("upgrade of {} must decode", spec.name));
            assert_eq!(decoded.data.program, spec.name);
            assert_eq!(decoded.data.id, spec.id);
        }
    }

    #[test]
    fn an_upgrade_of_a_foreign_program_is_ignored() {
        let ix = upgrade_instruction(
            BPF_LOADER_UPGRADEABLE_ID,
            Pubkey::new_from_array([9; 32]),
            UPGRADE_INSTRUCTION_DATA.to_vec(),
        );
        assert!(LoaderUpgradeDecoder.decode_instruction(&ix).is_none());
    }

    #[test]
    fn other_loader_instructions_are_ignored() {
        let target = programs::PROGRAMS[0].id;
        // Every unit variant around Upgrade, plus a truncated tag. NOT in this list:
        // trailing garbage after a real Upgrade tag -- the runtime's
        // `allow_trailing_bytes` accepts that as a valid Upgrade, so we must too (see
        // the constant's doc and the test below).
        for data in [
            vec![2, 0, 0, 0], // DeployWithMaxDataLen (has fields anyway)
            vec![4, 0, 0, 0], // SetAuthority
            vec![5, 0, 0, 0], // Close
            vec![3, 0, 0],    // truncated
            vec![],
        ] {
            let ix = upgrade_instruction(BPF_LOADER_UPGRADEABLE_ID, target, data.clone());
            assert!(
                LoaderUpgradeDecoder.decode_instruction(&ix).is_none(),
                "data {data:?} must not decode as an upgrade"
            );
        }
    }

    #[test]
    fn trailing_bytes_after_the_upgrade_tag_still_decode() {
        // Parity with the on-chain parser: `limited_deserialize` allows trailing bytes,
        // so a successful transaction can carry them and MUST still be recorded.
        let spec = &programs::PROGRAMS[0];
        let ix = upgrade_instruction(
            BPF_LOADER_UPGRADEABLE_ID,
            spec.id,
            vec![3, 0, 0, 0, 0xde, 0xad],
        );
        let decoded = LoaderUpgradeDecoder
            .decode_instruction(&ix)
            .expect("trailing bytes must not hide an upgrade");
        assert_eq!(decoded.data.program, spec.name);
    }

    #[test]
    fn an_upgrade_instruction_of_another_program_is_ignored() {
        // Same data, same account shape, but not the loader executing it.
        let ix = upgrade_instruction(
            Pubkey::new_from_array([8; 32]),
            programs::PROGRAMS[0].id,
            UPGRADE_INSTRUCTION_DATA.to_vec(),
        );
        assert!(LoaderUpgradeDecoder.decode_instruction(&ix).is_none());
    }

    #[test]
    fn a_short_account_list_is_ignored() {
        let ix = Instruction {
            program_id: BPF_LOADER_UPGRADEABLE_ID,
            accounts: vec![meta(Pubkey::new_from_array([1; 32]))],
            data: UPGRADE_INSTRUCTION_DATA.to_vec(),
        };
        assert!(LoaderUpgradeDecoder.decode_instruction(&ix).is_none());
    }
}
