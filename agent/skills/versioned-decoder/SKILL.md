---
name: versioned-decoder
description: Split a program's decoding into slot-routed versions when a BREAKING IDL change is being prepared (or has landed) — freeze the old decoder, generate the new one, route on the recorded upgrade boundary. The heavyweight procedure; additive changes use regen-decoders instead.
---

# Versioned decoder — the breaking-change procedure

Use this when `upstream-sync` classified a program's diff as **breaking** (changed or
removed instructions/accounts/types — not just additions), after naming-drift has been
ruled out. This is ADR-25 made concrete; read that ADR and `docs/agentic-maintenance.md`
§5 before starting. Do NOT use this for additive diffs
(`agent/skills/regen-decoders/SKILL.md`) — a needless version split is carried complexity
forever.

The outcome you are building: the indexer ships to production **speaking V(n), holding
V(n+1)** — decoding everything the chain emits today, ready the instant the multisig
executes the upgrade, and correct if the multisig never executes it at all.

## Procedure

1. **Establish the ground truth.** Run `python3 scripts/agent/check-program-upgrades.py`.
   Note whether the breaking version is *coming* (chain unchanged — you are preparing) or
   *live* (chain moved — incident mode; the boundary slot already exists in
   `program_upgrades`, and everything below still applies, minus the waiting).

2. **Freeze the current decoder.**

   ```bash
   bash scripts/agent/freeze-decoder-version.sh <registry_name> <N>   # N = current version number
   ```

   This copies `crates/<p>-decoder` → `crates/<p>-decoder-vN` with exactly one changed
   line (the package name gains `-vN`; the script verifies nothing else moved) and
   archives the current IDL to `idls/versions/<p>/vN.json`. The frozen crate is never
   regenerated again; its provenance is the archived IDL + git history. Then follow the
   script's printed wiring checklist:
   - root `Cargo.toml`: add `"crates/<p>-decoder-vN"` to the workspace `exclude` list
     (the generated crates carry their own `[workspace]` tables — see the existing
     entries' comment);
   - `docker/rust.Dockerfile`: add a `COPY crates/<p>-decoder-vN ...` line next to the
     existing decoder COPYs (they are copied before `cargo chef cook` on purpose — read
     the comment there);
   - `crates/indexer/Cargo.toml`: add the frozen crate as a path dependency alongside the
     current one (its lib imports as `carbon_<p>_decoder_vN`).

3. **Bring in the new IDL and regenerate the current crate.** Copy the NORMALIZED new IDL
   (address = `addresses.json`'s, from `build-upstream-idls.sh`'s `$OUT_DIR`) over
   `idls/<p>.json`, then regenerate `crates/<p>-decoder` from it with the pinned command
   (README "Regenerating a decoder"; `xcavate_whitelist` maps to `crates/whitelist-decoder`):

   ```bash
   npx --yes @sevenlabs-hq/carbon-cli@0.12.0 parse \
     -i ./idls/<p>.json -o ./crates/<p>-decoder \
     -s anchor -c --with-postgres true --with-graphql true --with-serde true
   bash scripts/agent/verify-decoder-purity.sh <registry_name>
   ```

   Check the regenerated crate's `Cargo.toml` still targets `carbon-core = "0.12.0"`; if
   not, STOP — that is a whole-workspace pin bump (ADR-12), a separate task.

4. **Build the versioned mapper.** The routing point is the mapper, not the decoder
   (decoders are slot-blind; every `ProgramMapper::map_instruction` sees the slot via its
   metadata, and `account_write_op` receives it as an argument — see
   `crates/indexer/src/mapping/mod.rs`). The shape, using marketplace as the example:

   - Keep the current mapper (`mapping/marketplace.rs`) as the V(n+1) mapper — it compiles
     against the regenerated decoder's types; the exhaustive `ix_name` matches will FORCE
     you through every new/changed instruction (that is the point — do the mapping work,
     reading the upstream source's `close =` constraints for every changed instruction's
     account positions).
   - Add `mapping/marketplace_v1.rs` (copy of the pre-change mapper) typed against
     `carbon_marketplace_decoder_v1`'s enums. Frozen logic for frozen bytes: do not "fix"
     anything in it.
   - Register BOTH versions' typed decoder+processor pairs for the program in
     `pipeline::common_pipes`. This is safe only when the two versions' surfaces are
     discriminator-disjoint or layout-identical per shared discriminator — **verify that
     explicitly** by comparing the two IDLs' discriminators: for any instruction/account
     whose discriminator survived with a CHANGED layout, dual registration would
     mis-decode, and you must instead register a single wrapper decoder whose
     `decode_instruction` tries the layouts and a wrapper mapper that routes by slot
     (`Either`-style enum over the two decoded types). State in the PR which of the two
     situations held and how you verified it.
   - Route by slot against the boundary: read the program's boundaries from
     `db::upgrades::upgrades_for` once at startup (plumb it next to the other
     per-program state in `main::start`); boundary absent → the V(n+1) mapper must treat
     every instruction as future (i.e. V(n) handles everything). Boundary slot itself:
     slot ≥ boundary routes new, with a decode-attempt fallback to old for that exact
     slot (`docs/agentic-maintenance.md` §5 has the why).
   - Stamp `program_instructions.decoder_version` (the 0011 column) in the mapper's
     `instruction_row` output for BOTH versions from now on (NULL remains "v1, written
     before versioning").
   - Snapshot path: `ProgramSpec::snapshot_write_op` gets newest-first fallback — try the
     new decoder, fall back to vN; log which version decoded each account only at debug.

5. **Schema.** Whatever the new version needs (new tables, columns, widened CHECKs) via
   `agent/skills/write-migration/SKILL.md` — in the SAME PR, because migrations auto-apply
   at startup and the decoder must never be able to produce a row the schema rejects (the
   CHECK-stall trap is documented there). Version metadata never goes into state tables
   (ADR-2: they stay disposable snapshot-rebuildable mirrors).

6. **Tests.** Everything `regen-decoders` requires for the new surface, plus the
   version-specific ones: a routing test per boundary side (slot below → v1 mapper's row,
   slot at/above → new mapper's row), the boundary-slot fallback, a frozen-decoder
   decode-through test against bytes from the archived IDL era (reuse the existing
   real-devnet fixtures in `integration_tests.rs` — they ARE v1 bytes and must keep
   decoding through the v1 path forever), and `decoder_version` stamping.

7. **Record the slot range** once known: when the upgrade lands, the post-upgrade PR
   annotates `idls/versions/<p>/vN.json`'s archival entry (a `README.md` in
   `idls/versions/<p>/` listing `vN: slots [deploy_or_prev_boundary, upgrade_slot)`) AND
   bumps the program's `decoder_covers_boundary` in `crates/indexer/src/programs.rs` to
   the recorded upgrade slot — that is what silences the startup "decoder is stale"
   warning for a remediated boundary.

8. **Ship it forward-compatible.** Hand off to `agent/skills/verify-and-ship/SKILL.md`.
   The PR must say: what the chain runs now, what this prepares for, that V(n+1) is
   dormant until the boundary is recorded, and what the multisig go/no-go checklist is.
   After the multisig executes: the post-upgrade duties in verify-and-ship (confirm the
   recorded boundary, restart/redeploy so routing activates, run the production backfill
   re-walk to heal the in-between window, watch `DecodeFailures`).

## Checklist before you finish

- [ ] Frozen crate differs from its source by exactly the package-name line; archived IDL
      committed; wiring (workspace exclude, Dockerfile COPY, Cargo.toml dep) complete.
- [ ] Current crate regenerated from the new normalized IDL; purity check green;
      carbon-core still =0.12.0.
- [ ] Discriminator-disjointness explicitly verified and stated (or wrapper-decoder route
      taken).
- [ ] Every changed instruction's close positions re-read from the upstream program
      SOURCE, not the IDL.
- [ ] Routing dormant without a boundary; boundary-slot fallback implemented; snapshots
      newest-first; `decoder_version` stamped.
- [ ] v1 fixtures still decode through the v1 path; routing tests on both sides of the
      boundary.
- [ ] Migration in the same PR; `bash scripts/lint-migrations.sh` green.
- [ ] Full gauntlet + `verify-devnet.sh` green; handed off to verify-and-ship.

## Traps

- **Dual-registering non-disjoint decoders.** A surviving discriminator with a changed
  layout decodes "successfully" under both versions — silent wrong data, the worst
  failure this system has. Verify disjointness; wrapper-decode otherwise.
- **"Fixing" the frozen mapper.** Its bugs (if any) are what wrote the existing rows;
  changing it makes history re-walks disagree with stored data. Frozen means frozen.
- **Swapping `idls/<p>.json` without freezing first.** The old decoder's provenance chain
  breaks and pre-upgrade history loses its decoder.
- **Regenerating the frozen crate.** It regenerates from the NEW idl and silently becomes
  a copy of the current one. It is excluded from the purity script's scope on purpose.
- **Letting the versioned mapper touch state-table schemas.** Version metadata lives in
  `program_upgrades` + `decoder_version` only (ADR-25); state tables stay ADR-2 mirrors.
- **Shipping the decoder and the migration in different PRs.** Migrations apply at
  startup; a decoder that can emit a value the schema rejects stalls the whole batcher
  (all four programs) in a retry loop.
