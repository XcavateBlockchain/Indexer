# IDLs

One new-format Anchor IDL per indexed program, named by registry name
(`crates/indexer/src/programs.rs` / `addresses.json`). Two invariants:

1. **These files must always decode the programs as DEPLOYED on devnet** — they never
   blindly track upstream `main` (an additive update may land ahead of the chain, since a
   superset decoder still decodes everything deployed; a breaking one goes through the
   versioned-decoder mechanism, ADR-25).
   `anchor build` stamps `address` from the source's `declare_id!`, which does not match
   the deployed addresses; a freshly built IDL must be normalized against `addresses.json`
   (`scripts/agent/idl-tools.py normalize`, done automatically by
   `scripts/agent/build-upstream-idls.sh`) before it lands here. The chain is the
   authority: `scripts/agent/check-program-upgrades.py` says what is deployed.
2. **Each file is the generation source of its decoder crate**: `crates/<p>-decoder`
   regenerates byte-identically from it (`scripts/agent/verify-decoder-purity.sh`), with
   `xcavate_whitelist.json` → `crates/whitelist-decoder`. Changing a file here without
   regenerating its decoder fails the purity check.

## `versions/`

Created when the first breaking upgrade is prepared (ADR-25,
`agent/skills/versioned-decoder/SKILL.md`): `versions/<program>/vN.json` archives the IDL
a frozen decoder crate (`crates/<p>-decoder-vN`) was generated from, and
`versions/<program>/README.md` records each version's slot range once the upgrade slot is
known (the boundaries themselves live in the `program_upgrades` table). The top-level
`<program>.json` always stays the CURRENT version.
