# Migration log — SubQuery → Carbon

Working notes for the migration (spec: `docs/plans/carbon-migration-spec.md`).
Not a deliverable; feeds README/ARCHITECTURE/DECISIONS/RUNBOOK in Phase 9.

## Phase 0 — Recon (2026-08-15)

### Program & network (§3.1)

- **Network: Solana devnet** (chainId/genesis `EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG`).
- **Program indexed by SubQuery:** `xcavate_whitelist` = `2vVARM46pPD4rcHdbXHnYA4vTGN14q6skQAzsQWcHUxn`.
- **startBlock / backfill floor:** `483386556` (the program's deployment slot).
- Old endpoint config: Alchemy devnet JSON-RPC `https://solana-devnet.g.alchemy.com/v2/<key>` primary,
  `https://api.devnet.solana.com` public fallback (key injected via compose, never committed).
- **Handler list (9, all instruction handlers — no account/log handlers):**
  `initialize_config`, `update_authority`, `accept_authority`, `add_admin`, `remove_admin`,
  `assign_role`, `remove_role`, `renounce_role`, `set_permission`. Failed transactions were
  NOT indexed (SubQuery default; handlers assume successful instructions).
- The repo also holds addresses for three sibling programs (`regions`
  `FYysH5v23qtz4gK4H1yLDHneFwx6PSAT7oQwHcuRyRh`, `marketplace`
  `B6YRVAmjmhN28smZxNfCnuKc19CamBbAEMXsp5KTfWog`, `property`
  `8f4NHc1wGBM1BAufDFd9dNechLW8pxmStSfxfuJfDzob`) — out of scope for the SubQuery indexer
  (docs/design.md §8) but four fresh IDLs (one per program) were just dropped into `idls/`.
  **Scope question raised with the user.**

### Old entity model (schema.graphql) — state-like vs event-like

| Entity | Kind | Notes |
| --- | --- | --- |
| `Config` (id="config") | state-like (singleton; mirrors Config PDA) | `authority`, `pendingAuthority?`, updatedAtBlock/At/InTx |
| `Admin` (id=admin pubkey) | state-like (mirrors Admin PDA `["admin", key]`) | `active` soft-delete + addedBy/addedAt*/removedAt* audit fields (DERIVED from instructions, not on-chain) |
| `RoleAssignment` (id=`<user>-<roleIndex>`, mirrors RoleAccount PDA `["role", user, role_byte]`) | state-like | `user`, `role`, `permission`, `active` + rentPayer/assignedBy/removalKind/removedBy audit fields (rentPayer IS on-chain per IDL; assignedBy etc. derived) |
| `WhitelistAction` (id=`<txSig>-<ixPath>`) | event-like (append-only audit log) | type/subject/role/permission/actor + block coords |

Enums: `Role` (6 variants, borsh index = PDA seed byte, declaration order load-bearing),
`Permission` (COMPLIANT=0, REVOKED=1), `RemovalKind` (REMOVED|RENOUNCED — disambiguated by
which instruction closed the PDA, not by on-chain data), `ActionType` (9 values).

**Important:** old entities store Solana **block heights**, not slots (design.md §5). The new
schema uses slots throughout (spec §5.1/§5.2) — a documented divergence (DECISIONS #11).

### Non-trivial mapping logic to port (src/mappings/mappingHandlers.ts)

- Account addresses resolved by **position** in the instruction's account list
  (static keys + loaded lookup-table addresses): authority=accounts[0] everywhere;
  new_admin/user = accounts[2] for add_admin/assign_role/remove_role/set_permission;
  renounce_role user = accounts[0].
- `initialize_config` → Config row created, pendingAuthority=None.
- `update_authority(new_authority)` → sets `pendingAuthority` (two-step handover; re-propose overwrites).
- `accept_authority` → authority=accounts[0], pendingAuthority=None.
- `add_admin` → Admin row **create-or-reset** (re-add of removed admin resets audit fields).
- `remove_admin(admin_key)` → soft-delete by **instruction arg** (not account position).
- `assign_role(role)` → RoleAssignment create-or-reset, permission defaults to COMPLIANT.
- `remove_role(role)` / `renounce_role(role)` → soft-delete; removalKind REMOVED vs RENOUNCED
  by instruction; removedBy = admin signer vs the user.
- `set_permission(role, permission)` → updates permission only.
- Every handler appends one WhitelistAction (id = `sig-ixPath`, ixPath dot-joined for inner ixs).
- Handlers throw on violated invariants (unknown admin/assignment, decode failure) — for a
  compliance registry, integrity beats liveness. Port this stance: decode/link failures must
  be loud (metrics + log), not silently skipped.

### IDL (§3.2)

- **Found, checked in:** `idls/xcavate_whitelist.json` (fresh, Anchor spec 0.1.0 format;
  supersedes the previously hand-authored `idls/xcavate_whitelist.idl.json`, now deleted).
  Accounts: `Admin`, `Config`, `RoleAccount`. Instructions: the 9 above. Events: 8.
- Sibling IDLs also present: `marketplace.json` (36 ix / 9 accounts / 34 events),
  `property.json` (13 ix / 6 accounts / 13 events), `regions.json` (16 ix / 6 accounts / 17 events).
- No IDL published on-chain (design.md §4); the checked-in files are the source of truth.

### Event emission (§3.3)

- No `event_authority` account in any instruction of any of the four IDLs ⇒ programs use
  **`emit!` (log-based)**, not `emit_cpi!`. Logs are truncatable under load.
- The old indexer already ignores events entirely and derives all state from instruction
  data + account keys — every event payload is recoverable from its instruction
  (design.md §4 verified this). The new indexer keeps that approach. → DECISIONS #10.

### Program upgrades (§3.4)

Checked via devnet RPC (`getAccountInfo` on each ProgramData account, 2026-08-15):

| Program | lastDeploySlot | Upgraded since deploy? |
| --- | --- | --- |
| xcavate_whitelist | 483386556 | No (== startBlock) |
| regions | 483386626 | No |
| marketplace | 483386726 | No |
| property | 483386809 | No |

All four share upgrade authority `7bGxnDFi3zKLAbgeXtCANcf8MGSYob1EAmoWZY77qjp2`.
**No versioned decoders needed.** Devnet chain tip at check time: ~484,179,475
(~793k slots ≈ a few days of history — backfill is small).

### Alchemy gRPC (§3.5) — verified against live docs 2026-08-15

- Product: **Alchemy Yellowstone gRPC** — supported on **mainnet AND devnet**.
  Devnet endpoint: `https://solana-devnet.g.alchemy.com` (TLS; port not stated in docs,
  443 implied — confirm empirically).
- Auth: API key as **`X-Token` gRPC metadata header** — "not in the URL"
  (docs.alchemy.com Yellowstone gRPC quickstart). Drop-in for `yellowstone-grpc-client`'s
  `.x_token(...)`.
- Yellowstone-compatible: yes — official examples use the standard
  `yellowstone_grpc_client`/`yellowstone_grpc_proto` crates. Proto version not pinned in
  docs; pin ours and test connectivity.
- **Plan gating: PAYG or Enterprise only — NOT available on the free tier**
  (quickstart prerequisites). Usage-billed at $75/TB. This repo previously documented
  free-tier CU throttling, so the account tier is a blocker question → **asked the user**.
- Replay window: 6,000 slots / 48h block replay via `from_slot`.
- Numeric filter/stream limits: not documented.
- Fallback if free tier: Alchemy devnet **WebSocket** subscriptions
  (`accountSubscribe`/`programSubscribe`/`logsSubscribe`/`slotSubscribe`) are documented on
  devnet with a 100-connection cap on free tier (`wss://solana-devnet.g.alchemy.com/v2/<key>`);
  `blockSubscribe` undocumented on Alchemy. Third-party devnet Yellowstone gRPC: Helius
  LaserStream (devnet on their paid Developer plan and above); others unverified.

### Carbon APIs (§3.6) — verified against crates.io / docs.rs / repo `main` 2026-08-15

- **carbon-core 1.0.0** (all carbon crates in lockstep at 1.0.0). Modules: account,
  account_deletion, account_utils, block_details, collection, datasource, deserialize,
  error, filter, instruction, metrics, pipeline, processor, transaction, transformers.
  **`postgres` and `graphql` are feature-gated modules inside carbon-core** (not separate
  crates): `postgres = ["sqlx", "num-traits", "sqlx_migrator", "bigdecimal"]`,
  `graphql = ["juniper", "axum", "juniper_axum"]`. graphql::server exposes `build_schema` +
  `graphql_router` mounting `/graphql` and `/graphiql` on an Axum Router.
- **PipelineBuilder** (exact): `.datasource(ds)`, `.datasource_with_id(ds, id)`,
  `.instruction(decoder, processor)`, `.account(decoder, processor)`,
  `.account_deletions(processor)`, `.block_details(processor)`, `.transaction::<C>(processor)`,
  `_with_filters` variants, `.metrics(Arc<dyn MetricsExporter>)`,
  `.shutdown_strategy(ShutdownStrategy::Immediate|ProcessPending)`,
  `.datasource_cancellation_token(token)`, `.channel_buffer_size(n)`, `.build()` → `.run().await`.
- **Processor trait**: `trait Processor<T: Sync> { fn process(&mut self, data: &T) -> impl Future<Output = CarbonResult<()>> + Send; }`
  (native AFIT, Rust ≥1.75 — write plain `async fn process`). Inputs:
  `InstructionProcessorInputType<'a, T> { metadata, decoded_instruction, nested_instructions, raw_instruction }`,
  `AccountProcessorInputType<'a, T> { metadata, decoded_account, raw_account }`,
  `AccountDeletion { pubkey, slot, transaction_signature }` (bare struct, no wrapper).
- **Decoder codegen**: npm CLI `@sevenlabs-hq/carbon-cli`:
  `npx @sevenlabs-hq/carbon-cli parse -i <idl.json> -o <dir> -n <name> -s anchor`
  with `--with-postgres/--with-graphql` (default true), `--with-serde` (default false),
  `--postgres-mode typed|generic` (default typed), `--standalone` (default true; pass false
  for a workspace-member crate), `--as-crate`. Generated crate `carbon-<name>-decoder`
  exposes an instruction enum (impl `InstructionDecoder`) + account enum (impl
  `AccountDecoder`); features: `serde`, `postgres = ["carbon-core/postgres", sqlx,
  async-trait, sqlx_migrator, serde]`, `graphql = ["carbon-core/graphql", juniper, serde]`.
- **Realtime datasource**: `carbon-yellowstone-grpc-datasource` 1.0.0 —
  `YellowstoneGrpcGeyserClient::new(endpoint, x_token: Option<String>,
  commitment: Option<CommitmentLevel>, account_filters: HashMap<String, SubscribeRequestFilterAccounts>,
  transaction_filters: HashMap<String, SubscribeRequestFilterTransactions>, block_filters,
  account_deletions_tracked: Arc<RwLock<HashSet<Pubkey>>>, geyser_config, disconnect_notifier, stream_timeout)`.
  Tx filter example: `SubscribeRequestFilterTransactions { vote: Some(false), failed: Some(false),
  account_required: vec![PROGRAM_ID], .. }`. Uses `yellowstone-grpc-client/proto` **v10.0.0**.
- **Backfill datasources** (all 1.0.0): `carbon-rpc-transaction-crawler-datasource`
  (`RpcTransactionCrawler::new(rpc_url, account, ConnectionConfig, Filters { accounts,
  before_signature, until_signature }, commitment)` — getSignaturesForAddress walk);
  `carbon-rpc-gpa-datasource` (`GpaDatasource::new(rpc_url, program_id)` — gPA snapshot);
  `carbon-rpc-block-crawler-datasource` (slot-range block crawl). Realtime non-gRPC
  fallbacks exist too (`rpc-block-subscribe`, `rpc-program-subscribe`).
- **Metrics**: `carbon-prometheus-metrics` 1.0.0 implements `MetricsExporter`; default
  `http-server` feature serves Prometheus text at `0.0.0.0:9464/metrics`
  (`PrometheusServerConfig { listen_addr, .. }`). Pipeline built-ins:
  `carbon_updates_{received,processed,successful,failed}_total`, `carbon_updates_queued`,
  process-time histograms, per-pipe counters
  (`carbon_account_updates_processed_total`, `carbon_transaction_updates_processed_total`,
  `carbon_account_deletions_processed_total`), plus per-datasource metrics
  (e.g. `yellowstone_grpc_*`, `transaction_crawler_*`).
- **Toolchain**: repo pins Rust 1.88.0; published-crate MSRV 1.82. No system `protoc`
  needed (yellowstone-grpc-proto vendors it); `libclang` only needed for the
  validator-snapshot datasource (we don't use it).
- **Reference examples** in sevenlabs-hq/carbon `examples/`: `yellowstone-grpc`,
  `postgres-graphql`, `transaction-crawler-rpc`, `gpa-rpc`, `block-subscribe-rpc`,
  `versioned-decoders`, `custom-datasource`.

## Decisions from the user (2026-08-15)

1. **Scope: whitelist only.** The other three IDLs stay in `idls/` for a later phase; the
   pipeline layout must leave room (per-program decoder crates, addresses.json canonical).
2. **grpc-api: dropped from the active stack.** Removed from the new compose/CI/deploy.
   The GraphQL API on 3010 is the only query surface. Source stays in-repo for the
   SubQuery rollback path until the migration is verified.
3. **Realtime: Alchemy Yellowstone gRPC** (account is/will be PAYG) —
   `https://solana-devnet.g.alchemy.com`, X-Token auth, as specced.
4. **GraphiQL: enabled in production** (spec default), protected by the DoS guards.

### Existing serving/deploy surface (for Phases 5–8)

- `grpc-api/` (TypeScript, :50051): WhitelistService — CheckAccess/GetConfig/
  GetRoleAssignment/ListRoleAssignments/ListAdmins/ListActions/GetIndexerStatus + health.
  All SQL confined to `grpc-api/src/db/queries.ts` (by design, for schema drift). Reads
  `app.configs|admins|role_assignments|whitelist_actions|_metadata`. Breaks when SubQuery
  tables go away → fate raised with the user.
- SubQuery GraphQL (`subquerynetwork/subql-query`) on :3010 with playground.
- Monitoring already present: Prometheus (loopback-only, v3.13.2) + Grafana (:3011,
  provisioned from `monitoring/`, admin password via `GRAFANA_PASSWORD`).
- Deploy (`.github/workflows/deploy.yml`): builds+pushes GHCR images
  (`node`, `grpc`, `postgres`) tagged `latest`+sha → SSH (`HETZNER_SSH_KEY/HOST/USER/
  KNOWN_HOSTS/SSH_PORT`) → upload compose+monitoring+rendered `.env` to `/opt/indexer` →
  `docker compose pull && up -d --remove-orphans` → verify `/ready`, prometheus, grafana →
  prune old images. Secrets in use: `HETZNER_*`, `ALCHEMY_API_KEY`, `POSTGRES_PASSWORD`,
  `GRAFANA_PASSWORD`, optional `GHCR_PULL_TOKEN`. Vars: `GRAPHQL_PORT`(3010),
  `GRPC_PORT`(50051), `GRAFANA_PORT`(3011), `PROMETHEUS_PORT`(9090).
- CI (`ci.yml`): SubQuery codegen/build/tsc + grpc-api build. Needs Rust jobs.
- Local `.env` exists (gitignored) with `ALCHEMY_API_KEY` and `POSTGRES_PASSWORD` —
  live-chain testing is possible locally.

### Known constraints & risks carried over

- Devnet ledger resets can orphan history; recovery = wipe DB volume + re-run backfill
  (minutes at this program's volume).
- Alchemy free tier: 30M CU/month, 500 CU/s (old stack tuned batch-size/workers for this).
- Old stack ran `--unfinalized-blocks=true`; new stack indexes at `confirmed` with
  slot-guarded upserts and soft closes; confirmed-level rollbacks are theoretically
  possible and are a documented residual risk (DECISIONS #5/#7).

## Phase 2 — Database schema (2026-08-15)

Migrations live in `migrations/` (repo root, `sqlx::migrate!()`-compatible, applied in
order `0001`..`0005`). Storage layer: `crates/indexer/src/db/` (`accounts.rs`, `actions.rs`,
`instructions.rs`, `sync_state.rs`, `models.rs`), exposed as this crate's library target
(`crates/indexer/src/lib.rs`). Full writeup, public API signatures, and test output:
`.superpowers/sdd/carbon-migration-spec/task-2-report.md`.

Every entity in the old `schema.graphql` maps onto something in the new schema — extending
the Phase 0 table above with the concrete names:

| Old entity (`schema.graphql`) | Old kind | New schema |
| --- | --- | --- |
| `Config` (id="config") | state-like singleton, on-chain fields + derived `updatedAt*` mixed into one entity | `config` table — **state only** (`pubkey`, `slot`, `lamports`, `closed_at_slot`, `authority`, `pending_authority`, `bump`); `config_view` — derived (`authority`/`pending_authority` read straight from `config`; `updated_at_slot`/`updated_at`/`updated_in_tx` folded from the latest `CONFIG_INITIALIZED`/`AUTHORITY_UPDATE_PROPOSED`/`AUTHORITY_UPDATED` row in `whitelist_actions`) |
| `Admin` (id=admin pubkey) | state-like + derived audit fields (`active`, `addedBy`/`addedAt*`, `removedAt*`) mixed into one entity | `admin` table — **state only** (`pubkey`, `slot`, `lamports`, `closed_at_slot`, `admin`, `bump`); `admins_view` — all the derived audit fields, folded from `whitelist_actions` (order-insensitive by construction, ruling R7) |
| `RoleAssignment` (id=`<user>-<roleIndex>`) | state-like + derived audit fields (`active`, `assignedBy`, `removedAt*`, `removalKind`, `removedBy`) mixed into one entity | `role_account` table — **state only** (`pubkey`, `slot`, `lamports`, `closed_at_slot`, `user_pubkey` [renamed from on-chain `user`, reserved word], `role`, `permission`, `rent_payer`, `bump`); `role_assignments_view` — all the derived audit fields, folded from `whitelist_actions` |
| `WhitelistAction` (id=`<txSig>-<ixPath>`) | event-like, append-only | `whitelist_actions` table — same shape/identity, but `slot` (ruling R8) instead of `blockHeight` |
| *(none — new)* | — | `program_instructions` table — raw append-only instruction history (`ON CONFLICT DO NOTHING`); the old SubQuery stack had no equivalent, this is new infrastructure the Task 3 processors write to (and that `whitelist_actions` / the views are derivable from, in principle, though the views fold over `whitelist_actions` directly for simplicity) |
| *(none — new)* | — | `sync_state` table — pipeline bookkeeping (`last_contiguous_slot`, `backfill_complete`, `backfill_floor_slot`, `snapshot_slot`); the old stack tracked equivalent progress in SubQuery's own internal `_metadata` table, which `grpc-api`'s `GetIndexerStatus` read directly — that read will need to move to `sync_state` when `grpc-api` (or its replacement) is repointed |

Enum mapping (all TEXT + CHECK, old `schema.graphql` spellings preserved so downstream
GraphQL/gRPC consumers don't have to change):

| Old `schema.graphql` enum | New schema |
| --- | --- |
| `Role` (6 variants, borsh index load-bearing) | `role_account.role`, `whitelist_actions.role`, `role_assignments_view.role` — same 6 spellings, same index order (documented in `migrations/0002_account_state.sql`) |
| `Permission` (`COMPLIANT`/`REVOKED`) | `role_account.permission`, `whitelist_actions.permission`, `role_assignments_view.permission` |
| `RemovalKind` (`REMOVED`/`RENOUNCED`) | `role_assignments_view.removal_kind` — derived only (which instruction closed the PDA), no stored column, same as the old design |
| `ActionType` (9 values) | `whitelist_actions.type` — same 9 spellings |

Nothing was dropped. The one structural change (spec §5.2 non-negotiable #3, controller
ruling R7) is that "state" and "derived/audit" fields, which the old entities mixed into one
row each, are now split: the `*_state` tables hold only fields that exist on-chain (so they
stay droppable/rebuildable from a `getProgramAccounts` snapshot), and every derived/audit
field moved into a view folded from `whitelist_actions`.

## Phase 3 — Pipeline and processors (2026-08-15)

`crates/indexer` is now a clap binary (`run` / `replay` / `smoke-grpc`, plus `backfill` and
`snapshot` stubs for Phase 4) around a Carbon 0.12.0 pipeline: one decoder, an instruction
pipe, an account pipe, an account-deletion pipe, a batched single writer, a Prometheus
`/metrics` listener, and the Yellowstone gRPC datasource pointed at Alchemy devnet. Full
writeup, module map, and exit-verification output:
`.superpowers/sdd/carbon-migration-spec/task-3-report.md`.

### Handler mapping — old TypeScript to new Rust

| Old (`src/mappings/mappingHandlers.ts`) | New |
| --- | --- |
| `metaOf(ix)` (`txSignature`, `blockHeight`, `blockTime`, `instructionIndex`) | `mapping::map_instruction` reads `InstructionMetadata`/`TransactionMetadata`; `blockHeight` became `slot` (ruling R8) and `instructionIndex` is `mapping::instruction_index(absolute_path)` — same dot-joined format |
| `accountAt(ix, n)` | `mapping::account_at(decoded.accounts, n)` — carbon has already resolved static + lookup-table keys, so there is no key-index indirection to redo |
| `decodedArgs(ix)` | the generated decoder's `InstructionDecoder`; instructions that fail to decode never reach the mapper |
| `recordAction(...)` | one `WriteOp::InsertAction` per instruction |
| `Config.create/save`, `Admin.create/save`, `RoleAssignment.create/save` (order-sensitive in-place mutation) | **gone** — replaced by `whitelist_actions` + the SQL views (ruling R7). Account state comes from the account pipe instead, straight off chain |
| `invariant(...)` throwing to halt indexing | `mapping::MappingError` -> `decode_skipped_total{reason}` + an error log + a failed update (carbon's `updates_failed`). Same stance: data integrity over liveness |
| the 3 instructions that close a PDA | additionally emit `WriteOp::CloseAdmin` / `CloseRoleAccount` (ruling R11); the old handlers set `active = false` on the entity instead |

### Findings

- **The Yellowstone transaction stream carries no `block_time`.** carbon's yellowstone
  datasource passes `None` for `UpdateOneof::Transaction` (only the far heavier `blocks`
  subscription has it), so `TransactionMetadata::block_time` is `None` on the live path and
  `Some` on the RPC-crawler path. `block_time::BlockTimeResolver` fills the gap with a cached
  `getBlockTime(slot)` (ruling R14). Both `block_time` columns are `NOT NULL`, so this is not
  optional.
- **carbon 0.12.0's yellowstone datasource cannot express a `slots` or `blocks_meta`
  subscription** — both maps are hardcoded empty in its `SubscribeRequest`, and it swallows
  subscribe errors inside a spawned task. `smoke-grpc` therefore drives
  `yellowstone_grpc_client::GeyserGrpcClient` directly (same endpoint, x-token, commitment,
  TLS config path and filters) so it can (a) get a heartbeat on an idle program and (b) see an
  auth/plan rejection. See `crates/indexer/src/grpc_smoke.rs`.
- **The RPC transaction crawler drops updates unless `blocking_send` is true.** Its default
  (`false`) uses `try_send` and logs a warning when the pipeline channel is momentarily full;
  for a backfill that is silent history loss. `pipeline::build_replay` sets it to `true`.
- **The crawler never terminates** — after exhausting history it polls forever. The `replay`
  subcommand detects completion with an idle watchdog over carbon's `updates_received`.
- **Build on Windows**: `carbon-yellowstone-grpc-datasource` pulls in `yellowstone-grpc-proto`,
  whose build script unconditionally builds a vendored protobuf from source via autotools
  (`protobuf-src`). That configure script cannot be driven with MSVC on this host. Workaround
  (host-local, nothing committed) is in the Task 3 report. On Linux/CI this builds normally.
- **Devnet history is tiny**: 12 signatures ever touched the program as of 2026-08-15 (1
  deploy + 1 `initialize_config` + 2 `add_admin` + 8 `assign_role`), and 11 program-owned
  accounts exist (1 `Config`, 2 `Admin`, 8 `RoleAccount`). No removals or permission updates
  have ever happened on chain, so those paths are covered by unit/integration tests only.

## Phase 4 — Backfill: snapshot, history walk, contiguity (2026-08-16)

Completes the data-completeness story: a `getProgramAccounts` snapshot, a resumable history
backfill, startup orchestration in `run`, and a periodic reconciliation supervisor that owns
`sync_state.last_contiguous_slot`.

### The division of labour (the thing to know about this indexer)

| Path | Job | Owns |
| --- | --- | --- |
| Yellowstone gRPC stream | **freshness** — a new whitelist transaction is in the database in ~1 s | `program_instructions`, `whitelist_actions`, live account state |
| `getSignaturesForAddress` crawl (backfill + reconcile) | **completeness** — "nothing below slot T is missing" | `sync_state.last_contiguous_slot`, `backfill_complete` |
| `getProgramAccounts` snapshot | **current state** on a program that may be idle for days | `config` / `admin` / `role_account`, `sync_state.snapshot_slot` |

The stream cannot own contiguity: carbon's Yellowstone datasource re-subscribes internally on
error and swallows auth/plan rejections in a retry loop, so a process cannot observe that it
missed a window; and on an idle program "no updates" is the normal case, so silence proves
nothing. Task 3's update-driven `SessionMarker` was therefore removed; `grpc_reconnects_total`
and the reconnect loop's `gap_opened()` remain as a belt-and-braces freeze.

### New schema

| File | Contents |
| --- | --- |
| `0006_backfill_cursor.sql` | `backfill_cursor` — singleton resume cursor (oldest fully-committed signature + slot). Deleted when a walk finishes, so "a cursor exists" means "an interrupted walk is waiting to be resumed". |

### New env var

`RECONCILE_INTERVAL` (seconds, default 300) — how often the reconciliation supervisor re-walks
the tip. Cost at the default: ~576 RPC requests/day (~1.4 M Alchemy CU/month, under 2 % of the
free tier).

### Findings

- **carbon's `Pipeline::run()` does NOT drain on datasource cancellation.** Its
  `ShutdownStrategy::ProcessPending` only covers carbon's own SIGINT branch; the
  `datasource_cancellation_token` branch `break`s immediately, dropping whatever is still queued
  in the pipeline channel. A crawl window therefore cancels the *crawler* (via the `Observed`
  wrapper's own token) and lets the channel close naturally, which is the path that does drain.
- **The crawler still never terminates**, so a window is bounded by our own
  `getSignaturesForAddress` page: `before` = resume cursor, `until` = the first signature below
  the page. A window is complete when every successful signature it enumerated has been
  delivered — evidence, not a timeout. (Task 3's idle-watchdog `replay` subcommand is gone,
  superseded by `backfill`.)
- **A snapshot re-run legitimately updates rows.** It is tagged with a fresh `getSlot`, so the
  slot guard admits it; every non-slot column is unchanged (verified by digest). Instruction and
  action rows never change on a re-run.
- The **deploy transaction** (slot 483386556, the `backfill_floor_slot`) is in
  `getSignaturesForAddress` but invokes the BPF loader, not the program, so it correctly
  produces no rows: 12 chain signatures ↔ 11 indexed.

## Phase 5 — GraphQL API (2026-08-16)

`crates/api` is now a real Axum + Juniper server on port 3010 (`GRAPHQL_PORT`): the old
SubQuery-shaped `schema.graphql` surface over Task 2's parity views, GraphiQL, `/health`, and
the spec's mandatory DoS guards. Full report: `.superpowers/sdd/carbon-migration-spec/task-5-report.md`.

### Schema surface

Same field names/case as the old `schema.graphql`, with only the slot renames ruling R8
mandates (`updatedAtBlock` → `updatedAtSlot`, `blockHeight` → `slot`, etc.). `Config`,
`Admin`/`AdminConnection`, `RoleAssignment`/`RoleAssignmentConnection`,
`WhitelistAction`/`WhitelistActionConnection` back onto `config_view` / `admins_view` /
`role_assignments_view` / `whitelist_actions` respectively. `checkAccess` (ruling R17) and
`syncStatus` (replacing the old `_metadata`) are new, ported from the dropped grpc-api and the
new `sync_state` table. BYTEA pubkeys (only `config_view.authority`/`pending_authority` — every
other view already stores base58 text, inherited from `whitelist_actions`) are base58-encoded
in the resolver.

### Reused from `carbon_core::graphql`

`carbon_core::graphql::server::build_schema` (generic over any query root/context, so it works
with this crate's own `QueryRoot` — the generated decoder's own GraphQL surface, ruling R10, is
never referenced) and `carbon_core::graphql::primitives::I64` (a string-serialized big-int
scalar — juniper's built-in `Int` is `i32`-only). `graphql_router` was NOT reused: it has no
seam to run the depth/complexity guard before juniper executes, so `crates/api/src/router.rs`
builds the same route shape directly from `juniper_axum`'s primitives instead.

### The DoS guards

- `first` clamps to `[0, 100]` (default 20), `offset` to `[0, 10_000]` — silent, never an error.
- Query depth (≤8) / complexity (≤500 fields) pre-parsed with the `graphql-parser` crate
  (0.16 juniper has no built-in limiter) before anything reaches juniper. GraphiQL's real
  `IntrospectionQuery` is legitimately deeper than 8 (its `TypeRef` fragment nests `ofType`
  seven levels); rather than raise the limits, an exact allowlist recognises only an operation
  named `IntrospectionQuery` whose top-level selection is `__schema`/`__type` — a disguised deep
  data query under the same name is still measured and rejected.
- Dedicated read pool (`crates/api/src/db.rs`) with `SET statement_timeout = '5s'` via
  `after_connect`, separate from the indexer's write pool.
- `graphql_requests_total`, `graphql_request_duration_seconds`, `graphql_rejected_total{reason}`
  on `METRICS_ADDR` (default `0.0.0.0:9465` — deliberately different from the indexer's `9464`).

### Not extracted into a shared module

Both `crates/indexer`'s `config`/`metrics` modules stayed indexer-only; `crates/api` duplicates
the ~20 lines of overlap (RPC endpoint selection, URL redaction, the Prometheus exporter
`install(addr)` shape) rather than depending on the `indexer` crate as a library. Depending on
it would drag its whole non-GraphQL dependency graph (Yellowstone gRPC, `carbon-yellowstone-grpc-datasource`,
`clap`, the Windows `protoc` workaround) into this binary's build for no functional benefit.
`crates/indexer` itself was not modified.

## Phase 6 — Observability: Prometheus + Grafana (2026-08-16)

`monitoring/` now targets the Carbon binaries instead of the old SubQuery node. Full report:
`.superpowers/sdd/carbon-migration-spec/task-6-report.md`.

### Scrape config

`monitoring/prometheus.yml`'s single `subquery-node` job (port 3000) is replaced by two jobs at
the ports Tasks 3/5 actually bind: `indexer` → `indexer:9464`, `api` → `api:9465`, both 10s
intervals. A `rule_files` entry points at the new `monitoring/alerts.yml`. The old job's comment
now explains it belongs to the SubQuery rollback stack (git history has the file).

### Alert rules (new: `monitoring/alerts.yml`)

Ruling R19: rules only, no Alertmanager — notification routing is explicitly out of scope.
`SlotLagHigh` (`chain_tip_slot - last_contiguous_slot > 3000` for 5m), `DecodeFailures`
(`updates_failed` or `decode_skipped_total` increased in 10m), `IndexerDown`/`ApiDown`
(`up == 0` for 2m per job), `ReconnectStorm` (`grpc_reconnects_total` up more than 5 in 15m).

**Correction vs. the brief**: `BackfillStalled` cannot be `backfill_complete == 0 and
increase(backfill_last_processed_slot[15m]) == 0` as suggested — there is no `backfill_complete`
Prometheus metric (checked against task-4-report.md's metrics table and
`crates/indexer/src/metrics.rs`; `backfill_complete` is a `sync_state` DB column, surfaced only
via GraphQL/`​/health`). Derived instead from what exists: `(chain_tip_slot -
last_contiguous_slot > 3000) and changes(backfill_last_processed_slot[15m]) == 0` for 5m —
`changes()` rather than `increase()` because the gauge falls, it doesn't count up. Caveat noted
in the rule's own comment: since the gauge freezes forever once a backfill completes, this can
also fire if the *reconciler* stalls well after backfill finished; `syncStatus`/`/health`'s
`backfillComplete` field disambiguates the two cases.

### Dashboard (`monitoring/grafana/dashboards/indexer-health.json`)

Same `uid`/filename (`indexer-health`), same pinned `prometheus` datasource uid, rebuilt for the
13 carbon metrics across all required panel groups: slot lag (top-left, biggest, green
<300/yellow <3000/red >=3000 — plus a companion chain-tip-vs-contiguous timeseries), updates/sec
by pipe (`transaction_updates_processed`/`account_updates_processed`/
`account_deletions_processed` — no `carbon_` prefix, no `_total` suffix, verified against
carbon-core 0.12.0's `pipeline.rs` source directly), decode failures (`updates_failed` +
`decode_skipped_total` by reason), DB flush latency p50/p95/p99 (`histogram_quantile` over
`db_flush_duration_seconds`) and batch size (`db_flush_rows`), stream health (`grpc_reconnects_total`
rate + `up{job="indexer"}`, with the "understates brief blips" caveat in the panel description),
backfill progress (`backfill_last_processed_slot` with a threshold line at the floor 483386556,
`snapshot_accounts_loaded`), and GraphQL (request rate, p95 latency, `graphql_rejected_total` by
reason).

### Verification

`promtool check config`/`check rules` clean (prom/prometheus:v3.13.2, Docker). Dashboard
imported cleanly into a throwaway grafana:13.1.3 (all 13 panels, `provisionedExternalId:
indexer-health.json`, no errors); the datasource proxy could reach a throwaway Prometheus on the
same network. Live-data smoke: `indexer run` + `api` against Task 5's replayed devnet database
(`carbon_task5`), scraped by a scratchpad Prometheus pointed at `host.docker.internal:9464/9465`
— `chain_tip_slot`/`last_contiguous_slot` both read `484472667` (lag 0), `graphql_requests_total`
incremented across four live GraphQL queries, all six alert rules evaluated `health: ok` and
`inactive`. Harness torn down; committed configs re-checked to contain `indexer:9464`/`api:9465`,
not the harness targets.
