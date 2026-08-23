---
name: upstream-sync
description: Entry point when upstream realxmarket-solana main has moved (the watcher queued a SHA) or the ProgramUpgradeDetected alert fired — establishes what changed and what is deployed, then dispatches to the right procedure.
---

# Upstream sync — the dispatcher

Use this when `scripts/agent/watch-upstream.sh` queued a new upstream SHA (in
`$STATE_DIR/pending`), when the **ProgramUpgradeDetected** alert fired, or on a periodic
sweep. Do **not** use it to make changes — it only measures and dispatches; the linked
skills do the work. Read `AGENTS.md` first if you have not this session.

## The one idea that governs everything

Three sources of truth, in rank order, answer different questions:

1. **The chain** (`scripts/agent/check-program-upgrades.py`) — what is *deployed*. Always
   authoritative.
2. **The indexer's record** (`programUpgrades` GraphQL query / `program_upgrades` table) —
   what has been *observed*. Must converge to (1).
3. **Upstream `main`** (`scripts/agent/build-upstream-idls.sh`) — what is *coming*. Never
   authoritative: deploys happen by hand/multisig from arbitrary trees, upstream has
   added-then-dropped features within days, and `idls/` must describe (1), not (3).

## Procedure

1. **Probe the chain first.**

   ```bash
   python3 scripts/agent/check-program-upgrades.py          # exit 0 = unchanged, 10 = action
   ```

   - Any program `UPGRADED past known boundary` → the deployed code moved before we
     prepared for it (the ordering contract was broken, or detection lagged). This
     outranks everything else: jump to **Reacting to a live upgrade** below.
   - `MISSING ON-CHAIN` or a slot *before* the expected deploy slot → devnet reset or
     redeploy at a new address. **Stop and ask a human** (RUNBOOK "Devnet ledger reset" is
     the procedure, but the decision to wipe production data is not yours).
   - Owner not loader-v3 → **stop and ask a human** (the upgrade recorder cannot see
     loader-v4 upgrades; this needs design work, not a routine update).

2. **Build and classify the upstream diff.**

   ```bash
   bash scripts/agent/build-upstream-idls.sh                # exit 0/10/20, report in $OUT_DIR
   ```

   The script refuses a mismatched Anchor CLI (it prints the `avm` command to fix — fix the
   toolchain, never work around the check) and normalizes every built IDL's `address` to
   `addresses.json` before diffing, so address noise never appears. Read
   `$OUT_DIR/summary.json` and each `<program>.diff.json`.

3. **Judge the per-program classification, with two nuances.**

   - *Generator-naming drift*: an Anchor version bump can rename IDL types (e.g. `Config` →
     `marketplace::state::Config`) without any layout change. Before accepting "breaking",
     check whether the "removed"+"added" pairs are the same layout under a new name and
     whether "changed" entries differ in discriminator/fields or only in nested type
     *names*. Judge layouts, not labels. If after that the surface still differs in
     discriminators, args, account order, or field layouts — it is breaking, full stop.
   - *Deployed vs coming*: pair each program's classification with step 1's answer. A
     breaking diff for a program the chain still runs at version 1 is **preparation** work
     (relaxed timeline, forward-compatible shipping); the same diff after the chain moved
     is **incident** work (new-format data is being missed right now — `DecodeFailures`
     fires for bytes that decode but no longer map, while bytes matching no known
     discriminator are skipped with no metric at all).

4. **Dispatch per program.**

   | Diff vs deployed state | Do |
   | --- | --- |
   | `identical` | Nothing. Log the sweep result and stop. |
   | `additive` only | `agent/skills/regen-decoders/SKILL.md` (read it fully first) |
   | `breaking` | `agent/skills/versioned-decoder/SKILL.md` (read it fully first) |
   | any, plus schema impact | those skills hand off to `agent/skills/write-migration/SKILL.md` |

   Multiple programs changed → one PR per coherent change-set is fine, but marketplace and
   property often change together upstream (shared types); if their diffs are coupled,
   handle them in one branch. Everything funnels into
   `agent/skills/verify-and-ship/SKILL.md` at the end.

5. **Record the sweep** even when nothing is actionable: append a dated line to the agent's
   own log (not the repo) with upstream SHA, probe result, classification per program.
   When actionable, the PR body (verify-and-ship's template) carries all of it.

## Reacting to a live upgrade (detection fired / probe shows movement)

1. Confirm the indexer recorded it: `programUpgrades` via GraphQL, or
   `python3 scripts/agent/check-program-upgrades.py --graphql http://<host>:3010/graphql` —
   the chain's last-deploy slot must appear as a recorded boundary. If the chain moved but
   no boundary is recorded after a reconcile interval (~5 min), the production indexer is
   not observing (down? stream dead?) — check `IndexerDown`/`SlotLagHigh` first; a
   production `indexer backfill` re-walk re-delivers the upgrade transaction and heals the
   record.
2. Diff **the deployed reality**, not upstream HEAD: build IDLs from the commit that was
   actually deployed if it is known (ask the humans / deployment notes); otherwise treat
   upstream HEAD as the best hypothesis and say so explicitly in the PR.
3. Proceed to the dispatch table above with "incident" urgency: until the indexer ships,
   new-format transactions are missed — loudly (`DecodeFailures`) when they still decode
   but fail mapping, silently when they match no known discriminator. They are *not lost*
   (idempotent backfill re-walks recover everything once the new decoder ships), but the
   dataset is stale for the new surface. Say exactly that in the PR so reviewers can gauge urgency.

## Checklist before you finish

- [ ] Chain probed; result recorded; no unexplained disagreement between chain, indexer
      record, and `idls/`.
- [ ] Upstream diff built with a version-checked toolchain and classified per program.
- [ ] Every actionable program dispatched to the right skill (and those skills READ, not
      skimmed).
- [ ] Anything on the stop-and-ask list (`docs/agentic-maintenance.md` §9) actually
      stopped and asked.

## Traps

- **Trusting upstream HEAD.** The whitelist's own history proves the trap: its
  `declare_id!` changed upstream while devnet still runs the old id. The chain governs.
- **Acting on an unsettled HEAD.** The watcher debounces for a reason; a mid-burst commit
  may be reverted days later. If you bypass the watcher, apply its settle rule yourself.
- **Reading "breaking" off type names.** Anchor naming drift looks like carnage in a diff;
  layouts and discriminators are what decode. Judge those.
- **Working around the Anchor version check.** An old CLI silently emits old-format IDLs
  and every diff becomes garbage.
- **Doing work in this skill.** Dispatch only. The moment you edit a file you are in the
  wrong skill.
