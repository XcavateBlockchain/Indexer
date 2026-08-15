# Migration Spec: SubQuery → Carbon Solana Indexer

**Audience:** Claude Code, operating inside the existing SubQuery indexer repository.
**Goal:** Replace the SubQuery indexer with a Rust/Carbon indexer, preserving the existing
deployment pipeline, and shipping a GraphQL API and a Grafana dashboard.

---

## 0. How to use this document

Work through the phases in order. Each phase has an explicit **exit check** — do not start
the next phase until the current one's exit check passes.

There are things in here that you **cannot know from memory** and must verify against live
sources or the repo itself. They are marked **`VERIFY`**. Do not guess at these; a wrong
guess here produces code that compiles and silently indexes nothing.

Where this document says **`ASK`**, stop and ask the human rather than picking for them.

Keep a running `MIGRATION_LOG.md` at the repo root as you go: what you changed, what you
found in the old code, what you decided and why. This is a working document, not a
deliverable — it feeds the real docs in Phase 9.

---

## 1. Objective

| Item | Value |
| --- | --- |
| Framework | Carbon (Rust) — `carbon-core` + generated decoder crate |
| Data source (realtime) | Alchemy Solana gRPC (Yellowstone-compatible), key in `ALCHEMY_API_KEY` |
| Data source (backfill) | Alchemy RPC — transaction history + `getProgramAccounts` snapshot |
| Database | PostgreSQL |
| GraphQL API + GraphiQL | port **3010** |
| Grafana | port **3011** |
| Deployment | Hetzner, via the **existing** GitHub Action in this repo |
| Indexed data | Both instruction/event history **and** current account state |

---

## 2. Non-negotiables

These are settled decisions. Do not relitigate them, and do not "improve" on them without
asking:

1. **Commitment level is `confirmed`.** Not `processed` (unreliable), not `finalized`
   (adds ~13s latency).
2. **The account-state table holds current state only.** No history, no versioning.
3. **The account-state table contains only fields that exist on-chain.** No derived
   columns, no counters, no `first_seen_at`. It must be droppable and rebuildable from a
   fresh `getProgramAccounts` snapshot at any time. Derived values live in separate tables
   computed from instruction history.
4. **Every account write is slot-guarded.** See §5.2. This is not optional and is not an
   optimisation.
5. **Instruction history is append-only** and keyed so that reprocessing is a no-op.
6. **Account closes are soft.** Set `closed_at_slot`; never `DELETE`.
7. **Secrets come from the environment.** `ALCHEMY_API_KEY`, `DATABASE_URL`, and Grafana
   credentials are never committed, never in `docker-compose.yml` as literals, never in a
   `.env` that is tracked.
8. **Do not rewrite the existing GitHub Action.** Adapt it. See Phase 8.

---

## 3. Phase 0 — Recon

Read before writing anything. The SubQuery project contains most of the domain knowledge
you need, and re-deriving it from scratch is how details get lost.

### 3.1 Mine the SubQuery project

Locate and read, in this order:

- **`project.yaml` / `project.ts`** — the manifest. Extract:
  - the program address(es) being indexed
  - `startBlock` — this is your backfill floor, reuse it
  - the network endpoint config
  - the handler list: which instructions/accounts SubQuery was filtering on. This tells you
    what actually matters versus what the program merely defines.
- **`schema.graphql`** — SubQuery's entity definitions. This is the single most valuable
  file in the repo. It is a working, production-tested description of the data model the
  consumers of this indexer expect. Your Postgres schema and your new GraphQL schema should
  stay recognisably close to it, because downstream clients are querying it today.
  - Record every entity, field, and relation in `MIGRATION_LOG.md`.
  - Note which entities are *state-like* (one row per on-chain account) versus
    *event-like* (one row per occurrence). This maps directly onto the two-table split
    in Phase 2.
- **`src/mappings/*.ts`** — the handlers. This is the business logic being ported. Pay
  attention to anything that is **not** a straight field copy: computed values,
  cross-entity lookups, conditional writes. Those are the parts that will not fall out of
  the IDL automatically and that you must reimplement by hand in the Rust processors.
- **`src/types/`** — generated types, ignore, they're regenerated artefacts.
- **`docker-compose.yml`** — note the existing port assignments and service names so you
  can tell what the deploy action expects to find.
- **`.github/workflows/`** — read the Hetzner deploy workflow in full. Do not edit yet.

### 3.2 Locate the IDL

Find the Anchor IDL for the program. In order of preference:

1. A checked-in `target/idl/*.json` or `idl/*.json` in this repo or a sibling program repo.
2. Fetched from chain: `carbon-cli parse --idl <PROGRAM_ADDRESS> --url mainnet-beta`.

**`ASK`** if you cannot find an IDL and the program address does not have one published
on-chain. Without it there is no decoder and the migration cannot proceed.

### 3.3 Check how the program emits events

**`VERIFY`** — grep the program source (if available in this repo or a sibling) for `emit!`
and `emit_cpi!`.

- `emit!` writes base64 into program logs. **Logs get truncated under load**, so events
  are silently dropped exactly when throughput is highest. If the program uses `emit!`,
  record this as a known data-integrity risk in `DECISIONS.md` and prefer deriving state
  from instruction data plus account keys wherever the same information is available both
  ways.
- `emit_cpi!` encodes the event as a self-CPI instruction inside the transaction. Reliable.
  Decode it from the instruction pipe.

If the program source is not available, note the uncertainty and default to deriving from
instruction data.

### 3.4 Check for program upgrades

**`VERIFY`** — if the program has been upgraded since `startBlock`, the account layouts and
instruction encodings may differ across slot ranges. Determine whether this has happened.

If it has, you need versioned decoders keyed by slot range, and this materially expands
scope. **`ASK`** before proceeding — this is a decision the human needs to make.

### 3.5 Verify the Alchemy gRPC interface

**`VERIFY`** against Alchemy's current Solana documentation — do not rely on memory:

- The gRPC endpoint URL format for Solana.
- How the API key is passed: URL path segment, `x-token` metadata header, or bearer token.
  This differs between providers and Carbon's Yellowstone datasource needs it configured
  correctly.
- Whether the stream is Yellowstone/Geyser-protocol compatible (it should be — this is what
  lets `carbon-yellowstone-grpc-datasource` work against a non-Triton provider).
- Any subscription limits, filter limits, or plan gating on gRPC access.

Write the findings into `MIGRATION_LOG.md` before writing datasource code.

### 3.6 Verify current Carbon APIs

**`VERIFY`** — Carbon's API has moved across versions. Check, do not assume:

- `https://docs.rs/carbon-core/latest/carbon_core/` for the current module layout.
  Expected: `pipeline`, `datasource`, `processor`, `account`, `instruction`, `transaction`,
  `account_deletion`, `filter`, `metrics`, `postgres`, `graphql`.
- The `examples/` directory in `github.com/sevenlabs-hq/carbon` for runnable references —
  specifically the Geyser streaming, account-state loading, transaction backfill, and
  Postgres-backed GraphQL examples.
- The exact builder method names for account pipes and account-deletion pipes.
- Which cargo features gate the `postgres` and `graphql` modules.

**Exit check for Phase 0:** `MIGRATION_LOG.md` contains the program address, start block,
the full SubQuery entity list classified state-vs-event, the IDL location, the event
emission mechanism, the Alchemy gRPC connection details, and the confirmed Carbon API
shape.

---

## 4. Phase 1 — Decoder

Generate the decoder crate from the IDL:

```bash
npx @sevenlabs-hq/carbon-cli parse \
  --idl ./idl/my_program.json \
  --out-dir ./crates/program-decoder \
  --name my-program
```

Enable the decoder crate's `serde`, `postgres`, and `graphql` features — **`VERIFY`** the
exact feature names from the generated `Cargo.toml`. These are what let the IDL-derived
types flow into the database layer and the GraphQL schema without being hand-redeclared
three times.

Set up the workspace:

```
crates/
  program-decoder/     # generated, do not hand-edit
  indexer/             # pipeline, processors, datasources
  api/                 # Axum + Juniper GraphQL server
```

Keep the decoder crate generated and unmodified. When the IDL changes, it is regenerated.
Anything you hand-write in there will be lost.

**Exit check:** `cargo build` succeeds and the decoder crate exposes a typed enum of
instructions and a typed enum of account types matching the IDL.

---

## 5. Phase 2 — Database schema

Use `sqlx` with checked-in migrations (`migrations/`). No ORM.

### 5.1 Instruction history — append-only

```sql
CREATE TABLE program_instructions (
    signature    BYTEA       NOT NULL,
    ix_index     SMALLINT    NOT NULL,
    inner_index  SMALLINT    NOT NULL DEFAULT -1,  -- -1 = top-level, else CPI position
    slot         BIGINT      NOT NULL,
    block_time   TIMESTAMPTZ NOT NULL,
    ix_name      TEXT        NOT NULL,
    accounts     BYTEA[]     NOT NULL,
    data         JSONB       NOT NULL,
    PRIMARY KEY (signature, ix_index, inner_index)
);

CREATE INDEX idx_pi_slot     ON program_instructions (slot);
CREATE INDEX idx_pi_name_time ON program_instructions (ix_name, block_time DESC);
```

The composite primary key is what makes reprocessing safe. You will replay the same
transactions constantly — on every stream reconnect, and wherever backfill overlaps the
live stream. All writes to this table use `ON CONFLICT DO NOTHING`.

`data` stays JSONB for now. Once the GraphQL query patterns settle, promote the fields that
are actually filtered on into typed columns with indexes. Do not pre-emptively flatten
everything; do not leave hot filter fields in JSONB forever.

### 5.2 Account state — current only, typed columns

One table per account type in the IDL, with real typed columns mirroring the on-chain
struct fields. Not JSONB — the table is bounded by live PDA count rather than by time, so
it stays small, and it regenerates from a snapshot so column migrations are cheap.

Every such table carries this common shape:

```sql
CREATE TABLE <account_type> (
    pubkey         BYTEA PRIMARY KEY,
    slot           BIGINT NOT NULL,       -- write guard, NOT history
    lamports       BIGINT NOT NULL,
    closed_at_slot BIGINT,                -- NULL = live
    -- ... typed columns mirroring the on-chain struct, one per field ...
);
```

**Every write uses this exact pattern:**

```sql
INSERT INTO <account_type> (pubkey, slot, lamports, /* ...fields... */)
VALUES ($1, $2, $3, /* ... */)
ON CONFLICT (pubkey) DO UPDATE SET
    slot     = EXCLUDED.slot,
    lamports = EXCLUDED.lamports,
    -- ...fields...
WHERE <account_type>.slot < EXCLUDED.slot;
```

The `WHERE` clause is the load-bearing part. Without it, a snapshot load or a stream
reconnect overwrites fresh state with stale state. It is a silent corruption bug that
surfaces weeks later as wrong balances, and it is very hard to diagnose after the fact.
Write a test for it: apply slot 200, then apply slot 100, assert the row still reads slot 200.

Account deletions set `closed_at_slot` — same slot guard, never `DELETE`.

### 5.3 Sync state

```sql
CREATE TABLE sync_state (
    id                      SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    last_contiguous_slot    BIGINT NOT NULL,
    backfill_complete       BOOLEAN NOT NULL DEFAULT FALSE,
    backfill_floor_slot     BIGINT NOT NULL,
    snapshot_slot           BIGINT,
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

`last_contiguous_slot` is the highest slot below which there are no gaps — not simply the
highest slot seen. The distinction matters: the naive version reports healthy while you
have holes.

### 5.4 Derived data

Any aggregate the old SubQuery `schema.graphql` exposed that is **not** a raw on-chain
field goes in its own table, computed from `program_instructions`. Do not add it to an
account-state table. Doing so breaks the rebuild-from-snapshot property, which is the whole
reason that table is cheap to operate.

**Exit check:** migrations apply cleanly to an empty database; the slot-guard test passes;
every entity from the old `schema.graphql` maps to something in the new schema, and
`MIGRATION_LOG.md` records the mapping including anything deliberately dropped.

---

## 6. Phase 3 — Pipeline and processors

One decoder, two pipes:

```rust
Pipeline::builder()
    .datasource(alchemy_grpc_datasource)
    .metrics(Arc::new(PrometheusMetrics::new()))
    .instruction(MyProgramDecoder, InstructionProcessor::new(pool.clone()))
    .account(MyProgramDecoder, AccountProcessor::new(pool.clone()))
    // account-deletion pipe — VERIFY exact builder method name
    .build()?
    .run()
    .await
```

**`VERIFY`** the account and account-deletion builder method names and the processor trait
signatures against docs.rs and `examples/` before writing this out. The instruction pipe
shape above matches the documented quick start; the others need checking.

Datasource configuration:

- Yellowstone gRPC datasource pointed at Alchemy, key read from `ALCHEMY_API_KEY`.
- Filter as narrowly as possible: your program ID only, `vote: false`, `failed: false`
  unless the old SubQuery handlers were tracking failures — check.
- Commitment `confirmed`.

Processor requirements:

- **Batch writes.** One transaction per update is a throughput disaster. Buffer and flush
  on a size threshold or a short timer, whichever comes first.
- **Idempotent by construction.** Instruction writes use `ON CONFLICT DO NOTHING`; account
  writes use the slot-guarded upsert. Never a bare `UPDATE`.
- **Advance `sync_state` only after a successful commit**, and only for contiguous ranges.
- **Port the non-trivial SubQuery mapping logic** identified in §3.1. Field copies fall out
  of the decoder; computed values and cross-entity lookups do not.

**Exit check:** run against devnet (or mainnet with a short slot window), send transactions
at the program, and confirm decoded rows land in both tables with correct values. Do this
before writing any API code — the majority of indexer bugs are decode bugs and they are
cheapest to find here.

---

## 7. Phase 4 — Backfill

Order matters. Run it this way and only this way:

1. **Start the live stream first.** Let it write account updates through the slot-guarded
   upsert immediately. Record the slot at which the stream connected.
2. **Take the `getProgramAccounts` snapshot**, tagged with the slot it was taken at. Store
   it in `sync_state.snapshot_slot`.
3. **Write the snapshot through the same slot-guarded upsert.** Anything the live stream
   already delivered at a higher slot survives untouched. This is precisely what the guard
   is for.
4. **Backfill transaction history separately**, walking backwards from the snapshot slot to
   `backfill_floor_slot` (the SubQuery `startBlock` from §3.1). Set `backfill_complete` when
   it reaches the floor.

Snapshotting before connecting the stream leaves a gap the width of however long the
snapshot took. Do not do it that way, and put a comment in the code saying why, because it
looks like unnecessary complexity to anyone reading it later.

Backfill must be **resumable and re-runnable**. It will be interrupted. Make it a separate
binary or a subcommand so it can be run by hand against production without redeploying.

**Exit check:** a full backfill from `startBlock` completes; row counts for a sampled slot
range match what the old SubQuery instance reports; running backfill a second time changes
no rows.

---

## 8. Phase 5 — GraphQL API on port 3010

Use Carbon's `graphql` module (Juniper schema + Axum integration) in the `api` crate.

- Bind Axum to `0.0.0.0:3010`.
- `POST /graphql` — the API endpoint.
- `GET /graphiql` — the GraphiQL IDE, served via `juniper_axum`'s GraphiQL handler pointed
  at `/graphql`. **`ASK`** whether GraphiQL should be exposed in production or gated behind
  an env flag; default to enabled since the request specified it, but flag the exposure.
- `GET /health` — readiness, returning `last_contiguous_slot` and its lag behind chain tip.

**Schema design:** stay close to the old `schema.graphql`. Downstream clients are querying
that shape today and gratuitous renaming turns a backend migration into a client migration.
Where you must diverge, document each divergence in `DECISIONS.md` with the reason.

**Mandatory guards** — an auto-derived GraphQL API over an append-only history table is a
denial-of-service waiting to happen:

- Hard maximum page size on every list field. Pick a number, enforce it server-side,
  ignore any larger client request rather than erroring.
- Query depth limit.
- Query complexity limit.
- Statement timeout on the Postgres connection used by resolvers.

Someone will eventually issue a query that tries to walk the program's entire history in
one request. Cap it now, not after.

> **Alternative considered:** point PostGraphile or Hasura at the same Postgres database
> and get filtering, pagination, relay connections and subscriptions with no Rust. Rejected
> here because the requirement is for the Carbon indexer itself to serve the API, and a
> single binary is one less service to deploy on the Hetzner box. Record this in
> `DECISIONS.md` — if the API surface grows significantly, revisiting it is reasonable.

**Exit check:** GraphiQL loads at `:3010/graphiql`, introspection works, a representative
query from the old SubQuery API returns equivalent data, and a deliberately abusive query
is rejected by the limits rather than hanging.

---

## 9. Phase 6 — Observability, Grafana on port 3011

Carbon exports pipeline metrics through Prometheus. Wire the chain:

1. **Indexer** exposes `/metrics` on an internal port (not published to the host).
2. **Prometheus** container scrapes it. Internal to the Docker network only — do not
   publish Prometheus to the host.
3. **Grafana** on host port **3011**, provisioned as code.

Grafana must be provisioned via files, not clicked together by hand:

```
grafana/
  provisioning/
    datasources/prometheus.yml
    dashboards/dashboards.yml
  dashboards/indexer.json
```

Dashboard panels, at minimum:

- **Slot lag** — chain tip minus `last_contiguous_slot`. This is the single most important
  number; everything else is diagnostic.
- Updates processed per second, split by pipe (instruction / account / deletion).
- Decode failure rate, by instruction and account type. A nonzero rate after a program
  upgrade is your early warning.
- Database write latency (p50 / p95 / p99) and batch flush size.
- gRPC stream connection state and reconnect count.
- Backfill progress: current slot versus `backfill_floor_slot`.
- GraphQL request rate, latency, and rejected-by-limit count.

Alerts: slot lag above threshold, decode failure rate above zero, stream disconnected,
backfill stalled.

Grafana admin credentials come from the environment. Never `admin`/`admin`, never
committed.

**Exit check:** Grafana reachable on `:3011`, dashboards present on a fresh
`docker compose up` with no manual configuration, slot lag panel showing live data.

---

## 10. Phase 7 — Docker Compose

Services: `postgres`, `indexer`, `api`, `prometheus`, `grafana`.

Published to the host: **3010** (api) and **3011** (grafana) only. Everything else stays on
the internal network.

Requirements:

- Multi-stage Rust build with `cargo-chef` or equivalent dependency caching. A naive
  Dockerfile rebuilds the entire dependency tree on every source change and will make CI
  miserable.
- Healthchecks on every service; `indexer` and `api` `depends_on` postgres being healthy.
- Named volumes for Postgres data and Grafana state.
- `restart: unless-stopped`.
- Migrations run on `indexer` startup, before the pipeline connects.
- All secrets injected from the environment. Provide a tracked `.env.example` listing every
  variable with a description and no values.

Required environment variables — document all of these in `.env.example` and the README:

| Variable | Purpose |
| --- | --- |
| `ALCHEMY_API_KEY` | Alchemy gRPC + RPC authentication |
| `ALCHEMY_GRPC_URL` | gRPC endpoint (format per §3.5) |
| `ALCHEMY_RPC_URL` | HTTP RPC for backfill and snapshot |
| `DATABASE_URL` | Postgres connection string |
| `PROGRAM_ID` | Indexed program address |
| `BACKFILL_START_SLOT` | From the SubQuery `startBlock` |
| `GRAFANA_ADMIN_USER` / `GRAFANA_ADMIN_PASSWORD` | Grafana auth |
| `RUST_LOG` | Log filter |

---

## 11. Phase 8 — Deployment (reuse the existing Hetzner action)

**Read the existing workflow completely before touching it.** Then adapt, do not replace.

Specifically, preserve:

- The SSH/authentication mechanism and the secret names it already uses.
- The target host reference and any environment/branch gating.
- The deploy strategy (whether it builds an image and pushes, or pulls and builds on the
  host).
- Any pre/post hooks, notifications, or health gates already wired up.

What needs to change:

- Build step: Node/SubQuery → Rust. Add `cargo build --release` and Rust toolchain caching.
- Add `cargo clippy -- -D warnings`, `cargo fmt --check`, and `cargo test` as gates.
- Add `sqlx` offline query verification (`cargo sqlx prepare --check`) so schema drift fails
  in CI rather than at runtime — commit `.sqlx/`.
- New secrets: `ALCHEMY_API_KEY`, `GRAFANA_ADMIN_PASSWORD`, and anything else from §10 not
  already present. **List every new secret explicitly in the migration log and the README**
  — the human has to add these to the repo settings by hand and there is no way for you to
  do it.
- Compose service names and published ports, if the action references them.

Do not remove SubQuery-related workflow steps until the Carbon deploy is verified working.
Leave them disabled or commented with a note, so there is a rollback path.

**`ASK`** before changing anything about how the action authenticates to Hetzner or which
host it targets.

---

## 12. Phase 9 — Documentation

All of the following are deliverables, not optional:

**`README.md`** — what this indexes and why; quickstart from clone to running locally; the
full environment variable table; port map (3010 GraphQL/GraphiQL, 3011 Grafana); how to run
a backfill; how to regenerate the decoder after an IDL change.

**`ARCHITECTURE.md`** — the pipeline (datasource → decoder → processors → Postgres → API);
the two-table split and why; the backfill ordering and why the stream starts first; where
each piece of the old SubQuery logic ended up.

**`DECISIONS.md`** — one short ADR per decision, each with context, decision, and
consequences. At minimum:

1. Carbon over SubQuery — one framework covering instruction decoding and account decoding
   from the same IDL, avoiding two parallel deserialization paths that drift.
2. Account state is current-only — the table is a disposable mirror, rebuildable from a
   snapshot in minutes.
3. Account-state tables contain only on-chain fields — preserves the rebuild property;
   derived values live in separate tables.
4. Typed columns for account state, JSONB for instruction payloads — the former is bounded
   by PDA count and cheap to migrate, the latter is unbounded and its query patterns aren't
   settled yet.
5. `confirmed` commitment — the latency/reliability balance.
6. Slot-guarded upserts — prevents stale-overwrites-fresh corruption from reconnects and
   snapshot loads.
7. Soft deletes via `closed_at_slot` — a close observed at `confirmed` can in principle roll
   back, and the history is usually wanted anyway.
8. Stream-before-snapshot backfill ordering — eliminates the gap.
9. Carbon's built-in GraphQL over PostGraphile/Hasura — single binary, single deploy; note
   the tradeoff and the conditions under which to revisit.
10. Whatever you found in §3.3 about `emit!` vs `emit_cpi!`, and its integrity implications.
11. Any divergence from the old `schema.graphql`, with reasons.

**`RUNBOOK.md`** — how to tell if the indexer is behind (slot lag panel); how to rebuild
account state from a snapshot; how to re-run a backfill for a slot range; what to do after
a program upgrade (regenerate decoder, check decode failure rate, consider versioned
decoders); how to roll back to SubQuery if needed.

**Inline:** rustdoc on every public item. Explicit comments on the slot guard and the
backfill ordering explaining *why* — both look like removable complexity to a reader who
doesn't know the failure mode they prevent.

---

## 13. Acceptance checklist

- [ ] `docker compose up` from clean gives a working stack with no manual steps
- [ ] GraphiQL loads at `:3010/graphiql`; representative old-SubQuery queries return
      equivalent data
- [ ] Grafana at `:3011` with provisioned dashboards showing live slot lag
- [ ] Slot-guard test passes (apply slot 200, then slot 100, row stays at 200)
- [ ] Backfill is resumable and idempotent — second run changes zero rows
- [ ] Row counts match the old SubQuery instance for a sampled slot range
- [ ] Abusive GraphQL query rejected by limits rather than hanging
- [ ] CI passes: clippy clean, fmt, tests, `sqlx prepare --check`
- [ ] Existing Hetzner action deploys successfully with its auth mechanism unchanged
- [ ] Every new secret is listed explicitly for the human to add
- [ ] README, ARCHITECTURE, DECISIONS, RUNBOOK all written
- [ ] No secrets in tracked files
- [ ] SubQuery deploy path left disabled rather than deleted

---

## 14. Things to stop and ask about

- No IDL available (§3.2)
- Program has been upgraded since `startBlock` — versioned decoders needed (§3.4)
- Alchemy gRPC unavailable on the current plan, or not Yellowstone-compatible (§3.5)
- SubQuery mapping logic depends on something with no clean Carbon equivalent (§3.1)
- Whether GraphiQL should be publicly exposed in production (§8)
- Any change to how the deploy action authenticates or which host it targets (§11)
- Old `schema.graphql` entities that cannot be reproduced from on-chain data alone

Report these rather than picking a plausible-looking option. A wrong guess in any of them
produces an indexer that appears to work and quietly holds bad data.
