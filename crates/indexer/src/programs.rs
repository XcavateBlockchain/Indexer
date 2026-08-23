//! The static registry of indexed programs.
//!
//! One entry per program of the realXmarket protocol, in deploy order. The addresses are the
//! decoder crates' compiled-in `PROGRAM_ID` constants -- NOT re-parsed strings -- so the
//! registry can never drift from what the decoders actually accept (the old `PROGRAM_ID` env
//! override was removed for exactly that reason: it re-aimed every filter and crawl, but the
//! decoder still hard-checks its compiled-in id, so overriding it produced a subscription
//! that decoded nothing).
//!
//! `deploy_slot` is each program's deployment slot on devnet (the oldest signature returned
//! by `getSignaturesForAddress`, cross-checked against MIGRATION_LOG.md's recon table): the
//! backfill floor and the initial `last_contiguous_slot` seed. There is nothing to index
//! below it.
//!
//! The set of programs a process actually indexes defaults to all of them and can be
//! narrowed with the `PROGRAMS` env var (comma-separated names) -- see [`crate::config`].

use solana_account::Account;
use solana_pubkey::Pubkey;

use crate::batcher::WriteOp;
use crate::db::close::StateTable;

/// Everything the program-agnostic machinery needs to know about one program.
///
/// `snapshot_write_op` is the only place decoder dispatch has to cross a `fn` boundary: the
/// `getProgramAccounts` snapshot loop is generic over programs at runtime (a plain loop),
/// while the pipeline pipes are generic at compile time (one typed
/// `.instruction()`/`.account()` pair per program in `pipeline::common_pipes`).
pub struct ProgramSpec {
    /// Snake_case name, matching the IDL file under `idls/`. Used as the metrics label, the
    /// Yellowstone filter-key prefix, and the `PROGRAMS` env var spelling.
    pub name: &'static str,
    pub id: Pubkey,
    /// Deployment slot on devnet: the backfill floor.
    pub deploy_slot: u64,
    /// The newest `program_upgrades` boundary this crate's checked-in IDL (and therefore
    /// its generated decoder) was written for. Equal to `deploy_slot` while a program has
    /// never been upgraded; the maintenance procedures (`agent/skills/regen-decoders`,
    /// `agent/skills/versioned-decoder`) bump it to the recorded upgrade slot whenever the
    /// IDL is updated for a post-upgrade program. `main::start` warns at startup only for
    /// recorded 'chain' boundaries ABOVE this stamp -- without it, an append-only boundary
    /// table would make the "decoder is stale" warning fire forever after remediation.
    pub decoder_covers_boundary: u64,
    /// Decode one `getProgramAccounts` result with this program's decoder and map it to the
    /// same state-table upsert the live account stream would produce. `None` = the account is
    /// owned by the program but does not decode (IDL drift -- the caller logs it loudly).
    pub snapshot_write_op: fn(Pubkey, i64, i64, &Account) -> Option<WriteOp>,
    /// This program's account-state tables, for the snapshot's close-missing sweep: any
    /// still-open row in these tables whose account is absent from a fresh
    /// `getProgramAccounts` result is provably closed on-chain and gets soft-closed at the
    /// snapshot slot (slot-guarded, so rows written after the snapshot's read are untouched).
    /// This is the healing path for closes no other mechanism can land -- e.g. a same-slot
    /// create+close tie, where the strict slot guards leave the row open (see `db::close`).
    pub tables: &'static [StateTable],
}

/// All indexable programs, in deploy order. The whitelist first: it is the compliance root
/// the others CPI into, and the oldest deployment.
pub static PROGRAMS: &[ProgramSpec] = &[
    ProgramSpec {
        name: "xcavate_whitelist",
        id: carbon_xcavate_whitelist_decoder::PROGRAM_ID,
        deploy_slot: 483_386_556,
        decoder_covers_boundary: 483_386_556,
        snapshot_write_op: crate::mapping::whitelist::snapshot_write_op,
        tables: &[
            StateTable::Config,
            StateTable::Admin,
            StateTable::RoleAccount,
        ],
    },
    ProgramSpec {
        name: "regions",
        id: carbon_regions_decoder::PROGRAM_ID,
        deploy_slot: 483_386_626,
        decoder_covers_boundary: 483_386_626,
        snapshot_write_op: crate::mapping::regions::snapshot_write_op,
        tables: &[
            StateTable::RegionsConfig,
            StateTable::RegionsLocation,
            StateTable::RegionsRegion,
            StateTable::RegionsRegionProposal,
            StateTable::RegionsRegionState,
            StateTable::RegionsVoteRecord,
        ],
    },
    ProgramSpec {
        name: "marketplace",
        id: carbon_marketplace_decoder::PROGRAM_ID,
        deploy_slot: 483_386_726,
        decoder_covers_boundary: 483_386_726,
        snapshot_write_op: crate::mapping::marketplace::snapshot_write_op,
        tables: &[
            StateTable::MarketplaceConfig,
            StateTable::MarketplaceInvestorPosition,
            StateTable::MarketplaceLawyer,
            StateTable::MarketplaceLawyerCandidacy,
            StateTable::MarketplaceLawyerVote,
            StateTable::MarketplaceListing,
            StateTable::MarketplacePropertyAsset,
            StateTable::MarketplaceReservation,
            StateTable::MarketplaceShareHolding,
        ],
    },
    ProgramSpec {
        name: "property",
        id: carbon_property_decoder::PROGRAM_ID,
        deploy_slot: 483_386_809,
        decoder_covers_boundary: 483_386_809,
        snapshot_write_op: crate::mapping::property::snapshot_write_op,
        tables: &[
            StateTable::PropertyConfig,
            StateTable::PropertyAgentCandidacy,
            StateTable::PropertyAgentVote,
            StateTable::PropertyLettingAgent,
            StateTable::PropertyLetting,
            StateTable::PropertyResignationNotice,
        ],
    },
];

/// Look a program up by its snake_case name.
pub fn by_name(name: &str) -> Option<&'static ProgramSpec> {
    PROGRAMS.iter().find(|p| p.name == name)
}

/// Look a program up by its address.
pub fn by_id(id: &Pubkey) -> Option<&'static ProgramSpec> {
    PROGRAMS.iter().find(|p| &p.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_matches_addresses_json() {
        // addresses.json is the canonical address source (ADR-19); the registry must agree
        // with it. Parsed here rather than at runtime so a drift is a test failure, not a
        // production surprise.
        let addresses: serde_json::Value =
            serde_json::from_str(include_str!("../../../addresses.json"))
                .expect("addresses.json parses");
        let programs = addresses["programs"]
            .as_object()
            .expect("addresses.json has a programs block");
        assert_eq!(programs.len(), PROGRAMS.len());
        // deploy_slots exists for the maintenance tooling (scripts/agent/ probes the chain
        // and needs the expected version-1 slots without parsing this file's Rust); the
        // registry stays the compiled-in runtime source and the two must agree.
        let deploy_slots = addresses["deploy_slots"]
            .as_object()
            .expect("addresses.json has a deploy_slots block");
        assert_eq!(deploy_slots.len(), PROGRAMS.len());
        for spec in PROGRAMS {
            let addr = programs[spec.name]
                .as_str()
                .unwrap_or_else(|| panic!("addresses.json lists {}", spec.name));
            assert_eq!(
                addr,
                spec.id.to_string(),
                "registry address for {} must match addresses.json",
                spec.name
            );
            let slot = deploy_slots[spec.name]
                .as_u64()
                .unwrap_or_else(|| panic!("addresses.json lists a deploy slot for {}", spec.name));
            assert_eq!(
                slot, spec.deploy_slot,
                "registry deploy slot for {} must match addresses.json",
                spec.name
            );
        }
    }

    #[test]
    fn every_state_table_belongs_to_exactly_one_program() {
        // The close-missing sweep partitions StateTable::ALL by program; a table missing
        // from every program's list would never be swept, and one listed twice would be
        // swept against the wrong program's account set.
        let mut counts = std::collections::HashMap::new();
        for spec in PROGRAMS {
            for table in spec.tables {
                *counts.entry(table.table_name()).or_insert(0usize) += 1;
            }
        }
        for table in StateTable::ALL {
            assert_eq!(
                counts.get(table.table_name()),
                Some(&1),
                "{} must appear in exactly one program's table list",
                table.table_name()
            );
        }
    }

    #[test]
    fn decoder_coverage_stamp_never_precedes_the_deploy() {
        // decoder_covers_boundary starts life equal to deploy_slot and only ever moves
        // FORWARD to a recorded upgrade slot (see the field's doc); a value below the
        // deploy slot would claim the decoder covers a version that never existed.
        for spec in PROGRAMS {
            assert!(
                spec.decoder_covers_boundary >= spec.deploy_slot,
                "{}: decoder_covers_boundary must be >= deploy_slot",
                spec.name
            );
        }
    }

    #[test]
    fn names_and_ids_are_unique() {
        for (i, a) in PROGRAMS.iter().enumerate() {
            for b in &PROGRAMS[i + 1..] {
                assert_ne!(a.name, b.name);
                assert_ne!(a.id, b.id);
            }
        }
    }

    #[test]
    fn lookups_work() {
        assert_eq!(by_name("marketplace").unwrap().name, "marketplace");
        assert!(by_name("nonexistent").is_none());
        let wl = by_name("xcavate_whitelist").unwrap();
        assert_eq!(by_id(&wl.id).unwrap().name, "xcavate_whitelist");
    }
}
