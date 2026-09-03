# Decisions

Architecture decision records: ADR-1..22 for the SubQuery → Carbon migration, ADR-23+ for
the agentic-maintenance era that followed it. Each entry is short: context, decision,
consequences. Source material and raw reasoning: `MIGRATION_LOG.md` and the controller
ledger (`.superpowers/sdd/carbon-migration-spec/progress.md`, rulings R1–R20); for ADR-23+,
`docs/agentic-maintenance.md`. Referenced from `docker-compose.subquery.yml`,
`.github/workflows/ci.yml` and `.github/workflows/deploy.yml`'s rollback comments, and from
`RUNBOOK.md`.

## ADR-1: Carbon over SubQuery

**Context.** SubQuery's Solana node consumes JSON-RPC polling only; it cannot ingest a
Yellowstone/Geyser gRPC stream, and hand-writing instruction handlers in TypeScript against
a hand-authored IDL doesn't scale to indexing more than one program (three sibling IDLs —
`regions`, `marketplace`, `property` — already sit unused in `idls/`).

**Decision.** Migrate to `sevenlabs-hq/carbon`: one IDL-driven framework that generates typed
decoders for both instructions and accounts, and that natively drives a Yellowstone gRPC
datasource.

**Consequences.** Rust replaces TypeScript for the indexing path; a decoder is generated (not
hand-authored) per IDL, and adding a second program later is "drop the IDL through the same
`carbon-cli` command" rather than writing new handlers by hand. The framework itself is
version-immature (see ADR-12).

## ADR-2: Account state is current-only (disposable, snapshot-rebuildable)

**Context.** The old `Config`/`Admin`/`RoleAssignment` entities mixed on-chain fields with
derived audit fields in one soft-deleted row each. Carbon's account pipe delivers current
state on change, not history.

**Decision.** The account-state tables (`config`, `admin`, `role_account`) hold exactly the
current on-chain state of each PDA — nothing else. They are a disposable mirror: droppable
and fully rebuildable at any time from a `getProgramAccounts` snapshot (`indexer snapshot`).

**Consequences.** No migration ever has to reconcile old audit data living in these tables;
a bad row is fixed by re-snapshotting, not by a data-repair script. Audit history lives
elsewhere (ADR-3).

## ADR-3: Account-state tables hold only on-chain fields; derived values live elsewhere

**Context.** Spec §5.2 non-negotiable #3 (controller ruling R7): state and derived/audit
data must not share a row, because that's what made the old entities un-rebuildable from a
snapshot.

**Decision.** Every derived/audit field the old entities carried (`active`, `addedBy`,
`assignedAt*`, `removedAt*`, `removalKind`, …) moved into SQL views folded over the new
`whitelist_actions` append-only table, never into the state tables themselves.

**Consequences.** See ADR-13 for why the fold is a view rather than incremental mutation.
The state tables stay exactly ADR-2's disposable mirror.

## ADR-4: Typed columns for account state, JSONB for instruction payloads

**Context.** `config`/`admin`/`role_account` have a small, fixed, IDL-defined shape.
`program_instructions` stores every decoded instruction, whose payload shape varies per
instruction and isn't queried directly (the parity layer, ADR-13, is the query surface).

**Decision.** Account-state tables use typed Postgres columns (`BYTEA`, `BIGINT`, `TEXT` +
`CHECK`, …). `program_instructions.data` is `JSONB`.

**Consequences.** Account state gets compile-time-checked `sqlx::query!` reads/writes and
real constraints (borsh-index-backed `CHECK`s on `role`/`permission`). The instruction
history stays schema-flexible without a migration per instruction shape, at the cost of no
column-level typing on that one table — acceptable because nothing queries into `data`
directly; `mapping.rs` already extracted anything that needs to be typed into
`whitelist_actions`.

## ADR-5: `confirmed` commitment

**Context.** The old stack ran with `--unfinalized-blocks=true`, i.e. inherently below
`finalized`. Carbon's Yellowstone datasource takes a `CommitmentLevel`.

**Decision.** Index at `confirmed`, not `finalized`.

**Consequences.** Faster visibility (finalized commitment lags confirmed by roughly a
minute on Solana), matching the old stack's tolerance for short-lived data. Confirmed-level
rollbacks are theoretically possible on a reorg; this is a documented residual risk, the same
one the old `--unfinalized-blocks=true` setting carried. Nothing in this indexer detects or
repairs a rollback — a devnet ledger reset is handled by RUNBOOK.md's "wipe and re-backfill"
procedure, not by rollback-aware logic.

## ADR-6: Slot-guarded upserts (the stale-overwrite failure mode)

**Context.** Two independent sources write account state: the live stream and the
`getProgramAccounts` snapshot (which necessarily reads a `getSlot` in the past relative to
"now"). Without a guard, a snapshot landing after a stream update can silently overwrite
fresher data with staler data — no error, no constraint violation, just a row that quietly
goes backwards.

**Decision.** Every account-state write is
`INSERT ... ON CONFLICT (pubkey) DO UPDATE SET ... WHERE <table>.slot < EXCLUDED.slot`.
A write whose slot isn't strictly newer than what's stored is a silent no-op.

**Consequences.** The stream and the snapshot can run concurrently and overlap safely by
construction (ADR-8 depends on this). Every account write path (stream, snapshot, close)
must remember to include the guard — `crates/indexer/src/db/accounts.rs` is the one place
this is implemented, and every caller goes through it.

## ADR-7: Soft closes via `closed_at_slot`

**Context.** A closed PDA (an admin removed, a role revoked) still needs its history
queryable, and Carbon's `AccountDeletion` event carries only `{pubkey, slot,
transaction_signature}` — not enough to synthesize a full row for a pubkey never seen
created.

**Decision.** A close is a slot-guarded `UPDATE` setting `closed_at_slot` (and bumping
`slot`), never a `DELETE`. A later re-create of the same PDA at a higher slot clears
`closed_at_slot` back to `NULL`, because the normal write path always includes
`closed_at_slot = NULL` in its column list.

**Consequences.** Row history for a closed-then-reopened PDA is implicit, not versioned —
only the current state is queryable from the state tables (full audit history is
`whitelist_actions`, ADR-3). A close on an unknown pubkey is a silent no-op (nothing to
guard against); see ADR-14 for why closes are instruction-driven, not deletion-driven, in
the first place.

## ADR-8: Stream-before-snapshot ordering

**Context.** `getProgramAccounts` takes noticeable wall-clock time. Any account that changes
between the snapshot's read and the stream's subscription would be invisible to both if the
snapshot ran first: the snapshot has the pre-change value, the stream never saw the change —
a permanent hole exactly as wide as the snapshot.

**Decision.** On startup, subscribe to the live stream first, let it settle
(`STREAM_SETTLE`, 5s — not a correctness mechanism, just makes the overlap the norm instead
of a race), *then* take the snapshot. The snapshot is tagged with a `getSlot` read *before*
the `getProgramAccounts` call, so its tag can only be older than the state it describes.

**Consequences.** The slot guard (ADR-6) resolves the resulting overlap: anything the stream
already delivered at a higher slot survives the snapshot's upsert untouched. Reversing the
order would reopen the hole; this ordering is spelled out in `crates/indexer/src/main.rs`
and `crates/indexer/src/snapshot.rs` precisely because it looks like removable complexity.

## ADR-9: Carbon's built-in GraphQL (Juniper/Axum) over PostGraphile/Hasura

**Context.** The spec's default is an off-the-shelf auto-schema GraphQL engine (à la the old
stack's `subql-query`, or PostGraphile/Hasura) in front of Postgres. This migration's schema
surface is a set of folded views (not raw tables) that must keep the old `schema.graphql`
field names, plus two hand-written fields (`checkAccess`, `syncStatus`) with no table behind
them at all, plus mandatory pre-execution DoS guards (depth/complexity measurement) that must
run *before* any resolver touches the database.

**Decision.** Hand-write `crates/api`'s `QueryRoot` in Rust using `carbon_core::graphql`'s
reused primitives (`build_schema`, the `I64` scalar) over `juniper`/`juniper_axum`/`axum`,
rather than deploying PostGraphile or Hasura as a separate service.

**Consequences.** No auto-generated CRUD/filtering — every resolver and its SQL is
hand-written and hand-maintained. In exchange: full control of the schema shape (matching
the old field names exactly), a clean seam to run the guard before juniper executes
(`crates/api/src/router.rs`; `graphql_router` itself was rejected for having no such seam),
and one more Rust binary instead of one more container/service. **Revisit if**: the query
surface grows enough (generic filtering, mutations, many more tables) that hand-writing
resolvers becomes the bottleneck — at that point a PostGraphile/Hasura layer in front of the
read views becomes attractive again, since the views already present a stable relational
shape to point one at.

## ADR-10: Events are `emit!` (log-based); state derives from instructions, events ignored

**Context.** None of the four checked-in IDLs has an `event_authority` account on any
instruction, which is the tell for `emit_cpi!`; these programs use `emit!` (log-based)
events. Solana log messages are truncatable under load, unlike instruction data and account
lists. The old indexer already verified every event's payload is recoverable from its
instruction (`docs/design.md` §4).

**Decision.** Keep the old indexer's stance: index instructions, ignore events entirely.
The generated decoder's `events/` module (8 typed payload structs) is never wired into any
processor.

**Consequences.** No log-parsing code exists or is needed. If a future program's events ever
carry data not derivable from its instructions, this decision needs revisiting for that
program specifically — not a blanket change.

## ADR-11: Divergences from the old `schema.graphql`

**Context.** The old schema stored Solana **block heights** (`docs/design.md` §5); the new
one stores **slots** everywhere, because carbon and the RPC surfaces this indexer uses expose
slots natively, and the old design already flagged block-height/slot divergence by millions
on devnet as a known wart. `txSignature` remains the canonical cross-chain reference either
way.

**Decision.** Rename every `*Block`/`*AtBlock`/`blockHeight` GraphQL field to its slot
equivalent; keep every other field name, type shape, and enum spelling identical to the old
`schema.graphql`. The `Date` scalar becomes `DateTime` (juniper's `chrono` scalar) — same
field names, different scalar *type name* only.

| Old (`schema.graphql`) | New (`crates/api`'s schema) |
| --- | --- |
| `Config.updatedAtBlock` | `Config.updatedAtSlot` |
| `Admin.addedAtBlock` | `Admin.addedAtSlot` |
| `Admin.removedAtBlock` | `Admin.removedAtSlot` |
| `RoleAssignment.assignedAtBlock` | `RoleAssignment.assignedAtSlot` |
| `RoleAssignment.updatedAtBlock` | `RoleAssignment.updatedAtSlot` |
| `RoleAssignment.removedAtBlock` | `RoleAssignment.removedAtSlot` |
| `WhitelistAction.blockHeight` | `WhitelistAction.slot` |
| scalar `Date` | scalar `DateTime` (juniper `chrono`) |

Everything else — `id`, `authority`, `pendingAuthority`, `active`, `addedBy`, `user`, `role`,
`permission`, `rentPayer`, `assignedBy`, `removalKind`, `removedBy`, `type`, `subject`,
`actor`, `blockTime`, `txSignature`, `instructionIndex`, and every enum's spelling — is
name-stable.

**Consequences.** Existing GraphQL clients need to rename the `*Block(Height)` fields they
read; nothing else in a client's query shape changes. `role_account.user` was additionally
renamed to `user_pubkey` at the **database** column level only (`user` is a SQL reserved
word) — the GraphQL field stays `user`, so this has no client-visible effect.

## ADR-12: Carbon stack pinned at 0.12.0

**Context.** The recon phase verified "all carbon crates are 1.0.0 on crates.io" — true, but
the published `@sevenlabs-hq/carbon-cli` npm package (the only way to generate a decoder) was
still generating code against `carbon-core = "0.12.0"` at migration time. 0.x is
semver-incompatible across minor versions; mixing a 0.12.0-generated decoder with a
1.0.0 `PipelineBuilder` doesn't compile (two incompatible `carbon-core` copies in the
dependency graph).

**Decision.** Pin every `carbon-*` crate in the workspace (core, the Yellowstone datasource,
the transaction-crawler datasource, metrics) to exactly `0.12.0`. Re-verify every 1.0.0-era
API shape the recon had assumed, at 0.12.0, before relying on it (this surfaced real gaps —
see ADR-16, ADR-17).

**Consequences.** No `carbon-rpc-gpa-datasource` exists at 0.12.0 (ADR-17); no usable
Prometheus exporter exists at 0.12.0 (ADR-16); the Yellowstone datasource at 0.12.0 can't
express a bare `slots`/`blocks_meta` subscription (worked around in `grpc_smoke.rs`) and
re-subscribes internally on error without surfacing the gap to the pipeline (ADR-15).
**Upgrade path**: when `carbon-cli` starts generating against a newer `carbon-core`,
regenerate `crates/whitelist-decoder` and bump every other `carbon-*` pin in the same commit
— never partially upgrade.

## ADR-13: Derived state as order-insensitive fold views

**Context.** The old SubQuery handlers mutated `Config`/`Admin`/`RoleAssignment` entities
in place, in indexing order. That's fine for a strictly-increasing single-source stream, but
this indexer has two independent writers of instruction history that can arrive in either
order relative to each other: the live stream (newest first) and a backwards history walk
(also newest-to-oldest, but running concurrently and independently). In-place mutation is
order-*sensitive*: applying the same two events in the opposite order can produce a different
final row.

**Decision.** The instruction processor writes exactly two things, both order-insensitive
and idempotent: `program_instructions` (raw history) and `whitelist_actions` (append-only
parity table, one row per instruction). Every "current derived state" the old entities
exposed (`active`, `addedBy`, `removalKind`, …) is a SQL **view** that folds over
`whitelist_actions`, sorted internally by a canonical order
(`slot, block_time, tx_signature, ix_path`) — never by insertion order.

**Consequences.** It does not matter whether the row for slot 100 or slot 200 lands first;
the fold always reads them out in the same order and produces the same result. State
converges to the true value exactly once backfill completes, regardless of interleaving.
Cost: view SQL complexity (`migrations/0005_derived_views.sql`); if data volume ever grows
enough that folding on every read becomes expensive, the views would need to become
materialized views with an explicit refresh trigger — not needed at this program's volume.

## ADR-14: Instruction-driven soft closes (owner filter can't see closes)

**Context.** Carbon's Yellowstone account filter is scoped by program **owner**. The instant
an account closes, its owner changes away from the whitelist program (Solana reclaims the
lamports/rent), so the *next* update about that pubkey — the deletion notification — may
never arrive through an owner-scoped filter. Carbon's `account_deletions_tracked` mechanism
mitigates this for pubkeys the process has already seen, but a process that hasn't yet
observed a given PDA is blind to its closure via the deletion pipe alone.

**Decision.** `remove_admin`, `remove_role`, and `renounce_role` each trigger a slot-guarded
soft close of the target PDA row directly from the **instruction** processor, in the same
batch transaction as the instruction/action rows. The `account_deletions` pipe is wired up
too, as redundant belt-and-braces — same guarded write, reached by a different path.

**Consequences.** A close is guaranteed to happen even if the deletion pipe never fires for
that pubkey, because the instruction that caused the close always arrives (it's what created
the `whitelist_actions` row in the first place). Both paths converge on the identical
guarded `UPDATE`, so redundancy costs nothing — whichever arrives first wins, the second is a
no-op.

## ADR-15: Crawler-driven contiguity, `--floor`, and the flush-outcome guard

**Context.** carbon's Yellowstone datasource re-subscribes internally on a stream error
(inside its own retry loop) and swallows auth/plan rejections in a spawned task — a process
cannot observe that it briefly missed a window. On an idle program (this one, for days at a
time), "no updates received" is *also* the normal case, so silence proves nothing either way.
The stream therefore cannot be trusted to prove completeness on its own.

**Decision.** Split the two jobs explicitly: the **stream** owns freshness (a new
transaction is in the database in ~1s); a **periodic `getSignaturesForAddress` crawl**
(the reconciliation supervisor, plus the one-time `indexer backfill`) owns *completeness* —
"nothing below slot T is missing" — and is the *only* writer of
`sync_state.last_contiguous_slot`. `indexer backfill` additionally supports `--floor`, but
only claims `backfill_complete` if the walk actually reached `sync_state.backfill_floor_slot`
(a partial walk with a higher operator-supplied floor claims nothing — fix round 1 closed a
real bug here). Every commit-barrier call site (`backfill`, `reconcile`, `snapshot`, shutdown)
checks the batcher's `FlushOutcome` before writing a completion marker, so a dropped batch
(commit failure racing shutdown) can never be mistaken for a committed one.

**Consequences.** `chain_tip_slot - last_contiguous_slot` (the Grafana slot-lag panel,
`SlotLagHigh` alert) is a real, evidence-backed freshness claim, not a hope. Cost: one
`getSlot` + one `getSignaturesForAddress` page per `RECONCILE_INTERVAL` (default 300s) —
roughly 576 requests/day, under 2% of Alchemy's free-tier budget even before accounting for
the paid plan this migration otherwise uses.

## ADR-16: Custom Prometheus exporter

**Context.** `carbon-prometheus-metrics` 0.12.0 hard-binds its HTTP server to
`127.0.0.1:9100` — unreachable from another container, which every deployment topology here
requires (Prometheus scrapes `indexer`/`api` over the compose network).

**Decision.** Implement `carbon_core::metrics::Metrics` directly in `crates/indexer`
(`crates/indexer/src/metrics.rs`) on top of the `metrics` facade crate +
`metrics-exporter-prometheus`, binding `METRICS_ADDR` (default `0.0.0.0:9464`). `crates/api`
does the same independently for its own metrics, on `0.0.0.0:9465` by default so the two
binaries' exporters never collide on one host.

**Consequences.** A small bespoke metrics shim to maintain per binary, in exchange for a
metrics endpoint that's actually reachable across the container network. Counters are
pre-registered at zero on startup (including every label value) so an absent series always
means "not yet observed," never "broken."

## ADR-17: Hand-written `getProgramAccounts` snapshot loader

**Context.** No `carbon-rpc-gpa-datasource` exists at `0.12.0` (it's 1.0.0-era, per ADR-12).
Without a snapshot mechanism, the account-state tables can only be populated by the live
stream reacting to a change — impossible for a program that can sit idle for days with a
fresh (or truncated) database.

**Decision.** Write the snapshot as a plain ~40-line loop in `crates/indexer/src/snapshot.rs`
(`getSlot` → `getProgramAccounts` → the decoder → the same `account_write_op` mapping
function the live account pipe uses → the same slot-guarded upserts), not as a carbon
`Datasource`.

**Consequences.** A snapshot row and a stream-delivered row are byte-identical by
construction (same mapping function), asserted by a dedicated test. As a `Datasource` it
would have had to fabricate `AccountUpdate`s and be driven by a whole `Pipeline`, adding
complexity for no benefit over a loop that just runs to completion. Reusable if/when a
0.12.0-compatible gPA datasource ever ships, at the cost of throwing this loop away.

## ADR-18: `grpc-api` dropped; `checkAccess` preserved in GraphQL

**Context.** User decision (recon phase). The old stack's standalone gRPC service
(`grpc-api/`, port 50051) read the SubQuery-populated Postgres schema directly and exposed
`CheckAccess`/`GetConfig`/etc. Keeping a second, separately-deployed read service that has to
track this migration's new schema wasn't judged worth it, but `CheckAccess(user, role)` —
"the primary integration query," per the old design doc — was judged worth keeping
somewhere.

**Decision.** Drop `grpc-api` from the active stack (compose, CI, deploy) entirely; GraphQL
on port 3010 becomes the only query surface. Re-implement its integration query as a GraphQL
field, `checkAccess(user, role) -> AccessCheck { hasRole, compliant }`, backed by
`role_assignments_view`. `grpc-api/`'s source stays in the repo (rollback path, ADR-21) but
is wired into nothing.

**Consequences.** One fewer service to deploy, secure, and keep schema-compatible. Any
consumer of the old `CheckAccess` RPC has to switch transports (gRPC → GraphQL) but keeps
the same semantics. If gRPC itself (not just this one query) turns out to still be needed,
`grpc-api/`'s source is the starting point, not a green field.

## ADR-19: Whitelist-only scope *(superseded by ADR-22)*

**Context.** User decision (recon phase). Four fresh IDLs (`xcavate_whitelist`, `regions`,
`marketplace`, `property`) were dropped into `idls/` at the start of this migration, but the
old SubQuery indexer only ever indexed `xcavate_whitelist` — the other three were already
out of scope per the old `docs/design.md` §8.

**Decision.** Migrate `xcavate_whitelist` only. The other three IDLs stay checked in,
unindexed, for a later phase.

**Consequences.** The workspace layout (per-program decoder crate under `crates/`,
`addresses.json` as the canonical address source) deliberately leaves room to add a program
later: generate its decoder the same way (ADR-1's `carbon-cli` command), give it its own
migration files, and wire a second `.instruction()`/`.account()` pair into the pipeline. Not
attempted or scaffolded here — `idls/marketplace.json`, `idls/property.json`,
`idls/regions.json` are inert data files today.

*Superseded*: ADR-22 executed exactly this recipe for all three sibling programs.

## ADR-20: Alerts as Prometheus rules only, no Alertmanager

**Context.** The old stack shipped Prometheus + Grafana with no alert routing at all.
Notification delivery (paging, Slack, email) was never in scope for this migration.

**Decision.** `monitoring/alerts.yml` defines six Prometheus alerting rules
(`SlotLagHigh`, `DecodeFailures`, `IndexerDown`, `ApiDown`, `ReconnectStorm`,
`BackfillStalled`) with no Alertmanager deployed alongside them.

**Consequences.** Firing alerts are visible on Prometheus's own `/alerts` page and through
Grafana's Prometheus datasource, but nothing pages anyone. Adding Alertmanager (and routing
config) is a self-contained follow-up whenever notification delivery becomes a real
requirement — it slots in without changing any rule.

## ADR-21: SubQuery rollback path preserved

**Context.** Spec §13: the migration must be backable-out, not just forward-only. Deleting
the old stack outright would make a rollback a `git revert` archaeology exercise instead of a
config flip.

**Decision.** Keep the old SubQuery stack fully intact, inert by default:
- `docker-compose.subquery.yml` — the pre-migration compose file, `git mv`'d unchanged
  (byte-identical apart from a top-of-file "disabled rollback path" comment) from the old
  `docker-compose.yml`. Not run by anything; `docker compose -f docker-compose.subquery.yml
  config` still parses, verified in Task 7.
- `.github/workflows/ci.yml` / `deploy.yml` — the old SubQuery/`grpc-api` build-and-deploy
  steps are commented out (not deleted), each with a `# SubQuery rollback path — disabled,
  see DECISIONS.md` marker.
- `grpc-api/` — source stays in the repo, unwired (ADR-18).
- The Postgres `pgdata` volume is **shared** between the two stacks: the new stack's tables
  live in the `public` schema; the old SubQuery/`grpc-api` tables live in schema `app`. Both
  coexist in the same volume without conflict.

**Consequences.** Rolling back is "stop the new stack, `docker compose -f
docker-compose.subquery.yml up -d`, uncomment the old CI/deploy steps" — the old data is
already sitting in `app`, untouched, as long as the volume was never wiped. See
`RUNBOOK.md`'s "Rolling back to SubQuery" for the exact steps. Cost: `docker-compose.yml`'s
`pgdata` volume is minor clutter (an unused `app` schema) for as long as the rollback path is
kept; deleting `docker-compose.subquery.yml`, the commented CI/deploy blocks, and `grpc-api/`
together is the natural cleanup once the migration is confirmed stable in production and the
rollback path is no longer wanted.

## ADR-22: All four programs indexed (supersedes ADR-19)

**Context.** ADR-19 deferred the three sibling programs (`regions`, `marketplace`,
`property`) whose IDLs sat inert in `idls/`. Their extension recipe was pre-planned there;
this ADR records executing it and the decisions the recipe left open.

**Decision.** One process, one live Yellowstone subscription, one merged pipeline indexes all
four programs. Concretely:

- **Decoders**: one generated crate per program (`crates/{marketplace,property,regions}-decoder`),
  produced by the same pinned `carbon-cli` 0.12.0 invocation as the whitelist's (verified
  byte-identical regeneration first), never hand-edited, workspace-`exclude`d like the
  whitelist's. All four decoder+processor pairs are registered on the one pipeline; each
  decoder self-filters by its compiled-in program id.
- **Program registry** (`crates/indexer/src/programs.rs`): the compiled-in table of names,
  addresses (taken from the decoder crates' `PROGRAM_ID` consts, asserted against
  `addresses.json` by a test) and deploy slots (the per-program backfill floors:
  regions 483386626, marketplace 483386726, property 483386809 — read off
  `getSignaturesForAddress` and matching MIGRATION_LOG's recon). The `PROGRAM_ID` /
  `BACKFILL_START_SLOT` env overrides are **removed**: a decoder only recognises its
  compiled-in program, so overriding the address always produced a subscription that decoded
  nothing (the trap is now structurally impossible). A `PROGRAMS` env var narrows the indexed
  subset by registry name instead.
- **Per-program sync bookkeeping** (migration 0007): `sync_state` and `backfill_cursor` are
  re-keyed by `program_id` (the old singleton rows are adopted as the whitelist's, preserving
  production progress); `program_instructions` gains a `program_id` column (all pre-existing
  rows are the whitelist's by construction) and a program-scoped name index, because the four
  Anchor programs share instruction names. Backfill, snapshot, reconciliation and the
  in-memory sync frontier all became per-program; the reconciler reads one chain tip per tick
  for all programs, and a live-stream session end opens every program's gap (they share the
  one stream).
- **State tables** (migrations 0008–0010): one slot-guarded table per account type, 0002's
  exact pattern, named `<program>_<entity>` (the whitelist's three keep their legacy
  unprefixed names; `PropertyLetting` collapses the duplicated word to `property_letting`).
  Integer widths: u8→SMALLINT, u16→INT (u16 max exceeds SMALLINT; on-chain bps validation is
  deliberately not relied on), u32/u64/i64→BIGINT (u64 above i64::MAX would fail loudly —
  same accepted caveat as `lamports`). Fixed-shape nested structs are flattened into typed
  columns (`spv_election_*`, `developer_lawyer_*`, `election_*`); genuine lists are JSONB in
  shapes this indexer constructs (pubkeys as base58 strings, postcodes as UTF-8 strings) —
  NOT the decoder's serde output; postcode-style byte strings are BYTEA.
- **No action log for siblings.** `whitelist_actions` and its fold views exist for SubQuery
  parity only; the siblings never had a SubQuery surface to be faithful to. Their current
  state is the account tables and their history is `program_instructions` (now exposed via
  the `programInstructions` GraphQL query, decoded args as JSON).
- **Closes**: each program's account-closing instructions were read off its on-chain source's
  `close =` constraints (per-instruction account positions — the same account type closes at
  different positions in different instructions, and two marketplace instructions close two
  PDAs at once). One genuinely conditional close exists in the protocol — property's
  `remove_letting_agent` closes the `LettingAgent` PDA only when the removed location was its
  last — and since the mapper is pure, that decision is made by the batcher's SQL against the
  stored row (`db::property::close_letting_agent_if_last`), self-healing under stale state
  like every other slot-guarded write. The deletion-pipe safety net now tries every state
  table via one enum-constrained dynamic close (`db::close`), exercised table-by-table in
  `db::tests`.
- **ADR-10 revisited per program**, as it requires. All three siblings emit events with
  log-based `emit!` (never `emit_cpi!`), so the decoders' synthetic `CpiEvent` variants stay
  unreachable, exactly like the whitelist. Derivability: most sibling events are recoverable
  from instruction args plus the stored account rows, but a handful are honestly NOT
  derivable at instruction-mapping time — clock-computed deadlines (`RegionProposed.expiry`,
  `ResignationInitiated`/`resign` due timestamps), mint-supply-derived bonds
  (`RegionClaimed.collateral`), on-chain election outcomes (`SpvElectionFinalized`,
  `AgentElectionFinalized`), and payment-math amounts (`PropertySharesBought`, `DealPayout`,
  `CancelledFeesSettled`). Every such value that is *persisted* into a tracked account is
  still captured by the account stream in the same slot; the residual gap is event payloads
  that live only in logs (computed payout splits). Indexing transaction logs is the
  documented follow-up if that gap ever matters; it is a new subsystem, not a per-program
  tweak.
- **GraphQL**: the sibling entities join the same hand-written juniper surface (ADR-9 stands)
  as unprefixed connections (`listings`, `regions`, `voteRecords`, ...; only the three
  per-program configs are prefixed, `marketplaceConfig` etc.), delegated to per-program
  resolver modules so `QueryRoot` stays an index. `syncStatus` and `/health` report fleet
  aggregates (min / AND across programs — the stack is only as caught-up as its laggiest
  program) plus a per-program breakdown. Per-program metrics gain a `program` label, and the
  alert/dashboard PromQL was rewritten for the labelled series.

**Consequences.** The dataset now covers the full history and current state of the whole
realXmarket protocol on devnet, from each program's deploy slot. Existing GraphQL consumers
are unaffected (the whitelist surface is unchanged; `syncStatus` keeps its fields with
aggregate semantics and gains `programs`). A redeployed production database is adopted
in-place by migration 0007 without re-backfilling the whitelist. RPC budget scales linearly
with programs (~4x reconcile cost — still under 6% of Alchemy's free tier). The revisit
trigger in ADR-9 (PostGraphile/Hasura once the query surface grows) is now materially closer:
the hand-written surface is ~24 connections across four programs.

## ADR-23: Agentic maintenance — PR-gated, chain-authoritative, indexer-before-multisig

**Context.** The upstream programs repo (`XcavateBlockchain/realxmarket-solana`) moves fast,
commits no IDLs, and has no CI; deployments to devnet are made by hand from uncommitted
keypairs, and an on-chain upgrade multisig is planned. Keeping this indexer in lockstep was
an operator procedure (RUNBOOK "After a program upgrade"). An always-on local AI agent now
automates it, which forces the implicit rules to become explicit ones — most sharply because
a push to `main` auto-deploys production (`deploy.yml`), and because upstream `main` and the
deployed chain routinely disagree (measured on 2026-08-22: upstream HEAD is breaking for
marketplace/property while the chain still runs version 1 of everything).

**Decision.** The maintenance loop (design: `docs/agentic-maintenance.md`; procedures:
`agent/skills/`; tooling: `scripts/agent/`) is governed by three contracts. (1) *PRs only*:
the agent never pushes `main`; a human reviews every change, and CI (including the new
`migration-lint` job) gates it. (2) *The chain is authoritative*: upstream `main` says what
is coming, the on-chain probe (`check-program-upgrades.py`) says what is deployed, and
`idls/` must always decode the deployed programs: an additive update may land ahead of the
chain (a superset decoder still decodes everything deployed), while a breaking one prepares
through the versioned-decoder mechanism (ADR-25) — never an early swap that would orphan
the deployed version. (3) *Ordering*: the multisig
executes an on-chain upgrade only after the updated indexer is merged, deployed and healthy;
the go/no-go evidence is the deployed sha, `/health`, the `programUpgrades` timeline and a
zero `DecodeFailures` reading.

**Consequences.** The indexer ships forward-compatible and the multisig can delay or reject
an upgrade indefinitely without ever de-syncing it. The agent's autonomy ends at two human
gates (PR review, multisig signatures). Verification is chain-grounded and cheap — a full
devnet rebuild into a disposable database (`verify-devnet.sh`, ~1 minute, public RPC) is
required evidence on every PR. The trigger is a settled-HEAD poll, not a webhook. Everything
is devnet-only today; mainnet is an explicit placeholder (`addresses.mainnet.json`,
`docs/agentic-maintenance.md` §8).

## ADR-24: On-chain upgrade detection: the `program_upgrades` version timeline

**Context.** Phase 0 established that none of the four programs had ever been upgraded, so
version handling was consciously deferred ("No versioned decoders needed"). The agentic
loop needs the opposite stance: an upgrade must be *detected* the moment it lands, with a
durable record of every version boundary — and the detection must survive indexer downtime.
The key observation: an upgrade is itself a transaction (the BPF upgradeable loader's
`Upgrade` instruction referencing the program account), so both existing data paths — the
per-program Yellowstone filters and the `getSignaturesForAddress` crawls — already deliver
it; it just decoded to nothing.

**Decision.** A hand-written loader decoder + recorder pipe
(`crates/indexer/src/upgrades.rs` — the one non-generated decoder in the workspace; the
native loader has no IDL and the interesting surface is one instruction) is registered on
`common_pipes`, so it rides the live stream and every crawl. It writes `program_upgrades`
(migration 0011): one row per (program, boundary slot), seeded at startup with each
program's deploy slot (`source='deploy'`), appended by observation (`source='chain'`),
idempotent under re-walks. Detection side effects fire at most once per boundary, after
commit: `program_upgrades_detected_total{program}`, a `warn!`, and the
ProgramUpgradeDetected alert. The timeline is served by the `programUpgrades` GraphQL query;
`scripts/agent/check-program-upgrades.py` probes ProgramData last-deploy slots directly as
the out-of-band cross-check (and catches what the recorder cannot: loader-v4 migrations,
devnet resets, deployments the indexer never saw). `main::start` warns loudly when the
database knows boundaries the running binary's decoders were not built for.
`program_instructions` gains a nullable `decoder_version` column now (NULL = "version 1"),
so activating version attribution later is a code change, not a migration racing an
upgrade.

**Consequences.** An upgrade during downtime is recovered by the next crawl over that range
(a full backfill re-walk rebuilds the whole timeline from nothing — verified end-to-end on
devnet). Detection is decoupled from reaction: the indexer never swaps decoders by itself;
a recorded boundary means "the checked-in IDL may no longer match the deployed program",
and the reaction is the maintenance loop. The recorder records `Upgrade` only — initial
deploys are the seeded rows, and loader-v4 (should devnet ever migrate) is the probe
script's job to flag.

## ADR-25: Slot-routed versioned decoding (designed, dormant) and the additive-only migration policy

**Context.** A breaking program upgrade must not make pre-upgrade history undecodable:
backfill and reconciliation deliberately re-walk old ranges through the same pipes as the
live stream (ADR-15), and a swapped decoder would turn the completeness machinery into a
failure generator — or, when a discriminator survives with a changed layout, into a silent
mis-decoder. Carbon decoders are slot-blind (`decode_instruction` sees only bytes), so the
routing point has to live where the slot is: the mapper. Nothing needs routing *today* (the
chain still runs version 1 of everything), but upstream already carries breaking changes,
so the design is fixed now while there is no deadline.

**Decision.** When the first breaking upgrade is prepared: freeze the current generated
crate as `crates/<p>-decoder-vN` (`freeze-decoder-version.sh`; one scripted package-rename
line is the single sanctioned deviation from "generated crates are never edited") and
archive its IDL under `idls/versions/<p>/`; regenerate `crates/<p>-decoder` from the new
IDL; wrap both in a versioned mapper that routes on the recorded `program_upgrades`
boundary, read at startup — dormant (boundary = +∞) until the upgrade actually lands, which
is what makes the indexer forward-compatible while the multisig deliberates. Activation is
restart-based, healed by an idempotent backfill re-walk; the boundary slot itself routes to
the new version with a decode-attempt fallback for that slot only; snapshots
(slot-unattributable by nature) try newest-first down the version list; the versioned
mapper stamps `program_instructions.decoder_version`. Supporting policy, enforced
mechanically (`scripts/lint-migrations.sh` + the `migration-lint` CI job): migrations are
additive-only — applied files immutable, numbers strictly increasing, no destructive SQL
without an in-file `-- lint: allow <KEYWORD> -- <why>` marker carrying a written
correctness argument (the 0007 precedent).

**Consequences.** Old bytes decode under the decoder that was true when they were written,
forever; state tables stay ADR-2 disposable mirrors (version metadata lives in
`program_upgrades` and `decoder_version`, never in state rows); rollback to a previous
image stays safe because the schema only ever gains. The cost is carried complexity per
version split (a frozen crate, a wrapper mapper, wiring) — accepted, because it is paid
only when a breaking upgrade actually ships and is removable once a version's history is
no longer served. Procedure: `agent/skills/versioned-decoder/SKILL.md`.

## ADR-26: A redeploy at new addresses is a clean swap plus a from-empty rebuild, not a version boundary

**Context.** On 2026-08-25 the protocol team redeployed all four programs to brand-new
devnet addresses (the deploys at slots 487427394..487427732) instead of upgrading the old
deployments in place, and declared the old programs abandoned. The new bytecode is upstream
`main@5927362` — the very state ADR-25 anticipated as the first BREAKING in-place upgrade
for marketplace (secondary share market: `ShareListing`/`Offer`, per-reason share locks)
and property (holder governance: `Proposal`/`Challenge`/`GovVote`; rental income:
`PropertyIncome`/`IncomeCheckpoint`). ADR-25's slot routing keys on boundaries *within one
program id* and cannot express "same logical program, different address"; every row the
indexer had ever written — accounts, instruction history, sync state, seeded boundaries —
described on-chain objects that no longer exist.

**Decision.** Treat the redeploy exactly like a devnet ledger reset with new addresses
(the case RUNBOOK "Devnet ledger reset" already anticipated): swap `addresses.json`, the
registry (`crates/indexer/src/programs.rs`), the `idls/`, and the four regenerated decoder
crates wholesale in one change; extend the schema for the new program surface in migration
0012 (seven new state tables; column surgery on the four reshaped ones under lint-allow
markers, correct because production is rebuilt from an empty database via the documented
volume drop); no frozen decoder crates, no `idls/versions/` archive, no slot routing — the
old deployments' history is abandoned along with the ADR-21 SubQuery rollback data in the
shared `pgdata` volume (its "natural cleanup" clause, invoked deliberately). The old
addresses survive only in immutable migrations 0007..0010 headers and historical logs.

**Consequences.** One decoder per program stays exact, the version timeline re-seeds at the
new version-1 deploy slots, and `verify-devnet.sh` proves the whole devnet dataset rebuilds
from the public RPC (4 programs, 32 instructions, 0 undecodable). The cost is history: the
old deployments' rows are gone rather than routed, acceptable because devnet data was
disposable by construction (ADR-2) and the owner declared the old programs dead. The
dormant ADR-25 machinery is untouched and still the answer for the next breaking IN-PLACE
upgrade of these addresses; a future redeploy-at-new-addresses repeats this ADR instead.
Two things this swap surfaced for later: the auto-approved `propose` path and the
emptied-`ShareListing` sales are same-instruction create+close / conditional runtime closes
(handled: no close op needed, and batcher-side conditional closes respectively), and three
new event payloads are not exactly reconstructible after the fact (`IncomeClaimed.amount`,
`ChallengeFinalized.slashed`, `IncomeDistributed.per_share_gain` — dust carry) — ADR-10
stays in force, with that revisit noted for the property program if consumers ever need
those exact figures.

## ADR-27: Off-chain property metadata is fetched by a background enricher into a derived table

**Context.** The 2026-08-25 redeploy replaced the `PropertyAsset`'s Core-asset NFT deed with
on-account `name` + `metadata_uri` (migration 0012): the URI points at an off-chain JSON
document hosted by the protocol team (property description, address, attributes, finances,
document and image links). The indexer stored only the URI string — none of the decomposed
content. That content is off-chain and mutable: unlike every other table in this repo it
cannot be rebuilt from a `getProgramAccounts` snapshot, cannot be slot-guarded, and cannot
be soft-closed — the on-chain account names the URI; the document is the operator's
off-chain claim. Fetching it inline in the account pipeline was rejected: a network call on
the write path couples ingestion of all four programs to the availability of an object
storage bucket, and the batcher's forever-retry on a deterministic failure (the write-
migration skill's stall trap) would stall everything on one dead URI.

**Decision.** A new DERIVED table `marketplace_property_metadata` (migration 0013) holds
one row per `PropertyAsset` PDA: the URI last attempted, the last successfully fetched
document verbatim (`raw` JSONB — the ground truth) decomposed into typed columns (the
nested `address` / `attributes` / `finances` objects flattened per 0010's convention — the
`address` fields under an `address_` prefix, the `attributes` / `finances` fields flat;
`propertyImages` / `otherDocuments` as JSONB lists of URL strings in the shape the indexer
constructs; the base58 wallet addresses `user_pubkey` / `companyWalletAddress` as BYTEA per
the pubkey convention), plus fetch state: `fetched_at` (snapshot time), `attempts`
(consecutive failures for the current URI), `next_attempt_at` (backoff deadline) and
`last_error`. A background task (`crates/indexer/src/metadata.rs`, spawned by `run`
next to the reconciliation supervisor; `METADATA_FETCH_INTERVAL`, default 30 s) each
cycle selects the work set — open assets with a non-empty `metadata_uri` that have no row, whose row points
at a different URI, or whose last attempt failed past its backoff — fetches over HTTP(S)
(reqwest + rustls; http/https schemes only, non-global IP-literal hosts and `localhost`
rejected as an SSRF guard, 2 MiB body cap) and upserts through `db::property_metadata`.
Parsing is LENIENT PER FIELD: a document that is not a JSON object fails the fetch, but a
mis-typed or missing field stores NULL in its column while the rest of the document is
still indexed and the whole document remains in `raw` — no information is ever lost and a
bad field can never wedge the loop. Failures record backoff on the row (30 s, doubling,
1 h cap) and never pass through the batcher, so a dead URI degrades to a lagging,
retried-and-logged column, never to a stalled pipeline. The same cycle is exposed as the
one-shot `indexer fetch-metadata` subcommand (run by hand against a production
`DATABASE_URL`, like `snapshot` / `backfill`). The API serves the table as a
`propertyMetadata` connection (filter: `assetId`); a fetch is a point-in-time SNAPSHOT —
content served under an unchanged URI is re-fetched only when the on-chain URI changes or
a fetch fails, and `fetched_at` is the provenance.

**Consequences.** The table is the repo's first non-mirror table: it is deliberately
excluded from `StateTable` / `ProgramSpec.tables` and from all close/sweep machinery
(there is nothing on-chain to close it), and ADR-2's drop-and-rebuild contract does not
apply to it — the rest of the schema's invariants are untouched. Off-chain claims are
indexed with provenance (URI + `raw` + `fetched_at`) but carry no on-chain guarantee; the
indexer validates shape, not truth. The cost is one new external dependency
(reqwest, rustls-only TLS — matching sqlx's `runtime-tokio-rustls`) and a new network
failure surface, quarantined to one loop with per-row backoff and two metrics
(`property_metadata_fetched_total{result}`, `property_metadata_pending`); a devnet reset
wipes the table with the rest of the volume. DNS-name SSRF (a hostname resolving to a
private IP) is a documented, accepted limitation on devnet, where the URIs come from the
protocol team's own deployments. Mainnet promotion (agentic-maintenance §8) inherits the
design unchanged.

## ADR-28: Property-asset registration fires an outbound webhook through a durable, retrying queue

**Context.** A consumer of the index wants to know the moment a new property asset is
registered on-chain — when the marketplace `init_property_assets` instruction creates the
`PropertyAsset` PDA (seeded `["property", listing_id]`): that is the first point at which
the asset's `name`, `metadata_uri`, and share mint all exist together (the earlier
`list_property` only opens the listing, before the asset is named). The notification
target is an operator HTTP endpoint configured per deployment. The obvious design — a
POST inside the batched commit — was rejected for the same reason ADR-27 rejected inline
fetching: a network call on the write path would couple ingestion of all four programs to
the availability of a third party, and the batcher's forever-retry on a deterministic
failure would stall the entire pipeline on one dead endpoint. Nor can delivery be
fire-and-forget from the live stream: backfill and reconciliation deliberately re-walk
old ranges (ADR-15), re-emitting the same instruction, and a process restart loses any
in-memory state — so the record must be durable and idempotent, and delivery a separate,
restartable loop.

**Decision.** Two tiers, mirroring the ADR-24 durable-record and ADR-27 retrying-loop
split. *Detection* (in the pipeline): the marketplace mapper emits exactly one
`WebhookEvent` for `init_property_assets` (and no other instruction) — `event_type`
`property_asset_registered`, `event_id`
`property_asset_registered:<base58 PropertyAsset PDA>` (account index 3; the PDA is the
dedup key — one asset registers exactly once), and a JSON payload (`event`, `pubkey`,
`listing_id`, `name`, `metadata_uri`, `share_mint` (account index 4), `slot`,
`tx_signature`, `block_time`, `program`). The batcher records it as a `WriteOp` committed
atomically with the rest of the transaction's rows into the durable table
`webhook_events` (migration 0014) via `INSERT ... ON CONFLICT (event_id) DO NOTHING` —
idempotent under backfill re-walks, so each asset is recorded at most once, ever. Like
`marketplace_property_metadata`, the table is NOT an account-state mirror: no slot guard,
no soft close, outside the `StateTable` roster. *Delivery* (out of the pipeline): a
background supervisor (`crates/indexer/src/webhooks.rs`, spawned by `run` next to the
metadata fetcher; `WEBHOOK_INTERVAL`, default 5 s; spawned only when `WEBHOOK_URL` is set
**and** `marketplace` ∈ `PROGRAMS`) each cycle drains the work set — undelivered rows
whose backoff has elapsed, ≤50 per cycle, ordered by `event_id` — POSTing each `payload`
verbatim to `WEBHOOK_URL` (reusing the metadata fetcher's SSRF-guarded reqwest client);
a 2xx marks the row delivered, a failure records `last_error`/`attempts` and schedules
the next attempt with exponential backoff in SQL (30 s, doubling, 1 h cap — the ADR-27
shape). A dead endpoint degrades to lagging, retried-and-logged rows, never to a stalled
pipeline or a lost notification. With no `WEBHOOK_URL` the events are still recorded but
the loop is never spawned and no external call is ever made. Metrics:
`webhooks_delivered_total{result=success|failure}` (both labels pre-registered at zero)
and the `webhooks_pending` gauge (work-set size after the last cycle; the series is
absent while the loop is off); the alert is `WebhookDeliveryFailing` (alerts.yml).

**Consequences.** The contract splits cleanly: *at-most-once recording* (the PDA-keyed
`ON CONFLICT` guarantees an asset is enqueued at most once, even across backfill
re-walks) and *at-least-once delivery* (a POST that succeeded on the server but errored
on the wire is re-POSTed — endpoints dedupe on `pubkey`). A fresh index with the webhook
enabled (first deploy, devnet volume wipe) records the whole historical
`init_property_assets` set during the backfill re-walk and then flushes it at ≤50 events
per 5 s cycle in `event_id` order (base58 PDA order, not slot order) — a one-time backlog
an operator should expect; steady state is one event per new asset, seconds after the
confirming slot. A devnet reset wipes `webhook_events` with the rest of the volume
(ADR-2's drop-and-rebuild contract applies; the chain re-records the table). The cost is
a second outbound network surface beyond ADR-27's (same client, same guard) and one more
table outside the close machinery; mainnet promotion (agentic-maintenance §8) inherits
the design unchanged.

## ADR-29: Fetched property metadata is nested under `propertyAssets` (one query), with the root `propertyMetadata` connection kept

**Context.** ADR-27 indexed the operator-hosted metadata document into the derived table
`marketplace_property_metadata` and surfaced it to the API only as a standalone
`propertyMetadata` connection (filterable by `assetId`). The practical shape of that
surface is two queries: a consumer that wants "property assets, each with its document"
must run `propertyAssets` and `propertyMetadata` separately and join client-side on the
asset PDA / `assetId` — ADR-27's finding "consumers join on `pubkey`" is this, stated up
front. The index's front-end consumers want asset + document answered in a single
GraphQL query.

**Decision.** Reuse the existing `PropertyMetadata` type NESTED under `PropertyAsset` as
`metadata` (nullable — `null` while the enricher has no row for the asset's PDA, i.e. the
fetch is pending or still failing). Implementation: the `property_assets` resolver keeps
its existing asset-page query byte-identical, then issues ONE extra keyed lookup —
`SELECT <the same 50 columns> FROM marketplace_property_metadata WHERE pubkey = ANY($1)`
over the page's PDA pubkeys (the derived table is 1:1 on the asset PDA's PK, so at most
one row per node; skipped on an empty page) — and attaches each row to its asset. The
three deliberate choices: (1) a keyed `ANY(...)` lookup, NOT a `LEFT JOIN` inside the
asset query — the API layer is uniformly single-table `query!` resolvers (no `JOIN`
anywhere in `crates/api`), the derived table is outside the mirror contract by design
(ADR-27), and keeping the asset query unchanged preserves its sqlx provenance (the cached
query is untouched; exactly one new query enters `.sqlx`); (2) the root `propertyMetadata`
connection is kept unchanged — it is still the document-first access (and the RUNBOOK's
"missing/stale metadata" flows query it), while the nested field is the asset-first
access; (3) the 50-column row→type decomposition lives in ONE module-local
`macro_rules!` (`property_metadata_from_row!`) used by both metadata queries: the
`query!` macro compile-checks each query's SQL (a renamed or mistyped column fails
`cargo sqlx prepare --check` per site) but binds each query SITE to its own private
`Record` row type that implements nothing shareable (no `sqlx::Row`, no `FromRow`), so a
plain shared function cannot name the row — the macro expands the decomposition
textually against either row type, keeping each site's full per-field compile-time
binding. No migration, no indexer change, no fetcher change — pure API surface.

**Consequences.** `propertyAssets { nodes { ... metadata { ... } } }` now answers asset +
document in one query; `metadata` is `null` until the first successful fetch (the on-chain
`metadataUri` remains readable throughout, so a pending fetch is visible, not an error).
Cost: one extra PK-keyed lookup per `propertyAssets` page (≤100 keys — `clamp_first`
cap; the table is small and PK-indexed), and the fetch runs even when the client does not
select `metadata` (juniper resolvers here are not selection-aware; at this data volume the
wasted lookup is cheaper than the machinery). The schema change is additive (new nullable
field on an existing type; every existing query still parses and answers); the DoS guards
are unaffected (nesting adds one field level under an existing field — well inside
`MAX_DEPTH = 8`, and complexity counts document fields, not result rows). The redundant-
looking `metadata { id assetId metadataUri }` inside the nesting is deliberate: one
canonical `PropertyMetadata` type serves both placements, no shadow type. Mainnet
promotion (agentic-maintenance §8) inherits the surface unchanged.

## ADR-30: realxhub is registered ahead of its devnet deploy (`deploy_slot: 0` sentinel)

**Context.** The fifth program to index — realxhub (`Hjd9AHDefWgTnwCGLCoTPRqttpXHWCAUE3zv9aeNJnXu`,
fractional hub shares on a secondary market) — has a stable IDL but is not yet deployed to
devnet: no executable account, hence no deploy slot to pin. Chain-over-repo (house rule 6)
governs the *version* of the IDL vs. the chain; the loop otherwise assumes every program
already exists on-chain — `check-program-upgrades.py` reads `getProgramData` by address and
`verify-devnet.sh` asserts the config PDA and instruction history per program. Onboarding a
program before the chain is the first time the loop meets that case.

**Decision.** Register the full surface now instead of deferring the pipeline until deploy.
The pieces are the standard ones — `idls/realxhub.json` + carbon-generated frozen
`crates/realxhub-decoder` (purity-checked), migration 0015 with five state tables (`realxhub_config`
singleton, `realxhub_hub`, `realxhub_holding`, `realxhub_share_listing`, `realxhub_faucet_receipt`;
close rules: `delist_shares` closes the listing PDA unconditionally and `buy_shares` closes it when
the listing empties), the mapping + registry entry + `close.rs` wiring, and the GraphQL surface
(`realxhub_config`, `realxhub_hubs`, `realxhub_holdings`, `realxhub_share_listings`,
`realxhub_faucet_receipts`) — and the "registered but not deployed" state is one sentinel:
**`deploy_slot: 0`** in both `addresses.json` and `crates/indexer/src/programs.rs`. Everything
downstream keys off it: `check-program-upgrades.py` reports `NOT YET DEPLOYED` (owner absent and
slot 0) and later `FIRST DEPLOY DETECTED at slot N`; `verify-devnet.sh` prints a NOTE and skips the
config-PDA/instruction assertions for realxhub; and the registry-driven seeding (`init_sync_state`
+ `upgrades::seed_deploy_slot`, both idempotent) gives realxhub a floor of 0, so the slot-0 backfill
is a superset and seeding before the chain is harmless. Deliberately not stored: the IDL's six
events (house pattern — instructions + state tables only; events duplicate those writes), the
`bump` field (stored in the tables, not exposed in GraphQL), and `u128` (income per share) is
TEXT/`String`, matching the existing house treatment of `u128`.

**Consequences.** The post-deploy procedure is: when the realxhub executable appears on devnet,
pin the real deploy slot in `addresses.json` (`deploy_slots.realxhub`) AND the realxhub entry in
`crates/indexer/src/programs.rs`, then re-run `scripts/agent/verify-devnet.sh` — the config-PDA and
instruction assertions engage for realxhub, and the registry↔addresses.json pinned test now
constrains the slot. Until then: realxhub backfill is `HistoryExhausted` on an empty range (nothing
to index), all gauntlet invariants hold 5-of-5, and the first deploy surfaces as `FIRST DEPLOY
DETECTED` from `check-program-upgrades.py`. A breaking IDL change before the first deploy is a
plain additive regeneration (nothing on-chain to protect); after it, the versioned-decoder
mechanism (ADR-25) applies unchanged. Mainnet promotion (agentic-maintenance §8) inherits the
sentinel pattern if a mainnet program ever lands ahead of its own deploy.

**Addendum (2026-09-03): the deploy landed, and it outranked the IDL.** The first
devnet deploy did at slot **489635042** — exactly what the watcher's
`check-program-upgrades.py` reported (exit 10, `FIRST DEPLOY DETECTED`) and what
`addresses.json` was pinned to (the registry↔addresses.json pinned test now
constrains the slot). But the deployed program was not what the pre-deploy IDL had
described: the on-chain `Holding` and `ShareListing` state carry a leading
`hub_id: u64` (`Holding` also carries `owner: Pubkey` as its second word, shifting
`amount`/`listed`/`per_share` by two words), and the `list_shares`/`record_sale`
instruction accounts differ (a `hub` PDA removed / a `config` PDA added
respectively). A client symptom reported the shift directly:
`realxhubShareListings` returned `seller: 4uQeVj5t…` — the leading `hub_id` word
decoded into the `seller` field. Hard rule 6 (the chain outranks the repo) applies,
and the correction was made *in place* in `idls/realxhub.json` rather than through
the ADR-25 versioned-decoder mechanism: ADR-25 exists to protect on-chain data
written under an old layout that a new decoder can no longer read, and there is
none — the program had no deploy before 489635042, so no pre-deploy state exists.
The decoder was regenerated from the corrected IDL (the same command, still
byte-identical), the mapper/db/API now carry `hub_id`/`owner`, `migrations/0017`
added the columns, and the one-time heal is a re-snapshot (`indexer snapshot`):
the slot-guarded upsert accepts the corrected decode over the rows the old decoder
had poisoned (the snapshot's tag slot postdates them — the RUNBOOK's rebuild
section carries the duty). The ADR-25 mechanism remains the mandatory path for any
*future* breaking realxhub change, at which point real data written under the
2026-09-03 layout will exist.

## ADR-31: Property images are mirrored to public object storage as 720x720 thumbnails

**Context.** ADR-27's `marketplace_property_metadata` stores the protocol team's off-chain
metadata document, and each open asset's document carries a `propertyImages` array of image
URIs. Those originals are full-resolution marketing photography (often several MiB each),
served from the protocol team's storage — external, and outside this repo's control for
liveness, rate limits, and delivery performance. Any client that renders a card or a list
downloads multi-MiB originals for what it displays as a thumbnail, and the API's own
`property_images` field only re-publishes those third-party URIs. A transaction's on-chain
meaning does not depend on the images ever downloading, so the fix is the ADR-27 shape
again: a bounded, per-item-backed-off background loop outside the account pipeline — here
mirroring each image into the operator's own object storage so the API can serve small,
stable, public thumbnails.

**Decision.** The indexer downloads each source image, re-encodes it as a 720x720
center-cropped JPEG (quality 85), and uploads it to the operator's public object storage
(Hetzner Object Storage in production; the S3 endpoint, region, and credentials come from
the `OBJECT_STORAGE_*` environment variables). The pieces:

- **Storage — migration 0016.** `marketplace_property_image` is a DERIVED table (same
  exclusion as ADR-27's metadata table and ADR-28's `webhook_events`: no slot guard, no
  soft close, no `db::close::StateTable` roster entry). Primary key `(asset_pubkey,
  image_index)`; `image_index` is the ZERO-BASED position in the `propertyImages` array —
  the document carries no per-image ids. `source_uri` is the URI the mirror LAST ATTEMPTED;
  `thumb_uri` / `uploaded_at` are the last SUCCESSFUL upload (NULL until one exists — a
  failure never erases the last published thumbnail); `attempts` / `next_attempt_at` /
  `last_error` are the per-image backoff chain. The object key is deterministic —
  `properties/<base58 pubkey>/<index>/<sha256(source_uri)>.jpg` — so a changed URI uploads
  a NEW object under a NEW key, which doubles as the client-side cache-buster (the new
  `thumb_uri` changes the URL).
- **The mirror — `crates/indexer/src/images.rs`.** The supervisor is spawned from
  `indexer run` only when object storage is configured AND the marketplace program is in
  the registry, and drains the work set (never-attempted, failed-and-backoff-expired, or
  source-URI-changed rows; bounded to 50 per cycle) every `IMAGE_MIRROR_INTERVAL` (default
  30 s). For each image, in order: SSRF-guard the URI with the existing
  `metadata::uri_allowed`, download it with a bounded streaming read (5 s connect / 15 s
  request timeouts, 10 MiB cap), decode it, center-crop it to a square, resize it to
  720x720, JPEG-encode at quality 85, and `PUT` the fully-encoded body in one call. A
  non-image or undecodable body (an HTML error page served with a 200) is a FAILURE — the
  mirror backs off rather than uploading garbage.
- **Backoff — per image, independent, computed in the SQL upsert.** A failure bumps
  `attempts` and schedules the next attempt at 30 s doubling per consecutive failure of
  the SAME URI, capped at 1 h (the ADR-27 shape); a changed URI restarts the chain. A dead
  URI (404, host outage) never blocks its siblings, and a success resets the chain and
  records the public thumbnail URL.
- **By hand — `indexer mirror-images`.** A one-shot cycle (like `fetch-metadata`),
  runnable against a production `DATABASE_URL` without redeploying; idempotent, and it
  refuses to start when `OBJECT_STORAGE_*` is unset since its job is to upload, not just
  fetch.
- **Configuration — all-or-nothing.** Any one of the five `OBJECT_STORAGE_*` variables
  (`ENDPOINT`, `BUCKET`, `REGION`, `ACCESS_KEY`, `SECRET_KEY`) set means all five must be
  present and non-empty; a half-configured mirror is a hard startup error, not a degraded
  mode. None set = the mirror is DISABLED — a first-class state: the supervisor is never
  spawned, no external call is ever made, and the API keeps serving
  `propertyImageThumbnails: null`. The keys are never logged (hand-written `Debug`). An
  optional `OBJECT_STORAGE_PUBLIC_BASE_URL` overrides the public thumbnail-URL prefix,
  which defaults to `{scheme}://{host}/{bucket}` derived from the endpoint (the shape
  every Hetzner Object Storage bucket serves public objects from).
- **API — `PropertyMetadata.propertyImageThumbnails`.** `Option<Vec<String>>` in
  `image_index` order, `null` until the mirror's first upload succeeds; the schema change
  is additive (existing queries still parse and answer). One `ANY(...)` lookup per
  `propertyAssets` page covers the derived table (the partial index on
  `thumb_uri IS NOT NULL` keeps it cheap as the backlog grows); rows whose `image_index`
  is at or beyond the asset's CURRENT `propertyImages` array length are dropped in the
  resolver, since a shrunken array leaves orphan rows behind.
- **Observability.** `property_images_mirrored_total{result=success|failure}` counter and
  the `property_images_pending` work-set gauge (deliberately NOT pre-registered: an absent
  series means the mirror is disabled, which the operator can read off the config), plus
  the counter-based `PropertyImageMirrorFailing` alert (same rationale as
  `WebhookDeliveryFailing`).

**Consequences.** The public bucket deliberately serves third-party marketing photography
to API clients — the images are public by design, and only the bucket's write path takes
credentials. This is a new OUTBOUND network surface (arbitrary third-party URIs), guarded
by the same `metadata::uri_allowed` and size/timeout bounds as the ADR-27 fetcher. A
changed source URI orphans the old object in the bucket (a few hundred KB, never
referenced again): the indexer never deletes objects — an object-storage lifecycle rule or
a manual `s3://<bucket>/properties/` sweep is the right tool for pruning, not the loop.
Leftover rows from a shrunken `propertyImages` array are harmless orphans the API filters
out. With the mirror disabled, `propertyImageThumbnails` stays `null` and clients fall
back to the original `property_images` URIs; enabling it later drains the whole backlog
through the work set. Mainnet promotion (agentic-maintenance §8) inherits the surface
unchanged: same table, same loop, a different bucket.
