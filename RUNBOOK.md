# Runbook

Day-2 operations for the Carbon/Rust indexer stack. For server setup, secrets, and deploy
mechanics, see [docs/deployment.md](docs/deployment.md) — this document assumes a stack is
already running and something needs investigating or fixing. For *why* things work this way,
see [ARCHITECTURE.md](ARCHITECTURE.md) and [DECISIONS.md](DECISIONS.md).

## Is the indexer behind?

Three signals, cheapest first:

1. **Grafana "Slot lag" panel** (top-left, biggest panel on the "Indexer health" dashboard) —
   `chain_tip_slot - last_contiguous_slot`. Green `<300`, yellow `<3000`, red `>=3000`. This
   is a *proven* lag (the reconciliation supervisor only advances `last_contiguous_slot`
   after actually walking and re-indexing that range — see
   [ARCHITECTURE.md §5](ARCHITECTURE.md#5-contiguity-crawler-driven-reconciliation)), not just
   "time since the last row was written."
2. **`curl <host>/health`** (via `api`, port 3010) —
   `{"last_contiguous_slot", "backfill_complete", "chain_tip_slot", "slot_lag", "healthy"}`.
   `healthy: false` / HTTP 503 means the database is unreachable, independent of chain lag
   (a chain-tip RPC hiccup alone never flips this to unhealthy — see
   `crates/api/src/health.rs`'s doc comment). `backfill_complete: false` after the stack has
   had a few minutes means the initial backfill hasn't finished yet — expected right after a
   fresh deploy or a volume wipe, not expected hours later.
3. **`syncStatus` GraphQL query** — the same fleet aggregates as `/health` (min
   `lastContiguousSlot` / AND `backfillComplete` across programs) plus a per-program
   breakdown in its `programs` field — the first place to look when the aggregate is behind:
   it names which program is dragging. If the stack runs with a narrowed `PROGRAMS` subset,
   set the api's `PROGRAMS` to the same value so the excluded programs' frozen rows don't
   drag both surfaces (see README's environment table).

### "Frozen frontier" — what it looks like and what usually unfreezes it

Symptom: `last_contiguous_slot` (and therefore the slot-lag panel) stops moving entirely,
not just falling behind.

- **Most common cause: the initial backfill hasn't completed.** The reconciliation
  supervisor refuses to advance `last_contiguous_slot` at all while
  `sync_state.backfill_complete = false` (`reconcile: skipping (backfill has not completed;
  ...)` in the logs) — this is deliberate, not a bug: advancing the frontier before the
  history walk has ever run would be claiming "no gaps below T" over a range nobody has
  looked at. Check `backfill_complete` via `/health`/`syncStatus`; if it's still `false` a
  while after startup, the backfill itself is stuck — check the indexer's logs for a
  `backfill:` line reporting an error (a poison signature the RPC can't fetch, or every
  configured RPC endpoint failing) rather than a `walk finished` line. See "Re-run a
  backfill" below.
- **Second most common: the reconciliation loop itself is failing.** Look for repeated
  `reconcile:` error lines. A transient RPC failure self-heals on the next
  `RECONCILE_INTERVAL` cycle (default 300s — so up to ~5 minutes of staleness during a
  healthy blip is normal, not a page-worthy event); a *persistent* failure (both configured
  RPC endpoints down, or a database write failure) needs investigating at the RPC/DB layer,
  not the indexer's.
- **Distinguishing a stalled backfill from a stalled post-backfill reconciler**: the
  `BackfillStalled` alert (see "Alert list" below) can fire for either case, because its
  underlying gauge (`backfill_last_processed_slot`) freezes forever once the initial backfill
  finishes and never resumes during ordinary reconciliation. Check `backfillComplete` via
  `/health`/`syncStatus` to tell them apart: `false` means the backfill itself is stuck;
  `true` means the reconciler is.

## Rebuild account state from a snapshot

```bash
docker compose exec indexer indexer snapshot
# or, bare cargo: DATABASE_URL=... ./target/debug/indexer snapshot
```

Safe to run at any time against a live, in-use `DATABASE_URL` — including while `indexer run`
is also live and writing. Why: the snapshot is tagged with a `getSlot` read taken *before*
the `getProgramAccounts` call, and every write goes through the same slot-guarded upsert the
live stream uses (`ADR-6`) — a snapshot row can only ever *lose* to a fresher stream-written
row, never overwrite one. Use this when:

- `config`/`admin`/`role_account` look empty or stale on an otherwise-healthy indexer (the
  account-state tables are only populated by the live stream reacting to a change, or by an
  explicit snapshot — a program idle for days won't self-heal this on its own, see
  [ARCHITECTURE.md §4](ARCHITECTURE.md#4-backfill-ordering-stream-first--snapshot--history-walk)).
- After manually truncating the account-state tables for any reason (`snapshot_slot` must
  also be cleared in `sync_state` first, or `indexer run` won't know to re-snapshot on its
  own next startup — see `task-4-report.md` concern #6).

`snapshot_accounts_loaded` (Prometheus gauge) and the log line `snapshot complete: N
account(s) written at slot S` confirm it ran and how many rows it touched.

## Re-run a backfill / resume after interruption

```bash
docker compose exec indexer indexer backfill
```

Safe to run repeatedly, including on a database where a previous backfill already completed
(it re-walks and re-verifies the whole range — idempotent, changes zero rows if nothing was
actually missing) and including after an interruption (kill, crash, redeploy) mid-walk.

**Cursor semantics**: `backfill_cursor` (one row per program) records the oldest signature
of that program's walk whose whole page has already committed. It is written *through the batcher*, sorted after
the rows of its own page, so it can never claim a page whose rows didn't land — and it's
deleted the moment a walk reaches its stop condition, so "a cursor exists" means exactly "an
interrupted walk is waiting to be resumed." A plain re-run with no flags automatically
resumes below that cursor rather than restarting from the tip.

**The `--floor` guard**: `indexer backfill --program <name> --floor <slot>` (`--floor`
requires `--program`; a shared floor across programs with different deploy slots would be
meaningless) stops that program's walk early instead of at its real deploy slot. It only marks `sync_state.backfill_complete = true` (which is
what allows the reconciliation supervisor to start advancing `last_contiguous_slot`) if the
supplied floor is at or below the database's own `sync_state.backfill_floor_slot` — i.e. if
the walk genuinely reached full history completeness. A higher `--floor` logs a warning and
claims nothing, but does *not* clear the resume cursor: it's left exactly where the partial
walk stopped, so a later unrestricted `indexer backfill` continues down to the real floor
instead of starting over from the tip. Don't hand-edit `sync_state.backfill_complete` to work
around this — it exists specifically to prevent the reconciler from being told "no gaps
below T" over a range that was never actually walked (see `task-4-report.md`'s "Fix round 1"
for the incident this guard closed).

## After a program upgrade

Upgrades are detected, not discovered: a BPFLoaderUpgradeable `Upgrade` of any tracked
program is recorded in-pipeline into the `program_upgrades` version timeline (ADR-24 —
the `warn!` log line `NEW program upgrade recorded`, the
`program_upgrades_detected_total{program}` counter, the **ProgramUpgradeDetected** alert,
the `programUpgrades` GraphQL query, and a startup warning whenever the running binary's
decoders predate a recorded boundary). Under the maintenance contract (ADR-23) the indexer
is normally updated and deployed **before** the multisig executes the upgrade, so this
section is either the planned post-execution tail of that flow or the incident path when
the ordering broke.

1. **Establish what happened**: `python3 scripts/agent/check-program-upgrades.py --graphql
   http://localhost:3010/graphql` — the chain's last-deploy slot per program vs
   `addresses.json` and vs the indexer's recorded boundaries. If the chain moved and no
   boundary is recorded after a reconcile interval, check `IndexerDown`/`SlotLagHigh`
   first; a re-run of `indexer backfill` re-delivers the upgrade transaction and heals the
   record.
2. **Dispatch the update work** (or verify it already shipped): the procedures live in
   `agent/skills/` — `upstream-sync/SKILL.md` classifies the change,
   `regen-decoders/SKILL.md` handles additive IDL changes, `versioned-decoder/SKILL.md`
   handles breaking ones (frozen v1 decoder + slot-routed mapper on the recorded boundary,
   ADR-25). The decoder-regeneration command itself is in README's
   ["Regenerating a decoder"](README.md#regenerating-a-decoder-after-an-idl-change);
   close semantics still come from the on-chain source's `close =` constraints, never the
   IDL.
3. **Heal the in-between window**: once the updated binaries are deployed, run a full
   backfill re-walk (`docker compose exec -T indexer indexer backfill` on the host) —
   transactions that streamed between the upgrade slot and the new decoder going live were
   dropped — loudly when the bytes still decoded but no longer mapped (`DecodeFailures`),
   silently when they no longer matched any known discriminator at all — and the re-walk
   re-inserts them (every write is idempotent `ON CONFLICT DO NOTHING` / slot-guarded).
4. **Watch the decode-failure panel** ("Decode failures" on the Grafana dashboard —
   `rate(updates_failed[5m])` + `decode_skipped_total` by `reason`) for the following hour.
   A nonzero rate means the deployed program and the decoders still diverge — every mapping
   failure is loud by design (`decode_skipped_total` + an error log naming the signature
   and path + carbon's own `updates_failed`): for a compliance registry, integrity beats
   liveness (`ADR-10`, `ARCHITECTURE.md §6`).

## Devnet ledger reset

**Symptoms**: `getSignaturesForAddress`/`getProgramAccounts` suddenly return far less history
than before, or the reconciliation supervisor's crawl reports slots that no longer resolve —
Solana devnet is periodically reset by the network operators, which orphans all prior
history.

**Recovery** (minutes, at these programs' data volume — the whole four-program history is
~31 signatures and ~30 accounts as of the ADR-26 redeploy):

```bash
docker compose down
docker volume rm indexer_pgdata      # check the exact name: docker volume ls
docker compose up -d
```

Know what the volume drop takes with it: `pgdata` also holds the inert SubQuery rollback
schema (`app` — ADR-21), so wiping it abandons that rollback path. Deliberate since the
ADR-26 redeploy (the old programs the SubQuery stack pointed at no longer exist), but worth
knowing before this command runs.

A fresh `pgdata` volume means a fresh `sync_state` too, so `indexer run`'s normal startup
path (per-program snapshot, then backfill from each program's deploy slot) does the full
rebuild automatically — no manual `indexer snapshot`/`backfill` needed unless you want to
watch it happen. If a program's on-chain address or deploy slot changed as part of the reset
recovery (e.g. it was redeployed at a new slot), update `addresses.json` AND the compiled-in
registry (`crates/indexer/src/programs.rs` — the addresses come from the regenerated decoder
crates' `PROGRAM_ID` consts, the deploy slots are literals there) and rebuild; there is no
env override for either (see README's environment table for why).

## Rolling back to SubQuery

The pre-migration stack is preserved, inert, specifically for this — see
[DECISIONS.md ADR-21](DECISIONS.md#adr-21-subquery-rollback-path-preserved):

1. **Stop the active stack**: `docker compose down` (this does *not* remove the `pgdata`
   volume — leave it alone; the old stack's data is still sitting there, see step 3).
2. **Start the old stack**: `docker compose -f docker-compose.subquery.yml up -d --build`.
3. **The `app` schema is already there.** The old SubQuery stack and the new Carbon stack
   share the same `pgdata` volume — the new stack's tables live in the `public` schema, the
   old stack's live in schema `app`, and neither touches the other. As long as the volume was
   never wiped since the migration, the old stack comes back up with its data intact, no
   restore needed.
4. **Re-enable the old CI/deploy steps.** `.github/workflows/ci.yml` and
   `.github/workflows/deploy.yml` both have the old SubQuery/`grpc-api` build steps commented
   out with a `# SubQuery rollback path — disabled, see DECISIONS.md` marker — uncomment
   them, and comment out (or remove) the new `rust`/build-and-push-indexer/api steps you
   don't want running alongside them.
5. **`grpc-api`** (the old gRPC read API) has its source still in the repo, unwired from
   everything — its own `docker/node.Dockerfile`-adjacent build step in `deploy.yml` is part
   of the same commented block as step 4.

`docker compose -f docker-compose.subquery.yml config` parses cleanly at all times (verified
in CI-adjacent testing — Task 7's exit verification) so you can sanity-check the rollback
file itself before an actual incident, without starting it.

## Alert list

Defined in [`monitoring/alerts.yml`](monitoring/alerts.yml), rules only — no Alertmanager, so
nothing pages anyone on its own; check Prometheus's `/alerts` page or Grafana's alerting view
(`ADR-20`).

| Alert | Fires when | What it means |
|---|---|---|
| `SlotLagHigh` | `chain_tip_slot - last_contiguous_slot > 3000` for 5m | The proven-contiguous freshness lag is over ~20 minutes of devnet blocks. Ground truth — the metric that actually catches a stream outage the datasource silently healed itself from (see `IndexerDown` below). Check "Is the indexer behind?" above. |
| `DecodeFailures` | `updates_failed` or `decode_skipped_total` increased in 10m | An instruction decoded but couldn't be mapped, or a processor returned an error. Per the old handlers' stance and this indexer's (`ADR-10`), only reachable if a deployed program's layout diverges from its checked-in IDL — treat as a data-integrity signal, not noise. See "After a program upgrade" above. |
| `IndexerDown` | `up{job="indexer"} == 0` for 2m | Prometheus can't scrape `indexer:9464` — the process is down, or its container isn't running. **Caveat**: carbon's Yellowstone datasource re-subscribes internally on a stream error without the process (or this scrape target) ever going down, so a short stream drop the datasource healed itself will *not* trip this. `SlotLagHigh` is what catches that case. |
| `ApiDown` | `up{job="api"} == 0` for 2m | Prometheus can't scrape `api:9465` — the GraphQL API process/container is down. |
| `ReconnectStorm` | `grpc_reconnects_total` increased by more than 5 in 15m | Repeated gRPC stream rebuilds — usually Alchemy throttling, a network problem, or an unhealthy upstream. Undercounts brief blips the datasource heals internally (same caveat as `IndexerDown`); treat as a lower bound. |
| `BackfillStalled` | `(chain_tip_slot - last_contiguous_slot > 3000) and changes(backfill_last_processed_slot[15m]) == 0` for 5m | No backfill-walk progress while the indexer is behind — most likely a stuck history walk (a poison signature, or every RPC endpoint failing). Also fires for a stalled *reconciler* long after the initial backfill finished, since the underlying gauge never resumes post-completion — check `backfillComplete` via `/health`/`syncStatus` to tell the two apart (see "Frozen frontier" above). |
| `ProgramUpgradeDetected` | `program_upgrades_detected_total` increased in 1h | A tracked program's bytecode was upgraded on-chain (`ADR-24`) — the running decoder was generated from the pre-upgrade IDL. First observations only (crawl re-walks can't re-fire it); the durable record is the `program_upgrades` table / `programUpgrades` GraphQL query. Follow "After a program upgrade" above. |

## Secrets / rotation

Mechanics unchanged by this migration — see [docs/deployment.md](docs/deployment.md) for the
full list of required GitHub secrets/variables and the `POSTGRES_PASSWORD`/`GRAFANA_PASSWORD`
rotation procedures. Nothing in this migration added a new secret (verified in
`task-8-report.md`'s secrets accounting, restated in `MIGRATION_LOG.md`'s "Migration
complete" section).
