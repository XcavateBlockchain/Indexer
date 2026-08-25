# Agentic maintenance

How this indexer is kept in lockstep with the `realxmarket-solana` programs by an always-on
AI maintenance agent, and how the pieces built for that (ADR-23/24/25) fit together. This is
the design document; the executable procedures live in `agent/skills/` (entry point:
`AGENTS.md` at the repo root), and the tooling in `scripts/agent/` (its README has the
script-by-script summary).

## 1. Objective

On every settled push to `XcavateBlockchain/realxmarket-solana` `main`, and on every on-chain
upgrade of the four deployed programs, the maintenance agent updates this indexer so that it
keeps indexing correctly — old history and new — with a human-reviewed PR as the only way
anything ships. The deployment contract with the program side:

> **The multisig executes an on-chain program upgrade only after the updated indexer is
> merged, deployed, and healthy.** The indexer is always ready for the chain before the
> chain moves.

Today everything (programs, indexer, this whole pipeline) targets **devnet only**. Mainnet
is a placeholder — see §8.

## 2. Non-negotiables

1. **PRs only.** The agent never commits to or pushes `main`: every push to `main`
   auto-deploys to the production host (`.github/workflows/deploy.yml`). Branches are named
   `agent/<yyyymmdd>-<topic>`.
2. **The chain outranks the repo.** Upstream `main` says what is *coming*;
   `scripts/agent/check-program-upgrades.py` says what is *deployed*. `idls/` must always
   *decode* what the deployed programs speak: an additive update may land ahead of the
   chain (a superset decoder still decodes everything deployed), while a breaking one
   prepares through the versioned-decoder mechanism (§5) — never an early swap that would
   orphan the deployed version.
3. **History keeps decoding.** Backfill and reconciliation deliberately re-walk old ranges
   through the same pipes as the live stream; any change that would make pre-upgrade
   transactions undecodable (or, worse, silently mis-decodable) is wrong by construction.
   Version boundaries are routed, not overwritten.
4. **Migrations are additive.** Enforced mechanically by `scripts/lint-migrations.sh` and
   the `migration-lint` CI job (ADR-25). Additive migrations are also what makes rollback
   (repointing the server to the previous image sha) safe: the old binary runs fine against
   the newer schema.
5. **All existing repo invariants hold** — never `cargo fmt --all`, never hand-edit
   generated decoder crates, never edit applied migrations, never touch the inert SubQuery
   rollback files (ADR-21). `AGENTS.md` is the canonical list.

## 3. The maintenance loop

```
                 upstream push                        on-chain upgrade
                      │                                     │
   watch-upstream.sh  │  (poll + settle debounce)           │  BPFLoaderUpgradeable Upgrade tx
                      ▼                                     ▼
             build-upstream-idls.sh              upgrade recorder (in-pipeline)
             anchor build + normalize            → program_upgrades row ('chain')
             + structural diff vs idls/          → ProgramUpgradeDetected alert
                      │                                     │
        ┌─────────────┼──────────────┐                      │
        │ identical   │ additive     │ breaking             │
        ▼             ▼              ▼                      ▼
      done      regen-decoders   versioned-decoder    post-upgrade duties
                 skill            skill               (verify-and-ship §6:
                      │              │                 confirm, re-backfill, watch)
                      └──────┬───────┘
                             ▼
                      write-migration skill (if schema is touched)
                             ▼
                      verify-and-ship skill
                      (gauntlet → PR → human review → merge → auto-deploy
                       → multisig go/no-go → multisig executes → post-upgrade)
```

The trigger is a poll (`scripts/agent/watch-upstream.sh`), not a webhook: the agent host has
no public endpoint and the upstream repo has no CI to send dispatches from. The poll
debounces — upstream lands bursts, and has added-then-dropped a feature within two days, so
only a HEAD that has sat still (default 30 min) is acted on. The deeper debounce is
structural: what matters is what the multisig *deploys*, and the pipeline never treats
upstream HEAD as authoritative (§2.2).

## 4. Upgrade detection (ADR-24)

A program upgrade is itself a transaction — the BPF upgradeable loader's `Upgrade`
instruction with the program account in its account list — so it already flows through both
existing data paths (the Yellowstone per-program filters and the `getSignaturesForAddress`
crawls). `crates/indexer/src/upgrades.rs` gives it a decoder and records every observation
into the `program_upgrades` table (migration 0011), seeded at startup with each program's
deploy slot so the table is the complete version timeline:

* first observation of a boundary → `program_upgrades_detected_total{program}` +1, a
  `warn!` log, and the **ProgramUpgradeDetected** Prometheus alert;
* re-observations (every backfill re-walk re-delivers historical upgrade txs) are `ON
  CONFLICT DO NOTHING` no-ops;
* the timeline is queryable at `programUpgrades` (GraphQL) and compared against the chain by
  `scripts/agent/check-program-upgrades.py` (which reads the ProgramData account's
  last-deploy slot directly — the belt to the recorder's braces, and the detector for
  loader-v4 migrations, devnet resets, and anything else the recorder cannot see);
* an indexer starting up against a database whose boundaries have moved past what its
  binary was built for logs a loud startup warning (`main::start`).

Detection is deliberately decoupled from reaction: with a single decoder per program, a
recorded upgrade *means* "the checked-in IDL may no longer match the deployed program", and
the reaction is the maintenance loop, not anything automatic in the indexer.

## 5. Version boundaries and slot-routed decoding (ADR-25 — designed, dormant)

Nothing routes by version today because nothing has ever needed it: all four programs are
still at their version-1 deploy (verified on-chain 2026-08-25 -- the version-1 deploys of
the ADR-26 REDEPLOY at new addresses, which absorbed upstream `main@5927362`'s breaking
changes without ever upgrading a program in place), so one decoder per program is exact.
The design below is pre-agreed so that activating it is mechanical when the first breaking
in-place upgrade is prepared. Note what ADR-26 makes explicit: a redeploy at a NEW address
is not a version boundary and this mechanism cannot express it -- the answer there is the
clean swap plus a from-empty database rebuild, not routing.

* **Where routing lives: the mapper, not the decoder.** Carbon decoders are slot-blind
  (`decode_instruction(&Instruction)`), but every `ProgramMapper::map_instruction` call has
  the slot via its metadata and `account_write_op` receives the slot explicitly — so a
  versioned mapper wrapping the old and new decoder types routes each instruction/account by
  slot without touching the generic processors.
* **The boundary is the recorded upgrade slot** from `program_upgrades` — read at startup;
  until the boundary exists, the new version is registered but dormant (boundary = +∞).
  That is what makes the indexer *forward-compatible*: it ships to production speaking V1,
  holding V2, and the multisig can take days or reject the upgrade entirely — the indexer
  stays correct in every one of those worlds.
* **Activation is restart-based, not hot.** When the upgrade lands, the recorder persists
  the boundary and alerts; the indexer keeps running V1 until it is restarted, re-reads
  the boundary, and starts routing. In between, new-format transactions either fail
  mapping loudly (the `DecodeFailures` alert) or — when the new bytes simply no longer
  match any known discriminator — fall through undecoded with no metric at all, which is
  why detection keys on the *upgrade transaction itself*, not on decode errors. The post-upgrade backfill re-walk (§6) then heals the
  in-between window — every write is idempotent and slot-guarded, so the re-walk is purely
  additive. Hot activation was considered and rejected: restart + re-walk uses only
  existing, tested machinery, and the whole devnet dataset rebuilds in about a minute.
* **The boundary slot itself is ambiguous** (intra-slot ordering is invisible at this
  granularity, same as `db::close`'s same-slot tie): the rule is slot ≥ boundary routes to
  the new version, with a decode-attempt fallback to the old version for boundary-slot
  transactions only.
* **Snapshots cannot slot-route** (`getProgramAccounts` has no per-account write slot):
  `snapshot_write_op` tries the newest decoder first and falls back down the version list —
  account discriminators make wrong-version parses overwhelmingly fail, and a genuinely
  ambiguous account surfaces as a loud undecodable, not silent garbage.
* **Old versions are frozen, not regenerated**: `scripts/agent/freeze-decoder-version.sh`
  snapshots the current crate as `crates/<p>-decoder-vN` (one scripted package-rename line —
  the single sanctioned deviation from "generated crates are never edited") and archives the
  IDL under `idls/versions/<p>/`. The current crate keeps its byte-identical regeneration
  story untouched.
* **History gets attribution when it needs it**: `program_instructions.decoder_version`
  (nullable, added in 0011) stays NULL while one decoder exists — NULL reads as "version 1"
  — and is stamped by the versioned mapper once routing activates, so mixed-shape JSONB
  history is distinguishable without a migration racing the upgrade.

The full procedure is `agent/skills/versioned-decoder/SKILL.md`.

## 6. Verification and shipping

Every change ends with `agent/skills/verify-and-ship/SKILL.md`: the local gauntlet (fmt /
clippy / locked offline build / full test suite / per-crate `sqlx prepare --check` /
migration lint), then the two ground-truth checks —

* `scripts/agent/verify-devnet.sh`: rebuild the entire dataset from nothing into a
  disposable Postgres from the **public** devnet RPC and assert it matches the chain (zero
  undecodable accounts, complete backfills, the config PDAs from `addresses.json` present,
  the version boundaries seeded). The four programs' whole footprint is tiny, so this full
  end-to-end proof costs about a minute — it is the same evidence the original migration's
  sign-off used, and it needs no Alchemy key.
* `scripts/agent/check-program-upgrades.py`: what is actually deployed, so the PR can state
  precisely whether it is *reacting* to the chain or *preparing* for it.

Then the PR (template in the skill), CI, human review, merge — which auto-deploys — and the
multisig go/no-go: deployed sha + `/health` healthy + expected `programUpgrades` boundaries
+ `DecodeFailures` at zero. After the multisig executes: confirm detection, cross-check with
`check-program-upgrades.py --graphql`, run the production backfill re-walk, and watch the
decode-failure panel.

Two humans stay in the loop by design: the PR reviewer (nothing ships without one) and the
multisig signers (nothing deploys on-chain without them). The agent's autonomy budget is
everything before those two gates.

## 7. Current state (2026-08-25, post-ADR-26 redeploy)

Measured, not assumed — by running this pipeline's own tooling:

* On-chain (devnet): all four programs are the 2026-08-25 redeploy at NEW addresses
  (ADR-26; `addresses.json` is current), each still at its version-1 deploy slot
  (`check-program-upgrades.py` exit 0). `idls/` matches the deployed programs; the indexer
  decodes every account and instruction on devnet (`verify-devnet.sh`: 4 programs, 32
  instructions, 0 undecodable, 4 seeded boundaries).
* Upstream `main@5927362` vs `idls/`: IDENTICAL for all four — the redeploy WAS that
  upstream state (secondary market, offers, income, governance). What 2026-08-22's entry
  here forecast as the first versioned-decoder activation instead arrived as a redeploy at
  new addresses, which ADR-25's slot routing cannot express and does not need to: the old
  deployments are abandoned and the database was rebuilt from empty. The next upstream
  breaking diff against these addresses is again versioned-decoder territory.
* Known upstream sharp edges the tooling already guards: no committed IDLs and no CI
  (`anchor build` is the only IDL source), no pinned Anchor CLI (the build script
  version-checks against upstream's `Cargo.lock`), `declare_id!`s that do not match the
  deployed addresses (normalization vs `addresses.json`), and one pre-existing upstream
  inconsistency — the whitelist's `declare_id` was changed (`55d99d0`) without updating its
  `Anchor.toml`, while devnet still runs the old id, which is exactly why the chain, not the
  repo, is authoritative.

## 8. Mainnet (placeholder)

Nothing is deployed on mainnet. When that changes, the intended shape is: a second
environment (own database, own `addresses.mainnet.json` — a placeholder file exists at the
repo root — own deploy target and `PROGRAMS`/cluster config), promoted only after the
corresponding devnet deployment has been verified by this pipeline end-to-end; devnet is the
permanent staging ground for every program upgrade (multisig dry-run included). The devnet
reset procedure (RUNBOOK) does not apply to mainnet; everything else in this document does.
Opening items when mainnet work starts: mainnet program IDs and deploy slots in
`addresses.mainnet.json`, an environment dimension in the deploy workflow, and a paid RPC/
gRPC plan for mainnet volume (the public-RPC verify loop stays devnet-only).

## 9. Stop-and-ask list

The agent stops and asks a human instead of proceeding when:

* the on-chain probe disagrees with both `idls/` and `program_upgrades` (unknown deployment
  provenance — someone deployed something the pipeline never saw);
* a program's account owner is no longer loader-v3, a program vanishes on-chain, or slots
  run backwards (devnet reset — RUNBOOK procedure, but the *decision* to wipe is human);
* an upstream diff removes instructions or shrinks account types in a way that would lose
  indexed data semantics (not just decoding);
* the migration needed cannot be expressed additively (the lint marker would be a lie);
* carbon-cli stops generating against carbon-core 0.12.0 (whole-stack pin bump, ADR-12);
* any event payload is no longer derivable from instructions + accounts (ADR-10's revisit
  clause) — log-parsing would be a new subsystem, not a maintenance task.
