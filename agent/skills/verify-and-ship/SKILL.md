---
name: verify-and-ship
description: The final phase of EVERY indexer change — local verification gauntlet, PR, multisig upgrade ordering (ADR-23), and post-upgrade duties. Use whenever a working-tree change to this repo is ready to leave the machine.
---

# Verify and ship

**Use when**: any change to this repo (decoder regen, mapping, migration, api, docs, config)
is complete in the working tree and needs to reach production. This skill ends every other
skill — `agent/skills/upstream-sync/SKILL.md` and `agent/skills/versioned-decoder/SKILL.md`
both hand off here; read the one that sent you if you have not.

**Do NOT use for**: exploratory work, answering questions, or anything that changes no file.
Never use it to push directly — a push to `main` auto-deploys to the production Hetzner box
(`.github/workflows/deploy.yml` lines 3–5: `on: push: branches: [main]`; lines 8–11: the
`deploy-production` concurrency group queues and never cancels, so a bad push WILL land).

## 1. Local gauntlet (run every step, in order, from repo root)

The long-lived compile-check Postgres is docker container `carbon-mig-test-pg` on port 54329
(README.md "Quickstart"). Start it if absent:

```bash
docker start carbon-mig-test-pg 2>/dev/null || \
  docker run -d --name carbon-mig-test-pg -e POSTGRES_PASSWORD=test -p 54329:5432 postgres:16
```

Then:

```bash
cargo fmt                       # NEVER `cargo fmt --all` — see Traps
cargo fmt --check               # what CI runs (ci.yml "cargo fmt --check")
cargo clippy --workspace --all-targets -- -D warnings
SQLX_OFFLINE=true cargo build --workspace --locked
DATABASE_URL=postgres://postgres:test@localhost:54329/postgres \
  cargo sqlx migrate run --source migrations
DATABASE_URL=postgres://postgres:test@localhost:54329/postgres \
  cargo test --workspace --locked
( cd crates/indexer && DATABASE_URL=postgres://postgres:test@localhost:54329/postgres \
  cargo sqlx prepare --check -- --lib )
( cd crates/api && DATABASE_URL=postgres://postgres:test@localhost:54329/postgres \
  cargo sqlx prepare --check -- --bin api )
bash scripts/lint-migrations.sh                  # additive-only policy, base origin/main
bash scripts/agent/verify-devnet.sh              # full rebuild vs public devnet, ~1 min
python3 scripts/agent/check-program-upgrades.py  # what the CHAIN says is deployed
```

Notes:
- If a `--check` sqlx step fails because you changed queries, regenerate FROM INSIDE the
  crate dir (`cd crates/indexer && cargo sqlx prepare -- --lib`; `cd crates/api &&
  cargo sqlx prepare -- --bin api`, same `DATABASE_URL`), commit `.sqlx/`, re-run the check.
  Root-level `cargo sqlx prepare --workspace` does NOT work here (virtual manifest — see
  ci.yml's comment above its two per-crate check steps).
- `verify-devnet.sh` must end with `VERIFY OK: N programs, N instructions, N snapshots,
  N deploy boundaries, N chain upgrades`. Any `VERIFY FAILED` line = the tree does not ship.
- `check-program-upgrades.py` exits 0 (chain unchanged) or 10 (upgraded / anomalous).
  Exit 10 is not necessarily a blocker — it is a FACT you must state in the PR body and
  reconcile with what your change assumes (ADR-23: the chain is what is DEPLOYED).
- If decoder crates changed, also run `bash scripts/agent/verify-decoder-purity.sh`
  (proves the crates are byte-identical to a pinned carbon-cli regen, never hand-edited).

## 2. Branch and PR

```bash
git checkout -b agent/$(date +%Y%m%d)-<topic>    # e.g. agent/20260822-regions-upgrade
git add <files>   # never `git add -A` blindly; no report/scratch files
git commit        # subject: short imperative (house style per `git log --oneline`),
                  # body: bullet list of what+why, plus a Co-Authored-By trailer naming
                  # the agent that produced the change (your harness's convention)
git push -u origin HEAD
gh pr create --title "<subject>" --body-file <body.md>
```

Never commit to `main`, never `git push origin main`, never merge your own PR without the
gauntlet green and the PR body complete.

## 3. PR body template (fill every section; delete none)

```markdown
## Upstream
- repo: XcavateBlockchain/realxmarket-solana @ <sha>
- diff class per program (build-upstream-idls.sh): <identical|additive|breaking> x4

## On-chain state right now
<paste check-program-upgrades.py output — per program: last deploy slot vs known boundary>

## Files changed
- idls/: <...>
- decoder regen (crates/*-decoder): <...>
- mapping/db (crates/indexer/src/...): <...>
- migrations/: <NNNN_*.sql or none>
- api (crates/api): <...>
- docs: <...>

## Verification
- verify-devnet.sh: <paste the final VERIFY OK line>
- cargo test: <N passed>
- lint-migrations.sh: clean
- verify-decoder-purity.sh: <clean | n/a — no decoder change>

## Rollout plan (ADR-23 ordering)
1. Merge this PR -> auto-deploy to Hetzner (deploy.yml).
2. I confirm the indexer is deployed and healthy (go/no-go checklist below).
3. ONLY THEN the multisig executes the on-chain upgrade.
4. Post-upgrade: confirm detection, restart indexer if routing is versioned, backfill
   re-walk, watch DecodeFailures 1h.
(If no on-chain upgrade is involved: steps 1–2 only.)

## Docs updated
- DECISIONS.md: <ADR-N added | not a design decision>
- RUNBOOK.md / docs/deployment.md: <rows updated | no ops-behavior change>
- MIGRATION_LOG.md: <dated section added>
```

This repo documents everything: a design decision needs an ADR in DECISIONS.md, an
ops-behavior change needs RUNBOOK.md plus docs/deployment.md rows, and MIGRATION_LOG.md
gets a dated section per shipped change. A PR with an empty "Docs updated" section is
almost always wrong.

## 4. CI, merge, and the deploy you must babysit

CI on the PR (ci.yml jobs): `migration-lint`; `rust` = fmt --check, clippy -D warnings,
offline locked build, cargo test vs live pg, per-crate `cargo sqlx prepare --check`;
`docker-build-smoke`. Green CI + green gauntlet -> merge.

Merge => deploy.yml builds SHA-tagged images and restarts the production stack at
`/opt/indexer` within minutes. Its verify step probes ONLY api `/health`, Prometheus
`/-/ready`, and Grafana (deploy.yml "Verify deployment") — **it never checks the indexer
container**. A green deploy run proves nothing about the indexer. You verify it yourself:

```bash
ssh deploy@<host> "cd /opt/indexer && docker compose ps"        # all 5 Up, indexer healthy
ssh deploy@<host> "cd /opt/indexer && docker compose logs --tail 50 indexer"
curl -s http://<host>:3010/health   # or ssh + curl localhost:3010 if 3010 is firewalled
```

`/health` fields: `healthy: true`, `backfill_complete: true`, `last_contiguous_slot` close
to `chain_tip_slot`, `slot_lag` small and shrinking. Watch the `IndexerDown` and
`SlotLagHigh` Prometheus alerts (tunnel: `ssh -L 9090:localhost:9090 deploy@<host>`, or
in-network: `docker compose exec -T api curl -s http://prometheus:9090/api/v1/alerts`).

## 5. THE ORDERING CONTRACT (ADR-23) — go/no-go for the multisig

Upstream main = what is COMING. The chain = what is DEPLOYED. The multisig may execute the
on-chain upgrade ONLY after the updated indexer is merged, deployed, and healthy — never
before. Post this checklist to the signers, all boxes checked, before they act:

- [ ] Indexer deployed at sha `<merge sha>` — `ssh deploy@<host> "grep INDEXER_IMAGE /opt/indexer/.env"` shows it
- [ ] `/health` returns `healthy: true`, `backfill_complete: true`, `slot_lag` < ~100
- [ ] `programUpgrades` GraphQL query shows the expected boundaries per program
- [ ] `DecodeFailures` alert not firing; `decode_skipped_total` flat
- [ ] GO for the on-chain upgrade — ping me the execution tx when done

If any box is unchecked: NO-GO. Fix the indexer first.

## 6. Post-upgrade duties (start the moment the multisig executes)

1. **Confirm detection** (crates/indexer/src/upgrades.rs pipe + batcher):
   - `ProgramUpgradeDetected` alert lit on Prometheus (`increase(program_upgrades_detected_total[1h]) > 0`; stays lit 1h)
   - indexer log: `docker compose logs indexer | grep "NEW program upgrade recorded"`
   - GraphQL: `programUpgrades` now shows a new row with `source: "chain"` for the program
2. **Cross-check chain vs recorded**:
   ```bash
   python3 scripts/agent/check-program-upgrades.py --graphql http://<host>:3010/graphql
   ```
   Expect per-program `boundary ... already recorded by the indexer`, exit 0.
3. **Activate slot routing (versioned-decoder changes only)** — the boundary is read at
   startup, so routing stays dormant until the indexer restarts:
   ```bash
   ssh deploy@<host> "cd /opt/indexer && docker compose restart indexer"
   ```
   Skip when the shipped change had no versioned mapper (a plain additive regen decodes
   new-format transactions without any boundary).
4. **Heal the live-window blind spot** — transactions between the upgrade slot and full
   readiness may have been missed. Trigger a production re-walk (RUNBOOK.md "Re-run a
   backfill"; the exec form is the documented one):
   ```bash
   ssh deploy@<host> "cd /opt/indexer && docker compose exec -T indexer indexer backfill"
   ```
   Safe: history writes are `ON CONFLICT ... DO NOTHING` (db/instructions.rs, db/upgrades.rs)
   and account-state writes are slot-guarded upserts that only lose to fresher rows (ADR-6)
   — the re-walk is purely additive. Optionally also `... indexer snapshot` to refresh
   account state (same guarantee, RUNBOOK.md "Rebuild account state from a snapshot").
5. **Watch for one hour**: `DecodeFailures` alert and `decode_skipped_total` /
   `updates_failed` on the Grafana "Decode failures" panel. Any increase = the deployed
   program diverged from the shipped decoder — back to `upstream-sync` (read its SKILL.md).

## 7. When something goes wrong

Rollback (docs/deployment.md §3 "Rollback"): on the server, edit `/opt/indexer/.env` to
point `INDEXER_IMAGE` / `API_IMAGE` / `PG_IMAGE` at the previous good SHA tag, then
`docker compose up -d` (images from the last 7 days are still local; older re-pull from
GHCR). This is safe ONLY because migrations are additive-only (scripts/lint-migrations.sh,
enforced by CI's `migration-lint` job): the old binary keeps running against the new schema
— new columns/tables are simply unread, nothing it depends on was dropped or rewritten.
That is the entire point of the policy; never weaken it to make a rollback "cleaner".
Alternatively: GitHub -> last good Deploy run -> "Re-run all jobs", or `git revert` + PR.

## 8. Devnet reset

Symptoms: `check-program-upgrades.py` prints `MISSING ON-CHAIN (devnet reset? wrong
cluster?)` or `BEFORE expected deploy slot ... redeployed (devnet reset?)`; RPC history
shrinks. Follow RUNBOOK.md "Devnet ledger reset" (compose down, `docker volume rm
indexer_pgdata`, compose up). The `program_upgrades` boundaries are orphaned with everything
else and re-seeded from the compiled-in deploy slots on the rebuilt DB (migration
0011_program_upgrades.sql's header documents exactly this). If addresses/deploy slots
changed, update `addresses.json` AND `crates/indexer/src/programs.rs` together — the tests
pin them to each other — then run this skill from step 1.

## Checklist before you finish

- [ ] Every gauntlet step green, `VERIFY OK` line captured
- [ ] No hand edits to any `crates/*-decoder` file, no edits to applied migrations, no
      touches to the legacy SubQuery files (repo-root package.json/project.ts/schema.graphql,
      src/, grpc-api/, docker/node.Dockerfile, docker-compose.subquery.yml — ADR-21)
- [ ] `idls/` describes what is DEPLOYED (plus, during a prepared upgrade, what is about to
      arrive via the versioned-decoder mechanism) — never upstream main alone
- [ ] PR body complete per the template, docs sections updated
- [ ] Merged only with green CI; indexer verified healthy on production afterward
- [ ] If an on-chain upgrade is in play: go/no-go posted, detection confirmed, backfill
      re-walk run, DecodeFailures watched for 1h

## Traps

- **`cargo fmt --all`**: reformats the workspace-EXCLUDED generated decoder crates,
  destroying their byte-identical-to-generator provenance (README.md ~150–153). Plain
  `cargo fmt` is what CI checks. There is no undo short of a full regen.
- **Pushing `main`**: auto-deploys to production within minutes (deploy.yml), queued, never
  cancelled. PRs only.
- **Merging with a red `verify-devnet.sh`**: it is the only end-to-end check against the
  real chain; a red run means the deployed programs and your tree disagree.
- **Declaring success from a green deploy.yml run**: its verify step checks api, Prometheus,
  Grafana — NOT the indexer container. Check the indexer yourself (step 4).
- **Multisig executing before the indexer ships**: violates ADR-23; the live window between
  upgrade and readiness becomes undecodable history. The go/no-go checklist is the gate.
- **Forgetting the post-upgrade backfill re-walk**: the blind-spot transactions stay missing
  forever; the re-walk is idempotent and cheap — always run it.
- **Trusting upstream HEAD over the chain**: upstream main is what is COMING, not what is
  DEPLOYED. `check-program-upgrades.py` is the authority on deployed state; `idls/` follows
  the chain.
- **Skipping the docs duty**: RUNBOOK.md "After a program upgrade" is the operator-facing
  mirror of this skill's post-upgrade steps — if your change alters that flow, update the
  RUNBOOK in the same PR (rule 7 in AGENTS.md), or the two drift apart.
