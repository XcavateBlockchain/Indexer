---
name: regen-decoders
description: Regenerate a program's carbon decoder crate after an ADDITIVE-only IDL change (build-upstream-idls.sh exit 10) or a deliberate carbon-stack pin bump, and rewire the mapping layer. Not for breaking diffs.
---

# Regenerate a decoder (additive IDL change)

## When to use / when NOT to use

Use this skill when `scripts/agent/build-upstream-idls.sh` exited **10** (every program's
classification in its `summary.json` is `additive` or `identical`), or when you are
executing a deliberate, whole-workspace carbon pin bump (ADR-12 upgrade path).

Do NOT use it when:

- Any program classified `breaking` (exit **20**): read and follow
  `agent/skills/versioned-decoder/SKILL.md` instead. An additive decoder still decodes
  everything the currently-deployed program emits (new discriminators simply never appear
  until the upgrade executes); a breaking one does not — that is the entire difference.
- You only need a schema change with no IDL change: read
  `agent/skills/write-migration/SKILL.md`.

Why additive is safe to ship ahead of the chain (ADR-23, summarized in
`scripts/agent/README.md`): the regenerated decoder is a superset, so it keeps decoding the
deployed program today AND is ready the instant the multisig executes the upgrade. The
indexer must be merged, deployed, and healthy BEFORE that execution — never after.

## Procedure

All paths relative to repo root (`/home/rosta/Indexer`). `<p>` = registry name from
`addresses.json` (`xcavate_whitelist`, `regions`, `marketplace`, `property`).
Crate-dir mapping (only the whitelist differs): `xcavate_whitelist` → `crates/whitelist-decoder`;
everything else → `crates/<p>-decoder`. Mapping module: `crates/indexer/src/mapping/whitelist.rs`
for the whitelist, `<p>.rs` for the rest.

1. **Confirm the additive classification.** Read the run's summary:

   ```bash
   jq . ~/.cache/realxmarket-agent/idls-<sha>/summary.json
   ```

   (`OUT_DIR` printed by `build-upstream-idls.sh`; it records `upstream_sha` — pin it in
   the PR description.) Every `classification` must be `additive` or `identical`. Any
   `breaking` → stop, switch to `agent/skills/versioned-decoder/SKILL.md` (read it first).

2. **Ask the chain which leg of ADR-23 you are on:**

   ```bash
   python3 scripts/agent/check-program-upgrades.py --graphql http://localhost:3010/graphql
   ```

   Judge the printed lines, not the exit code alone (with `--graphql`, an upgrade the
   indexer already recorded reports exit 0):

   - Every program prints `unchanged (still the version-1 deploy)` (exit 0): the change is
     only upstream (COMING). Normal case — prepare ahead.
   - Exit **10**, OR any line reading `upgraded, boundary N already recorded by the
     indexer`: the upgrade already executed on-chain (ARRIVED) and the deployed decoder is
     stale; finish this procedure urgently, state the incident urgency in the PR (ADR-23),
     and check `verify-devnet.sh` output for undecodable accounts. Drop `--graphql` if the
     local API is not running.

3. **Update `idls/<p>.json` from the NORMALIZED build output** — the file the script wrote
   to `$OUT_DIR`, never `target/idl/` from the upstream checkout (anchor stamps the
   `declare_id!()` address, which matches NO deployed program):

   ```bash
   cp ~/.cache/realxmarket-agent/idls-<sha>/<p>.json idls/<p>.json
   diff <(jq -r .address idls/<p>.json) <(jq -r '.programs["<p>"]' addresses.json)
   ```

   The diff must be empty (`addresses.json` is canonical, ADR-19).

4. **Prove the checked-in crate is pristine BEFORE regenerating** (so the regen diff is
   caused only by the IDL, never by a smuggled hand-edit) — this will now report the IDL
   you just changed as "stale regen", which is expected; every OTHER program must be pristine:

   ```bash
   scripts/agent/verify-decoder-purity.sh
   ```

5. **Regenerate IN PLACE with the PINNED carbon-cli** (0.12.0 — the version README's
   "Regenerating a decoder" command and `verify-decoder-purity.sh` both pin; ADR-12
   explains why the pin matters):

   ```bash
   npx --yes @sevenlabs-hq/carbon-cli@0.12.0 parse \
     -i ./idls/<p>.json \
     -o ./crates/<crate-dir> \
     -s anchor -c \
     --with-postgres true --with-graphql true --with-serde true
   ```

   Then re-run `scripts/agent/verify-decoder-purity.sh <p>` — it must print `pristine`:
   the checked-in crate must stay byte-identical to fresh generator output forever.
   Never hand-edit anything under `crates/*-decoder` afterwards, including formatting.

6. **Check the carbon-core pin lockstep (ADR-12).**

   ```bash
   grep carbon-core crates/<crate-dir>/Cargo.toml
   ```

   It must read `version = "0.12.0"`. If the generator moved it, **STOP**: this is now a
   whole-workspace carbon upgrade — read DECISIONS.md ADR-12 ("Upgrade path"), regenerate
   ALL FOUR decoders with the new CLI, and bump every other `carbon-*` pin (core,
   yellowstone datasource, transaction crawler, metrics) in the SAME commit. Never
   partially. Set `CARBON_CLI_VERSION` when re-running `verify-decoder-purity.sh`.

7. **Compile — and understand what the errors are FOR:**

   ```bash
   SQLX_OFFLINE=true cargo build --workspace --locked
   ```

   The `ix_name` match in `crates/indexer/src/mapping/<p>.rs` is exhaustive on the
   instruction enum **on purpose**: every new instruction variant is a compile error that
   forces you to do the mapping work. But the `closes` match ends in `_ => vec![]`, so a
   NEW CLOSING INSTRUCTION COMPILES SILENTLY and its PDA row will never close. For every
   new or changed instruction, read the upstream program source's `close =` constraints
   (in the `~/.cache/realxmarket-agent/upstream` checkout at the pinned sha) and add
   explicit close arms with `close_at(accounts, <index>, ...)` at the right account
   positions. Follow the convention documented in the module-doc table at the top of
   `crates/indexer/src/mapping/marketplace.rs` — and extend that table. Close positions
   are per-instruction facts from the SOURCE, never from the IDL, never per-account-type
   constants (`mapping/mod.rs` module docs). Whitelist only: new instructions also need a
   `whitelist_actions` parity row decision (see `mapping/whitelist.rs`).

8. **New account types or new fields → migration + wiring.** Read and follow
   `agent/skills/write-migration/SKILL.md` for the schema work (slot-guarded current-only
   state tables, ADR-2/3/6/7 pattern; additive-only per `scripts/lint-migrations.sh`).
   Then walk README.md "Adding another program" — for an upgraded existing program the
   relevant wiring is: the `tables` list in `crates/indexer/src/programs.rs`, the
   `StateTable` enum in `crates/indexer/src/db/close.rs`, row types in
   `crates/indexer/src/db/<p>.rs`, and resolvers in `crates/api/src/graphql/programs/`.
   Root `Cargo.toml` `exclude` + `docker/rust.Dockerfile` pre-cook `COPY` lines change
   ONLY for a brand-new decoder crate, and `pipeline.rs` `common_pipes` only for a
   brand-new program.

9. **Re-audit events (ADR-10).** Events stay ignored — but the ADR requires a
   per-upgrade check: for each NEW event in the IDL, verify its payload is derivable from
   instruction args + account list. If any event carries data that is not, FLAG IT IN THE
   PR (quote the event and field) instead of silently proceeding; that event needs an
   ADR-10 revisit for that program specifically.

10. **Tests.** Conventions are in the file headers — read them before writing:
    - `crates/indexer/src/mapping/tests.rs` (whitelist parity contract) and
      `mapping/sibling_tests.rs` (per-instruction close positions via `expect_close`,
      no-action-log shape, CpiEvent no-op). Add one test per new instruction asserting
      `ix_name`, and per new closing instruction asserting table + account index.
    - Decode-through policy: one real-borsh test
      (`a_real_borsh_encoded_instruction_decodes_and_maps` in `mapping/tests.rs`) builds
      discriminator + borsh bytes and runs the actual decoder → mapper seam; the note at
      the bottom of `sibling_tests.rs` explains why one suffices. If your change touches
      arg encodings, add a decode-through case for it with the new discriminator bytes
      taken from the regenerated decoder.
    - `crates/indexer/src/integration_tests.rs` fixtures are REAL devnet bytes; they must
      still decode with the regenerated crate (additive account fields can break old
      captures — recapture per that file's header if so).

11. **The full local gauntlet**, in order:

    ```bash
    cargo fmt                                    # NEVER cargo fmt --all
    cargo clippy --workspace --all-targets -- -D warnings
    SQLX_OFFLINE=true cargo build --workspace --locked
    docker start carbon-mig-test-pg 2>/dev/null || docker run -d --name carbon-mig-test-pg -e POSTGRES_PASSWORD=test -p 54329:5432 postgres:16
    export DATABASE_URL=postgres://postgres:test@localhost:54329/postgres
    sqlx migrate run --source migrations
    cargo test --workspace --locked
    (cd crates/indexer && cargo sqlx prepare -- --lib)
    (cd crates/api && cargo sqlx prepare -- --bin api)
    scripts/lint-migrations.sh                   # if you added a migration
    scripts/agent/verify-decoder-purity.sh
    scripts/agent/verify-devnet.sh               # full rebuild from public devnet RPC
    ```

    `.sqlx` regeneration MUST run from inside each crate dir against the live migrated
    Postgres (a root-level `--workspace` prepare does not work — see the comment in
    `.github/workflows/ci.yml`); commit both caches if they changed.

12. **Ship via PR — never push to main** (main auto-deploys to production Hetzner via
    `.github/workflows/deploy.yml`). Read and follow `agent/skills/verify-and-ship/SKILL.md`.

## Checklist before you finish

- [ ] `summary.json` shows only `additive`/`identical`; upstream sha pinned in the PR.
- [ ] `check-program-upgrades.py` result (COMING vs ARRIVED) stated in the PR.
- [ ] `idls/<p>.json` address == `addresses.json` address (normalized output, not `target/idl/`).
- [ ] `verify-decoder-purity.sh` prints `pristine` for every program.
- [ ] `carbon-core` in the regenerated crate is still `=0.12.0` (or the whole workspace moved in one commit).
- [ ] Every new instruction has an explicit mapping arm AND its `close =` constraints were read from source; new close arms + `marketplace.rs`-style doc-table row added.
- [ ] New events audited against ADR-10; any non-derivable payload flagged in the PR.
- [ ] If the chain already executed this upgrade (ARRIVED): that program's
      `decoder_covers_boundary` in `crates/indexer/src/programs.rs` bumped to the recorded
      upgrade slot (from `programUpgrades`) — otherwise the startup staleness warning
      fires forever after remediation. Leave it untouched on the COMING leg (the boundary
      does not exist yet; the post-upgrade PR bumps it).
- [ ] Mapping tests, close-position tests, decode-through coverage, integration fixtures all green.
- [ ] fmt / clippy / build / test / per-crate sqlx prepare / lint-migrations / verify-devnet all pass.
- [ ] No hand-edit anywhere under `crates/*-decoder`; no legacy SubQuery file touched (ADR-21).
- [ ] Handing off through `agent/skills/verify-and-ship/SKILL.md`; no direct push to main.

## Traps

- **`cargo fmt --all`** — reformats the workspace-EXCLUDED generated decoder crates and
  destroys their byte-identical-to-generator provenance; CI and you run plain `cargo fmt`
  (README.md "Regenerating a decoder").
- **Hand-editing a generated crate** — the next regen silently discards your edit and
  `verify-decoder-purity.sh` fails for everyone until then. The only sanctioned exception
  is `freeze-decoder-version.sh`'s one-line package rename (versioned-decoder skill).
- **`carbon-cli@latest`** — a newer CLI can generate against a newer `carbon-core` and
  force a whole-workspace pin bump you did not intend (ADR-12). Always `@0.12.0` (what
  README's command pins) unless you are deliberately executing that bump.
- **Stale `.sqlx` caches** — offline builds pass locally against the old cache, then CI's
  per-crate `cargo sqlx prepare --check` fails. Regenerate from INSIDE each crate dir
  against the migrated 54329 Postgres; committing the caches is part of the change.
- **The close catch-all's silence** — `_ => vec![]` means a new closing instruction
  compiles and runs while its PDA row stays open forever in a "current-only" state table.
  The compiler will NOT tell you; only reading the source's `close =` constraints will.
- **Account-position drift on same-length reorders** — if an instruction's account list is
  rearranged (or the closed account moves) the decoder still decodes and `close_at` reads
  a syntactically valid but WRONG pubkey. Undetectable at runtime; source review of every
  changed instruction's account list is the only defense.
- **`program_instructions.data` JSON shape drift** — that JSONB column is
  `serde_json::to_value` of the decoded args (`mapping/mod.rs`) in an append-only history
  table. A regen that changes field names/serde representation forks the JSON shape
  mid-history. Additive IDL diffs should not do this, but a CLI version bump can — diff
  the regenerated `types/` + `instructions/` serde output and flag any change in the PR.
- **Updating `idls/` from upstream main alone** — `idls/` must describe what the DEPLOYED
  programs speak (plus, for a prepared additive upgrade, the superset about to arrive).
  For breaking diffs that statement is only satisfiable via
  `agent/skills/versioned-decoder/SKILL.md`.
