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

## Phase 7 — Docker Compose (2026-08-16)

The new `docker-compose.yml` replaces the SubQuery-node/graphql-engine/grpc-api trio with the two
Carbon/Rust binaries (`indexer`, `api`) built by a new `docker/rust.Dockerfile`. The old stack
moves unchanged (except a top-of-file "disabled rollback path" comment) to
`docker-compose.subquery.yml`. Full report: `.superpowers/sdd/carbon-migration-spec/task-7-report.md`.

### Compose topology

Five services: `postgres` (unchanged `docker/pg-Dockerfile`, ruling R16), `indexer` and `api`
(both built from `docker/rust.Dockerfile` via a `build.target`), `prometheus`, `grafana`.
`DATABASE_URL` is composed inside the compose file from `POSTGRES_PASSWORD` (ruling R2), never a
separate secret. Only `api` (`GRAPHQL_PORT`, default 3010) and `grafana` (`GRAFANA_PORT`, default
3011) publish to the host; `prometheus` stays loopback-bound (`127.0.0.1:${PROMETHEUS_PORT:-9090}`,
unchanged from the old stack); `indexer` publishes nothing. Healthchecks on every service:
postgres `pg_isready`, indexer `curl http://localhost:9464/metrics`, api `curl
http://localhost:3010/health`, prometheus `wget --spider http://localhost:9090/-/ready` (its
image is busybox-based, no curl), grafana `curl http://localhost:3000/api/health`. `indexer`/`api`
`depends_on: postgres: condition: service_healthy`; `prometheus` has no `depends_on` (kept from
the old stack's own comment: monitoring must stay up when the monitored thing is down); `grafana`
`depends_on: prometheus: condition: service_started`. `pgdata` keeps its old-stack name (same
postgres instance, same volume — the SubQuery `app` schema coexists, which is the rollback path);
`promdata`/`grafanadata` also carried over unchanged. `monitoring/prometheus.yml` and
`monitoring/alerts.yml` are both mounted read-only (the latter at exactly
`/etc/prometheus/alerts.yml`, which Task 6's `rule_files` entry expects); the Grafana provisioning
and dashboard mounts needed no changes — Task 6 already verified they line up.

### `docker/rust.Dockerfile`: cargo-chef + the protoc decision

One Dockerfile, `chef`/`planner`/`builder` stages shared, then two final `FROM runtime-base`
targets (`indexer`, `api`) selected via `build.target` in compose. `crates/whitelist-decoder`
carries its own `[workspace]` table (Task 1), so it is a path dependency outside the main
workspace that `cargo chef prepare`'s recipe cannot skeletonize; `cargo chef cook` needs its real
files present, so the Dockerfile `COPY`s just that crate ahead of the cook step (found by a build
failure, not anticipated — see the report). BuildKit cache mounts on `/usr/local/cargo/registry`
and `/app/target` make the crates.io downloads and dependency compiles durable across separate
`docker build` invocations, independent of the ordinary layer cache.

**protoc**: the brief asked whether installing `protobuf-compiler` and setting `PROTOC` bypasses
`yellowstone-grpc-proto`'s slow vendored-protobuf build. Checked directly against the vendored
crate sources: `yellowstone-grpc-proto`'s build.rs unconditionally overwrites `PROTOC` with
`protobuf_src::protoc()` before use, so an env var alone does nothing. The real bypass is Cargo's
stable "Overriding Build Scripts" feature — `protobuf-src` declares `links = "protobuf-src"`, so a
`[target.<triple>.protobuf-src] rustc-env = { INSTALL_DIR = "/usr" }` in `.cargo/config.toml`
(generated in the Dockerfile from `rustc -vV`'s reported host triple) skips its build script
entirely, and Debian's `protobuf-compiler` + `libprotobuf-dev` packages happen to install to
exactly the `/usr/bin/protoc` + `/usr/include` layout it expects. Verified working end to end by
the real build below — the vendored autotools compile never runs.

### Exit verification

From a clean state (`docker compose down -v`; a fresh `.env.compose-test` with throwaway local
values plus the real `ALCHEMY_API_KEY`, never committed):

- `docker compose --env-file .env.compose-test up -d --build` → all 5 services reached healthy.
  Cold dependency build (`cargo chef cook` + `cargo build --release` for both binaries, base image
  already local): 5m16s. Image sizes: `indexer:local` 112MB, `indexer-api:local` 106MB,
  `indexer-postgres:local` 294MB (unchanged, `pg-Dockerfile` untouched per ruling R16).
- Indexer logs showed the subscribe gate passing against the real Alchemy key, a `getProgramAccounts`
  snapshot (11 accounts), and a history backfill (12 signatures) completing within seconds of
  startup; `curl localhost:3010/health` showed `backfill_complete: true`; `/graphql` introspection
  and representative queries (`whitelistActions`, `syncStatus`) matched the known chain-truth
  dataset (11/11/1/2/8 rows — same as Tasks 3-5); Grafana answered on `:3011`; Prometheus (queried
  via `docker exec` from inside its own container) showed both `indexer:9464` and `api:9465`
  targets `health: "up"`.
- `docker restart indexer-indexer-1` → healthy again within 15s, clean startup log
  ("startup jobs: nothing to do"), row counts unchanged (idempotency).
- `docker compose -f docker-compose.subquery.yml config` parses.
- Torn down with `docker compose down` (volumes kept for Task 8); `.env.compose-test` deleted.

**Incidental fix**: the repo's local `.env` had a corrupted first line (`ALCHEMY_API_KEY=` had lost
its variable name, leaving a bare `=<value>`), which broke every bare `docker compose`
subcommand that auto-loads `.env` (`ps`, `config`, `ls`, `exec` all failed with a Windows
`ERROR_INVALID_PARAMETER`-shaped `setenv` error — `up`/`down`/`build` were unaffected since they
were always invoked here with an explicit `--env-file`). Restored the variable name from the
value already read earlier in this task's session; `.env` is gitignored and outside this task's
tracked-file scope, so this is noted here rather than in a diff.

## Phase 8 — CI + deployment (2026-08-16)

`.github/workflows/ci.yml` and `.github/workflows/deploy.yml` are adapted, not replaced. The
SubQuery `indexer`/`grpc-api` CI jobs and deploy's node/grpc build-push steps are commented out
with a `# SubQuery rollback path — disabled, see DECISIONS.md` note, not deleted (spec §13). SSH
auth, secret names, branch gating, the concurrency group, and `DEPLOY_DIR=/opt/indexer` are
byte-for-byte unchanged — see task-8-report.md for the full line-referenced preserved-elements
checklist. Full report: `.superpowers/sdd/carbon-migration-spec/task-8-report.md`.

### ci.yml: new `rust` job

`ubuntu-latest` with a `postgres:16` service container; `dtolnay/rust-toolchain@stable`
(clippy+rustfmt) + `Swatinem/rust-cache@v2`; the same `.cargo/config.toml` protobuf-src
build-script override `docker/rust.Dockerfile` already uses (Task 7), duplicated rather than
shared since compose/Docker builds don't read `.github/`. Gates, in order: `cargo fmt --check` →
`cargo clippy --workspace --all-targets -- -D warnings` → `SQLX_OFFLINE=true cargo build
--workspace --locked` → `sqlx migrate run` → `cargo test --workspace --locked` → `cargo sqlx
prepare --check` for **each** crate, run from inside `crates/indexer/` and `crates/api/`
respectively — `--check --workspace` from the repo root does not work here (the root `Cargo.toml`
is a virtual manifest with no package of its own; verified locally, see the report) — matching how
every prior task's own `cargo sqlx prepare` re-run command already generated the two per-crate
`.sqlx` caches. A `docker-build-smoke` job (build both Rust image targets, no push, gha-cached,
`needs: rust`) shares its cache scopes with deploy.yml's real build-and-push job.

**Network-dependent tests**: verified by inspection (not assumed) that none exist anywhere in the
workspace — every `#[test]`/`#[tokio::test]`/`#[sqlx::test]` function either is pure logic or runs
against the local test Postgres using devnet byte fixtures captured once and checked into
`integration_tests.rs` (Task 3). The RPC/gRPC clients in `crawl.rs`/`block_time.rs`/
`grpc_smoke.rs` are only reachable via CLI subcommands (`smoke-grpc`, `snapshot`, `backfill`,
`run`), none of which `cargo test` invokes. Nothing needed gating.

### deploy.yml

Build & push: the SubQuery `node`/`grpc` images are replaced by `docker/rust.Dockerfile`'s
`indexer` and `api` targets, pushed as `ghcr.io/<repo>/indexer:{latest,sha}` and
`ghcr.io/<repo>/api:{latest,sha}` with their own gha cache scopes; the `postgres` image build is
untouched. Rendered server `.env`: `NODE_IMAGE`/`GRPC_IMAGE` → `INDEXER_IMAGE`/`API_IMAGE` (kept
`PG_IMAGE`); `GRPC_PORT` dropped from the rendered ports (grpc-api is out of the active stack).
The `dotenv_secret`/`dotenv_port` helpers and their validations are untouched. Verify step: the
`subquery-node /ready` check is replaced by `api` `/health` (curl confirmed present in both new
images, `docker/rust.Dockerfile`'s runtime-base stage); the in-network prometheus `/-/ready` and
grafana `/api/health` checks now exec from `api` instead of `subquery-node` (same retry pattern).
The upload step, image-prune step, and `docker-compose.subquery.yml` are unchanged — the rollback
compose file is **not** uploaded to the server (it stays git-only; uploading it would invite an
accidental `up`).

### CI/deploy secrets and vars — grepped from the final workflows, not assumed

| Name | Kind | File(s) | Status |
| --- | --- | --- | --- |
| `GITHUB_TOKEN` | secret (built-in) | deploy.yml | pre-existing |
| `HETZNER_SSH_KEY` | secret | deploy.yml | pre-existing |
| `HETZNER_KNOWN_HOSTS` | secret | deploy.yml | pre-existing |
| `HETZNER_HOST` | secret | deploy.yml | pre-existing |
| `HETZNER_SSH_PORT` | secret | deploy.yml | pre-existing |
| `HETZNER_USER` | secret | deploy.yml | pre-existing |
| `ALCHEMY_API_KEY` | secret | deploy.yml | pre-existing |
| `POSTGRES_PASSWORD` | secret | deploy.yml | pre-existing |
| `GRAFANA_PASSWORD` | secret | deploy.yml | pre-existing |
| `GHCR_PULL_TOKEN` | secret (optional) | deploy.yml | pre-existing |
| `GRAPHQL_PORT` | repo var | deploy.yml | pre-existing |
| `GRAFANA_PORT` | repo var | deploy.yml | pre-existing |
| `PROMETHEUS_PORT` | repo var | deploy.yml | pre-existing |
| `GRPC_PORT` | repo var | — (removed from rendering) | now unused; safe to delete from repo Settings → Variables whenever convenient, does not fail the workflow if left in place |

**No new secrets or vars.** `ci.yml`'s new `rust` job has zero `secrets.`/`vars.` references — its
Postgres service uses a hardcoded throwaway password (`test`), not a secret, matching ruling R2
(no `DATABASE_URL` secret; compose composes it, and CI's own test DB needs no persistence either).

### Verification

`actionlint` (via its docker image) on both files: **0 findings on `ci.yml`**; `deploy.yml` has the
same 12 pre-existing info-level shellcheck findings (`SC2029`/`SC2086`, all inside the untouched
SSH `run:` blocks) as the original file — byte-identical count and rule set, confirmed by running
actionlint against the original committed files first as a baseline. `docker compose config` with
a synthetic rendered `.env` (`INDEXER_IMAGE`/`API_IMAGE`/`PG_IMAGE` + the three ports) resolved
every pin correctly and validated clean. The extracted `Render server .env` script, dry-run with
fake values, produced the exact expected `.env` and correctly rejected a missing secret with the
`::error::` message and exit 1 (empty output file, matching the pre-existing, unmodified failure
behavior). The full CI rust-job command sequence was reproduced locally end to end, in order, with
the exact flags: fmt, clippy, `SQLX_OFFLINE=true` build, migrate, `cargo test --workspace --locked`
(96 tests: 71 indexer + 25 api, 0 failures), and both per-crate `cargo sqlx prepare --check` —
all green.

## Migration complete (2026-08-16)

This log is not itself a deliverable (see the top of this file) — it stays in the repo as
the migration's working record. The durable documentation it fed is:
**[README.md](README.md)**, **[ARCHITECTURE.md](ARCHITECTURE.md)**,
**[DECISIONS.md](DECISIONS.md)**, **[RUNBOOK.md](RUNBOOK.md)**, and the updated
**[docs/deployment.md](docs/deployment.md)**.

### What shipped

All nine phases landed on branch `carbon-migration`:

| Phase | Commit range | What |
| --- | --- | --- |
| 0 — Recon | (notes only, no commits) | Program/network/IDL/Alchemy-gRPC/Carbon-API findings, this file |
| 1 — Decoder | `903dda2..42c63d3` | Rust workspace, generated `carbon-xcavate-whitelist-decoder` |
| 2 — Database schema | `42c63d3..fbcfb94` | 5 migrations, `crates/indexer/src/db/` |
| 3 — Pipeline | `fbcfb94..26ba5f7` | Carbon 0.12.0 pipeline, processors, batched writer |
| 4 — Backfill | `26ba5f7..519f4db` | snapshot/backfill/reconcile, incl. fix round (`c33a9d8`, `e323a0d`, `35f4c83`, `519f4db`) |
| 5 — GraphQL API | `519f4db..53967c9` | `crates/api`, DoS guards, incl. fix round (`01a604c`, `53967c9`) |
| 6 — Observability | `53967c9..f53f6ed` | Prometheus scrape/alerts, Grafana dashboard |
| 7 — Docker Compose | `f53f6ed..ffaedeb` | `docker-compose.yml`, `docker/rust.Dockerfile`, SubQuery stack preserved as the rollback file |
| 8 — CI + deployment | `ffaedeb..d7224c2` | `ci.yml`/`deploy.yml` adapted, rollback steps commented (not deleted) |
| 9 — Documentation | `d7224c2..ffd47fc` | This section, plus the four docs and the `docs/deployment.md` update above |

Every phase's review is recorded Approved/clean in the controller ledger
(`.superpowers/sdd/carbon-migration-spec/progress.md`) before the next phase started; two
phases (4, 5) went through one fix round each for reviewer-found Important/Critical findings,
both closed before that phase was marked complete.

### CI/deploy secrets accounting (from Task 8, re-confirmed here)

**No new secrets or repository variables are required.** Task 8's own accounting
(`task-8-report.md`) grepped every `secrets.`/`vars.` reference in the final
`.github/workflows/*.yml` files: the new `rust` CI job has zero secret/variable references
(its test Postgres uses a hardcoded throwaway password, not a secret); `deploy.yml`'s set is
byte-for-byte the same secrets the pre-migration workflow already required, minus one now-
unused repository variable.

| Name | Kind | Status |
| --- | --- | --- |
| `HETZNER_SSH_KEY`, `HETZNER_KNOWN_HOSTS`, `HETZNER_HOST`, `HETZNER_SSH_PORT`, `HETZNER_USER` | secrets | pre-existing, unchanged |
| `ALCHEMY_API_KEY`, `POSTGRES_PASSWORD`, `GRAFANA_PASSWORD` | secrets | pre-existing, unchanged |
| `GHCR_PULL_TOKEN` | secret (optional) | pre-existing, unchanged |
| `GRAPHQL_PORT`, `GRAFANA_PORT`, `PROMETHEUS_PORT` | repo variables | pre-existing, unchanged |
| `GRPC_PORT` | repo variable | now unused (grpc-api dropped from the active stack, `ADR-18`); safe to delete from repo Settings → Variables whenever convenient, does not fail the workflow if left in place |

If a human is setting this repo up fresh today, the secrets/variables to configure are
exactly the ones `docs/deployment.md` §2 lists — nothing added by this migration.

## Phase 10 — Sibling programs (2026-08-18)

Executes ADR-19's pre-planned extension recipe: the three sibling programs (`regions`,
`marketplace`, `property`) are now indexed alongside the whitelist by the same process. The
decisions this phase made and the deviations it needed are recorded as
[DECISIONS.md ADR-22](DECISIONS.md#adr-22-all-four-programs-indexed-supersedes-adr-19); the
per-program analysis that fed them (account field maps, `close =` constraint tables, ADR-10
event-derivability audits, on-chain source cross-checks) was done against
`XcavateBlockchain/realxmarket-solana` at the time of this phase.

### Findings worth keeping

| Finding | Detail |
| --- | --- |
| Decoder regeneration is reproducible | `carbon-cli` 0.12.0 `parse -i <idl> -o <dir> -c -s anchor --with-serde true` regenerates `crates/whitelist-decoder` byte-identically — the same invocation generated the three sibling crates, so all four are provably untouched generator output. `cargo fmt --all` DOES reformat the excluded decoder crates (plain `cargo fmt`, which CI runs, does not) — never run `--all`. |
| Sibling deploy slots re-verified on-chain | regions 483386626, marketplace 483386726, property 483386809 (oldest `getSignaturesForAddress` entries; whitelist re-derived as 483386556, matching Phase 0). One signature per program is its deploy transaction, which touches the program id without invoking it — enumerated by the crawl, decoded by nothing, correctly producing no rows. |
| `account_required` AND-semantics trap avoided | multi-program transaction filtering needs one keyed filter entry per program; appending ids to one entry's `account_required` list would match only transactions touching ALL of them. |
| Close positions are per-instruction facts | the same account type closes at different account-list indices in different instructions (regions `RegionState` @3 vs @4; marketplace `InvestorPosition` @4 vs @5), two marketplace instructions close two PDAs each, marketplace `close_case` closes nothing despite its name, and property `remove_letting_agent` is the protocol's one *conditional* close (runtime `close()`, only when the removed location was the agent's last) — decided in the batcher's SQL against the stored row, since the mapper is pure. |
| Sibling events are log-`emit!`, like the whitelist | no `emit_cpi!` anywhere in the protocol; the ADR-10 derivability audit per program is summarized in ADR-22 (a handful of event payloads — clock deadlines, mint-supply bonds, election outcomes, payout splits — are not derivable at instruction-mapping time; those persisted into tracked accounts are still captured by the account stream). |
| u16 -> INT, not SMALLINT | u16 max (65,535) exceeds SMALLINT; on-chain bps validation (<= 10,000) is real today but deliberately not relied on for column widths. |
| Migration 0007 adopts production state in-place | the `sync_state`/`backfill_cursor` singletons become the whitelist's per-program rows and every pre-existing `program_instructions` row is backfilled with the whitelist's `program_id` — correct by construction, because the whitelist was the only thing the indexer could previously write. |

### Verification

- `cargo fmt --check`, `clippy --workspace --all-targets` (zero warnings),
  `SQLX_OFFLINE=true cargo build --workspace --locked`, 133 tests across both crates against
  a live migrated Postgres, per-crate `cargo sqlx prepare --check` (both `.sqlx` caches
  regenerated), and the `docker/rust.Dockerfile` indexer target built clean.
- End-to-end against live devnet (public RPC, fresh database): `indexer snapshot` decoded
  and wrote every account of all four programs (11 whitelist / 5 regions / 3 marketplace /
  1 property, zero undecodable); `indexer backfill` walked all four histories to
  `HistoryExhausted` (12/8/4/2 signatures, zero mapping failures); the GraphQL API served
  the new surfaces with values cross-checked against `addresses.json` ground truth (the
  region PDA, operator owner, both postcodes, both lawyer wallets, the accepted payment
  mints) and the legacy whitelist surface (`admins`, `roleAssignments`, `checkAccess`)
  unchanged.

## Agentic maintenance enablement — 2026-08-22

The post-migration extension that turns "keep the indexer in lockstep with the programs"
from an operator procedure into an automated, PR-gated maintenance loop (ADR-23/24/25;
design in `docs/agentic-maintenance.md`; procedures in `agent/skills/`, entry point
`AGENTS.md`; tooling in `scripts/agent/`).

### What was built

- **Upgrade detection in-pipeline** (ADR-24): migration `0011_program_upgrades.sql`
  (version-boundary table seeded with deploy slots + a nullable
  `program_instructions.decoder_version` column), a hand-written BPFLoaderUpgradeable
  `Upgrade` decoder + recorder pipe (`crates/indexer/src/upgrades.rs`) on
  `common_pipes` (rides live stream and all crawls), detection side effects at most once
  per boundary strictly after commit (`batcher.rs`), the
  `program_upgrades_detected_total{program}` counter + `ProgramUpgradeDetected` alert, a
  `programUpgrades` GraphQL query, and a startup warning for boundaries newer than the
  binary. `addresses.json` gained a `deploy_slots` block, pinned to the registry by test.
- **Versioned decoding designed, dormant** (ADR-25): mapper-level slot routing on the
  recorded boundary, frozen `-vN` decoder crates (`freeze-decoder-version.sh`),
  newest-first snapshot fallback, restart-based activation healed by an idempotent
  backfill re-walk. Nothing routes today — the chain still runs version 1 of everything.
- **Additive-only migration policy, mechanized**: `scripts/lint-migrations.sh` + the
  `migration-lint` CI job (immutable history, strictly-increasing numbering, destructive
  SQL only under an in-file `-- lint: allow` argument).
- **Agent tooling** (`scripts/agent/`): upstream watcher (settled-HEAD poll),
  Anchor-version-checked IDL build + address normalization + structural diff classifier
  (identical/additive/breaking), on-chain ProgramData probe, decoder purity check, and a
  full devnet rebuild-verify loop against the public RPC.

### Findings

| Finding | Detail |
| --- | --- |
| Upstream main vs deployed chain already diverge | on 2026-08-22 the chain runs version 1 of all four programs (probe: all last-deploy slots equal the recon deploy slots) while upstream `main@5927362` is IDENTICAL for regions/whitelist and **BREAKING** for marketplace/property (secondary market, offers, income, governance) — the versioned-decoder procedure's first real customer is already queued. |
| Anchor naming drift can masquerade as breakage | anchor-lang 1.1.2 namespaces ambiguous IDL type names (`Config` → `marketplace::state::Config`); the diff classifier reports it, and `upstream-sync/SKILL.md` instructs judging layouts/discriminators, not labels. |
| Upstream builds need a version-matched Anchor CLI | upstream pins no CLI; an 0.29-era CLI silently emits old-format IDLs. `build-upstream-idls.sh` hard-checks the CLI against upstream's `Cargo.lock` (needs 1.1.x today). |
| Upgrade transactions already flow through both data paths | `account_required`/`getSignaturesForAddress` match the program account an `Upgrade` references, so detection needed a decoder, not a subscription — and a full backfill re-walk rebuilds the complete upgrade timeline from nothing. |
| README's regen command was un-pinned | `carbon-cli@latest` could silently target a newer carbon-core (ADR-12 lockstep); now pinned to `@0.12.0` in README and `verify-decoder-purity.sh`. |

### Verification

- `cargo fmt --check`, `clippy --workspace --all-targets` (zero warnings),
  `SQLX_OFFLINE=true cargo build --workspace --locked`, full workspace test suite (113
  indexer tests incl. 9 new upgrade-recorder/timeline tests) against a live migrated
  Postgres, per-crate `cargo sqlx prepare --check` with regenerated caches.
- `scripts/agent/verify-devnet.sh` end-to-end against live devnet (public RPC, fresh
  database): 4 programs, 29 instructions, 4 snapshots, zero undecodable, 4 seeded
  version boundaries, 0 chain upgrades — VERIFY OK.
- `scripts/agent/check-program-upgrades.py` live: all four programs unchanged on-chain.
- `scripts/agent/build-upstream-idls.sh` live with anchor-cli 1.1.2: built and classified
  upstream `main@5927362` (exit 20, see Findings).
- `scripts/lint-migrations.sh` exercised on clean, destructive, marker-exempted and
  misnumbered fixtures.

## Protocol redeploy at new addresses (ADR-26) — 2026-08-25

The owner redeployed all four programs to new devnet addresses (bytecode = upstream
`main@5927362`, the breaking state §7 of docs/agentic-maintenance.md had been watching) and
declared the old deployments abandoned. Clean swap per ADR-26 — no versioned decoders, the
production database is rebuilt from empty (RUNBOOK "Devnet ledger reset").

Recon (devnet RPC, 2026-08-25; deploy slot = oldest `getSignaturesForAddress` entry — this
table supersedes §3.4's for the live deployment):

| Program | Address | Deploy slot | Signatures at recon |
|---|---|---|---|
| xcavate_whitelist | `7TrzjKpdrEhnfhxuw8tWdH1sjxadazscsG5HXCDPLmaY` | 487427394 | 13 |
| regions | `5iupkzVtWxee48UXh3s615V9sXXuYjsSr61VPuduXdPc` | 487427494 | 8 |
| marketplace | `dj9Q3CpHvDHwexCbkgJ5APDx4JsTxPssNebkvP15g1T` | 487427626 | 8 |
| property | `deCp9srk9C6P4BXJaFpjR5H6Jsm6DCq8AL2kk338dVq` | 487427732 | 2 |

### What was built

* **Address swap (ADR-19/26)**: `addresses.json` (programs, deploy_slots, configs, region),
  `crates/indexer/src/programs.rs` (slots + boundary stamps + table rosters),
  `crates/api/src/graphql/enums.rs` program-id mirror, README table, Grafana floor
  threshold.
* **Decoders regenerated (ADR-12)**: all four crates from the new IDLs with carbon-cli
  0.12.0, generated into clean dirs and swapped wholesale (the in-place generator leaves
  stale files — property's old `Config` account was renamed `property::state::Config`
  upstream, generating `PropertyStateConfig` and orphaning `config.rs`). `carbon-core` pin
  unchanged at 0.12.0; `verify-decoder-purity.sh` pristine for all four.
* **Schema (migration 0012)**: seven new state tables — `marketplace_share_listing`,
  `marketplace_offer`, `property_proposal`, `property_challenge`, `property_gov_vote`,
  `property_income`, `property_income_checkpoint` — plus reshapes: marketplace config
  (+`next_share_listing_id`), listing (+`collected_fee_quote`), property_asset
  (`core_asset` → `name`/`metadata_uri`), share_holding (`locked_amount` → four
  per-`LockReason` columns + `listed`), property config (+7 governance params),
  property_letting (+6 flattened `GovState` columns). `DROP COLUMN` under lint-allow;
  correctness argument in the migration header.
* **Mapping**: 9 new marketplace + 12 new property instruction arms; new close table rows
  read from the pinned source — `accept_offer` (Offer@10 + CONDITIONAL ShareListing@6),
  `reject_offer` (Offer@4), `cancel_offer` (Offer@2), `buy_relisted_shares` (CONDITIONAL
  ShareListing@6), `delist_shares` (ShareListing@2), `close_share_holding`
  (ShareHolding@4); property `finalize_proposal` (Proposal@4), `finalize_challenge`
  (Challenge@5, optional `agent_entry` AFTER it), `unlock_proposal_votes` /
  `unlock_challenge_votes` (GovVote@4), `close_income_checkpoint` (IncomeCheckpoint@2).
  Two new batcher-side conditional closes (`db::marketplace::close_share_listing_if_*`),
  following the letting-agent precedent.
* **API**: entities/resolvers/QueryRoot fields for the seven new tables, `GovVoteChoice`
  enum, reshaped entity fields; `.sqlx` caches regenerated per crate.
* **Fixtures**: integration fixtures recaptured from the new whitelist deployment (same
  wallets; role fixture is again lawyer2's Lawyer role, tx
  `47o8ja6JZ3…J8nn` at slot 487427944); synthetic-fixture slots moved above the new floors.

### Findings

| Finding | Detail |
|---|---|
| `propose` closes its own PDA sometimes, and needs NO close op | The auto-approval path (`amount <= config.low_proposal`) creates and closes the Proposal inside one instruction; the post-transaction state never matches the owner-scoped filter, so no row is ever written and there is nothing to close — unlike the two-transaction same-slot tie `db::close` documents. Asserted by `propose_closes_nothing_despite_the_auto_approval_path`. |
| Two conditional runtime closes with different evidence | `buy_relisted_shares` carries the bought amount in its own args; `accept_offer` sells the OFFER's amount, an account fact — its close op carries the offer pubkey and the batcher reads the amount from the stored offer row (soft-closed in the same batch, columns still readable). |
| GovVote has no kind discriminator | Proposal votes and challenge votes are one account type behind two seed prefixes; rows/API deliberately carry no kind column — consumers join `id` against proposals/challenges. |
| u128 reaches the schema for the first time | `IncomeStream.per_share` / `CheckpointEntry.per_share` are u128; stored in JSONB as decimal STRINGS (serde_json's number type cannot carry the range). |
| ADR-10 event audit (per-upgrade check) | Of the 22 new events, three payloads are not exactly reconstructible later: `IncomeClaimed.amount`, `ChallengeFinalized.slashed`, `IncomeDistributed.per_share_gain` (exact value lost to the dust carry). Flagged in ADR-26; events stay ignored. |
| The volume wipe abandons ADR-21 | `pgdata` also holds the SubQuery rollback schema; the reset procedure now says so (RUNBOOK). The old programs the rollback stack pointed at no longer exist, so this invokes ADR-21's natural-cleanup clause. |

### Verification

* `cargo fmt` / `cargo fmt --check` clean (never `--all`); `cargo clippy --workspace
  --all-targets -- -D warnings` clean.
* `SQLX_OFFLINE=true cargo build --workspace --locked` clean; per-crate `cargo sqlx prepare
  --check` clean against the migrated 54329 Postgres.
* `cargo test --workspace --locked`: 160 passed, 0 failed (30 api + 130 indexer, including
  the new conditional-close SQL tests and close-position tests).
* `scripts/lint-migrations.sh` OK (0012's `DROP COLUMN` under its lint-allow marker).
* `scripts/agent/verify-decoder-purity.sh`: pristine ×4.
* `scripts/agent/check-program-upgrades.py`: exit 0, all four `unchanged (still the
  version-1 deploy)` at the new slots.
* `scripts/agent/verify-devnet.sh`: **VERIFY OK: 4 programs, 32 instructions, 4 snapshots,
  4 deploy boundaries, 0 chain upgrades** — full rebuild from the public devnet RPC against
  the new deployments, zero undecodable accounts.

## Off-chain property metadata fetcher (ADR-27) — 2026-08-25

The marketplace `PropertyAsset` account's `metadata_uri` points at an operator-hosted JSON
document (property detail: decomposed address, decomposed attributes, finances, image and
document URLs). ADR-26 indexed only the raw URI. This change downloads each URI and indexes
the document verbatim plus a flattened, typed projection of it (ADR-27).

### What was built

* **Schema (migration 0013)**: `marketplace_property_metadata` — a derived table keyed by
  the asset PDA `pubkey`. Deliberately NOT an account-state mirror (no slot/lamports/close
  columns, absent from the `db::close` roster and the programs.rs table rosters): fetch
  state (`metadata_uri` = last attempted URI, `fetched_at`, `attempts`, `next_attempt_at`
  backoff, `last_error`), `raw JSONB` (the document verbatim), and flattened typed columns —
  `address_*` ×7, flat `attributes` (incl. `number_of_bedrooms BIGINT`,
  `construction_date DATE`), flat `finances` (incl. `stamp_duty_paid BOOLEAN`),
  `user_pubkey`/`company_wallet_address BYTEA` (base58 → 32 B), `created_at`/`updated_at
  TIMESTAMPTZ`, `floor_plan`/`map_url`/`sales_agreement TEXT`,
  `other_documents`/`property_images JSONB` (URL-string arrays). All content columns
  nullable; lenient per-field parsing (a field of the wrong type stores NULL, the document
  still lands in `raw`; only a non-object top level counts as a fetch failure).
* **Fetcher loop** (`crates/indexer/src/metadata.rs`): background task in `run_live` (spawned
  only when `marketplace` ∈ `PROGRAMS`); one SQL work-set query per cycle (assets whose URI
  is unknown, changed, or failed with backoff elapsed), ≤50 per cycle, sequential (object
  storage is the shared bottleneck), `reqwest` with an SSRF guard (http/https only, private/
  loopback/link-local ranges rejected). Failures back off in SQL (30 s doubling, 1 h cap,
  same-URI CASE reset) and never go through the batcher. **Point-in-time snapshot
  semantics**: a successfully fetched asset re-fetches only when its `metadata_uri`
  changes — the indexed document is the point-in-time content of the on-chain transaction,
  not a live feed. Interval: `METADATA_FETCH_INTERVAL` env (default 30 s).
* **One-shot command**: `indexer fetch-metadata` — a single fetch cycle (bootstrap + ops,
  like `snapshot`/`backfill`; no API key needed, public devnet RPC only).
* **API**: GraphQL `propertyMetadata` connection (filter `assetId`, ordered
  `fetched_at DESC NULLS LAST, pubkey ASC`); nested `address`/`attributes`/`finances`
  objects resolve only when at least one field is non-null.
* **Metrics**: `property_metadata_fetched_total{result=success|failure}`,
  `property_metadata_pending` gauge.
* **Docs**: RUNBOOK section "Property metadata is missing, stale, or not updating"
  (incl. the force-re-fetch `UPDATE` and the one-shot command), `docs/deployment.md` note
  (the derived table refills itself after a reindex from scratch), `.env.example` +
  compose comment for `METADATA_FETCH_INTERVAL`.

### Findings

| Finding | Detail |
|---|---|
| Derived rows outlive their asset | `db::close` stamps the asset row, not the metadata row (the table is outside the close roster) — consistent with the snapshot semantics; consumers join on `pubkey` and see closure state on the asset table. |
| The document format is off-chain contract | The operator's JSON is not on the chain, so field-level leniency is load-bearing: a wrong-typed field must not block the document, which is why parsing is per-field and failures are per-document (only a non-object top level). |
| Wallets stored as BYTEA | The document's `userId` is a Solana wallet (identical to `companyWalletAddress` in the sample documents); stored 32 B for join parity with every other wallet column, re-encoded base58 at the API. |

### Verification

* `cargo fmt` / `cargo fmt --check` clean (never `--all`); `cargo clippy --workspace
  --all-targets -- -D warnings` clean.
* `SQLX_OFFLINE=true cargo build --workspace --locked` clean; per-crate `cargo sqlx prepare
  --check` clean against the migrated 54329 Postgres.
* `cargo test --workspace --locked`: 171 passed, 0 failed (30 api + 141 indexer, including
  the new property-metadata SQL tests -- work-set selection, the failure/backoff chain, the
  last-snapshot-kept guarantee -- and the SSRF-guard cases).
* `scripts/lint-migrations.sh` OK (0013 is additive-only).
* `scripts/agent/verify-devnet.sh`: **VERIFY OK: 4 programs, 41 instructions, 4 snapshots,
  4 deploy boundaries, 0 chain upgrades** -- full rebuild from the public devnet RPC with
  zero undecodable accounts; the new fetch step pulled every live property document
  (4 live assets with URIs, 4 fetched, 0 pending, 0 failing).

## Property-asset registration webhook (ADR-28) — 2026-08-26

When the marketplace `init_property_assets` instruction registers a new property asset
(the `PropertyAsset` PDA gets its `name`, `metadata_uri`, and share mint), the indexer
now POSTs one JSON document to a configurable `WEBHOOK_URL` — detection inside the
batched pipeline, delivery by a separate retrying background loop (ADR-28).

### What was built

* **Detection (in the pipeline)**: `mapping::marketplace` emits one `WebhookEvent` for
  `init_property_assets` only — `event_type` `property_asset_registered`, `event_id`
  `property_asset_registered:<base58 PropertyAsset PDA>` (account index 3; the PDA is
  the dedup key), payload `event` / `pubkey` / `listing_id` / `name` / `metadata_uri` /
  `share_mint` (index 4) / `slot` / `tx_signature` / `block_time` / `program`. The other
  three mappers always emit none. Carried as `WriteOp::RecordWebhookEvent` (phase 0, the
  instruction's slot) and committed atomically with the rest of the transaction's rows.
* **Schema (migration 0014)**: `webhook_events` — the durable delivery queue keyed by
  `event_id` (TEXT PK), with `event_type`, `payload JSONB`, provenance (`slot`,
  `tx_signature`, `block_time`), delivery state (`created_at`, `attempts`,
  `next_attempt_at` backoff, `last_error`, `delivered_at`), and a partial index on the
  undelivered rows. Deliberately NOT an account-state mirror (no slot guard / no soft
  close, absent from the `db::close` roster and `StateTable`) — the same carve-out as
  `marketplace_property_metadata` (ADR-27).
* **Delivery loop** (`crates/indexer/src/webhooks.rs`): background task in `run_live`
  (spawned only when `WEBHOOK_URL` is set AND `marketplace` ∈ `PROGRAMS`); one SQL
  work-set query per cycle (undelivered, backoff elapsed; `WEBHOOK_INTERVAL`, default 5
  s), ≤50 per cycle, sequential, reusing the metadata fetcher's SSRF-guarded reqwest
  client; a 2xx marks the row delivered, a failure records `last_error` (≤500 chars) +
  exponential backoff in SQL (30 s doubling, 1 h cap). AT-MOST-ONCE record (`ON
  CONFLICT (event_id) DO NOTHING` — idempotent under backfill re-walks), AT-LEAST-ONCE
  delivery (endpoints dedupe on `pubkey`). No URL → no loop, no external call, rows
  still recorded.
* **Config**: `WEBHOOK_URL` (optional; empty = disabled; never logged — an operator may
  encode a bearer token in the query string) and `WEBHOOK_INTERVAL` (seconds, default
  5, must be > 0). `WEBHOOK_URL` wired through docker-compose; `.env.example` documents
  both.
* **Metrics**: `webhooks_delivered_total{result=success|failure}` (both labels
  pre-registered at zero) and the `webhooks_pending` gauge (work-set size after the last
  cycle; deliberately not pre-registered — an absent series means the loop never ran).
  **Alert**: `WebhookDeliveryFailing` (alerts.yml, counter-derived — see the rule's
  comment for why the gauge-based form does not work with the backoff design).
* **Docs**: ADR-28 (DECISIONS.md); RUNBOOK section "Property-asset registration
  webhooks: setup and 'not firing'" (+ alert-list row); ARCHITECTURE.md pipeline note;
  README env-table rows.

### Findings

| Finding | Detail |
|---|---|
| Fresh-index backlog | A fresh database records the whole historical `init_property_assets` set during the backfill re-walk; with the webhook enabled the loop flushes it at ≤50 events per 5 s cycle in `event_id` order (base58 PDA order, NOT slot order). A one-time burst after every rebuild-from-empty; the RUNBOOK notes it. |
| `webhooks_pending` is spiky | The gauge counts the backoff-aware work set, so a failing event at the 1 h cap is in it only for the ≤5 s cycle that retries it. The alert therefore keys on the failure COUNTER over a 2 h window (≥ the 1 h backoff cap) — the derivation is documented in alerts.yml. |
| No API surface | The webhook is emit-only: no GraphQL query over `webhook_events` (operators read the table directly; the API is a read-only mirror of the chain, and this table is the indexer's own outbox, not chain state). |

### Verification

* `cargo fmt` / `cargo fmt --check` clean (never `--all`); `cargo clippy --workspace
  --all-targets -- -D warnings` clean.
* `SQLX_OFFLINE=true cargo build --workspace --locked` clean; `cargo sqlx prepare
  --check -- --lib` clean against the migrated 54329 Postgres (5 new cached queries).
* `cargo test --workspace --locked`: 179 passed, 0 failed (30 api + 149 indexer,
  including the new mapping test — `init_property_assets` emits exactly one event with
  the PDA-keyed id and full payload, every other instruction emits none — the
  `webhook_events` SQL tests — record idempotency, the 30 s → 60 s backoff chain, the
  delivered terminal state, no re-announce after delivery — and the `host_of`
  token-hiding cases).
* `scripts/lint-migrations.sh` OK (0014 is additive-only).
* `scripts/agent/verify-devnet.sh`: **VERIFY OK: 4 programs, 44 instructions, 4 snapshots,
  4 deploy boundaries, 0 chain upgrades** -- full rebuild from the public devnet RPC with
  zero undecodable accounts (migration 0014 applied cleanly from zero; the ADR-27 fetch
  step pulled all 4 live property documents).
* End-to-end record proof: an `indexer backfill` re-walk of the live devnet history into
  the :54329 dev test DB recorded exactly 4 `webhook_events` rows -- one per historical
  `init_property_assets`, `event_id` = `property_asset_registered:<asset PDA>`, all
  undelivered (no `WEBHOOK_URL` is configured there).

## Nested property metadata under `propertyAssets` (ADR-29) — 2026-08-26

ADR-27's `propertyMetadata` was only reachable as a standalone connection, so a consumer
wanting "assets, each with its document" had to run two GraphQL queries and join them
client-side on the asset PDA. This change nests the same `PropertyMetadata` type under
`PropertyAsset` (`metadata`, nullable), so asset + document answer in a single query
(ADR-29).

### What was built

* **GraphQL schema (additive)**: `PropertyAsset.metadata: PropertyMetadata` — the SAME
  type the root `propertyMetadata` connection returns (reused, not shadowed); `null`
  while the enricher has no row for the asset's PDA (fetch pending or still failing —
  the on-chain `metadataUri` stays readable throughout). The root `propertyMetadata`
  connection is unchanged: it remains the document-first access and the RUNBOOK's
  "missing/stale metadata" flows still query it.
* **Resolver** (`property_assets`, `crates/api/src/graphql/programs/marketplace.rs`): the
  asset-page query is BYTE-IDENTICAL (its sqlx cache entry is untouched); after it, one
  keyed lookup — `SELECT <the same 50 columns> FROM marketplace_property_metadata WHERE
  pubkey = ANY($1)` over the page's PDA pubkeys, skipped on an empty page — attaches each
  derived row to its asset (at most one row per node: the derived table is 1:1 on the
  asset PDA's PK). `totalCount` still counts assets only. Deliberately NOT a `LEFT JOIN`
  in the asset query: the API layer is uniformly single-table `query!` resolvers (no
  `JOIN` anywhere in `crates/api`), and the derived table is outside the mirror contract
  by design (ADR-27).
* **Shared mapping**: the 50-column row→`PropertyMetadata` decomposition (incl. the
  "nested object surfaces only when at least one field is non-null" rule) moved into ONE
  module-local `macro_rules!` (`property_metadata_from_row!`) used by BOTH metadata
  queries — `query!` binds each query site to its own private `Record` row type
  (implements no `sqlx::Row` / `FromRow`), so a plain shared function cannot name the
  row and a `try_get`-by-name helper cannot typecheck; the macro expands the
  decomposition textually against either row type, keeping each site fully
  compile-checked. The root `property_metadata` resolver's inline mapping was replaced
  by the macro (no behavior change).
* **sqlx cache**: exactly ONE new `crates/api/.sqlx` entry (the `ANY(...)` lookup); the
  asset query's cached entry is untouched (provenance preserved).
* **Docs**: ADR-29 (`DECISIONS.md`); `propertyAssets`/`PropertyAsset` doc comments note
  the nested field and its `null` semantics.

### Findings

| Finding | Detail |
|---|---|
| `metadata` is `null`, not an error | A pending/failing fetch surfaces as `metadata: null` while the on-chain `metadataUri` is readable — consumers can distinguish "not fetched yet" from "no metadata". |
| Fetch runs even if not selected | juniper resolvers here are not selection-aware; a page whose client does not select `metadata` still pays the ≤100-key PK lookup. At this data volume the wasted lookup is cheaper than the machinery (documented in ADR-29). |
| Redundant identity inside the nesting | `metadata { id assetId metadataUri }` repeat the parent's identity; deliberate — one canonical `PropertyMetadata` type serves both placements, no shadow type. |
| DoS guards unaffected | Nesting adds one field level under an existing field (well inside `MAX_DEPTH = 8`); complexity counts document fields, not result rows. |

### Verification

* `cargo fmt` / `cargo fmt --check` clean (never `--all`); `cargo clippy --workspace
  --all-targets -- -D warnings` clean.
* `SQLX_OFFLINE=true cargo build --workspace --locked` clean; per-crate `cargo sqlx prepare
  --check` clean (indexer `--lib`, api `--bin api`) — one new cached api query (the
  `ANY(...)` lookup); the asset query's cache entry is untouched.
* `cargo test --workspace --locked`: 179 passed, 0 failed (30 api + 149 indexer; no test
  changes — the API surface has no DB-backed resolver tests in this repo).
* `scripts/lint-migrations.sh`: "no migration changes vs origin/main" (this change adds
  no migrations).
* `scripts/agent/verify-devnet.sh`: **VERIFY OK: 4 programs, 51 instructions, 4
  snapshots, 4 deploy boundaries, 0 chain upgrades** -- full rebuild from the public
  devnet RPC with zero undecodable accounts; the ADR-27 fetch step pulled all 4 live
  property documents (4 fetched, 0 pending, 0 failing).
* End-to-end single-query proof: with the :54329 dev DB rebuilt from live devnet
  (`indexer snapshot` + `backfill` + `fetch-metadata`), ONE GraphQL request —
  `propertyAssets(first: 10) { nodes { assetId name metadata { propertyId propertyName
  propertyType fetchedAt address { street townCity region } finances { propertyPrice
  numberOfShares sharePrice } } } }` — answered all 4 assets with their documents
  nested (no second `propertyMetadata` query needed); a live introspection of the
  running API shows `PropertyAsset.metadata: PropertyMetadata` (nullable).
* Environment note: the host's MSVC installs (VS18/14.51 and VS2022/14.44) are missing
  their core CRT headers (`include/stdio.h` / `stddef.h` absent from both toolsets), so
  any C-dependent build (ring, zstd-sys) fails out of the box — pre-existing, unrelated
  to this change. All of the above ran with `INCLUDE`/`LIB` pointed manually at the
  intact VS2022 compiler headers + Windows SDK 10.0.22621.0; the VS install needs a
  repair (VS Installer → Modify → C++ build tools) for normal builds.

## API: listings expose propertyAsset (nested) — 2026-08-27

A listing and its property asset are two rows a consumer almost always wants together (the
listing's prices against the asset's name/metadata); previously that meant two round-trips
— `listings { ... }` then a per-asset `propertyAsset(assetId:)` — and with page size
clamped to 100 that is up to 100 follow-up point queries. This change nests the asset
under the listing so one query returns both.

### What was built

* **`Listing.propertyAsset: PropertyAsset!`** (nullable: `null` when no asset row) — a plain
  `Option<PropertyAsset>` field on the juniper `#[derive(GraphQLObject)]` struct, matching
  the crate's already-resolved-data convention (no per-field DB resolvers, no request-scoped
  cache).
* **`listings()` is now one eager query**: `SELECT l.*` + 14 `pa_*` columns via
  `LEFT JOIN marketplace_property_asset AS pa ON pa.asset_id = l.asset_id`. The join is 1:1
  (both tables are one-row-per-PDA state tables, `pa.asset_id` indexed by
  `idx_mkt_property_asset_id`), so fan-out is at most one row per listing and a page costs
  exactly this query plus the existing `count(*)`, whatever the page size.
* **`?` nullability overrides on every `pa_*` column** (sqlx
  `ColumnNullabilityOverride::Nullable`): the macro derives nullability from the source
  table's NOT NULL constraints and its EXPLAIN-based outer-join patch does not match aliased
  output columns (PG 16 also dropped the `null` flag from EXPLAIN JSON), so without the
  overrides the macro typed the joined columns non-`Option` and a listing without an asset
  row would fail to decode. The overrides come from the SQL text, so online and offline
  prepare agree.
* **`.sqlx` cache**: one query file replaced (the hash follows the query text) — old
  `query-d572fa…` dropped, new `query-1b849a…` added; no other cache churn.
* No migrations, no schema changes, no indexer changes.

### Findings

| Finding | Detail |
|---|---|
| The `LEFT JOIN` is defensive in practice | `list_property` writes the Listing and the Property accounts in one instruction, so both rows land together; `propertyAsset: null` only exists for rows that cannot arise. Kept `LEFT` (not `INNER`) so any future divergence degrades to `null` instead of dropping listings from the page. |
| No ADR | Small additive API surface; no architectural consequence. |

### Verification

* `cargo fmt` / `cargo fmt --check` clean (never `--all`); `cargo clippy --workspace
  --all-targets -- -D warnings` clean.
* `SQLX_OFFLINE=true cargo build --workspace --locked` clean (proves the `?` overrides hold
  in offline mode); `cargo sqlx prepare --check` clean against the migrated 54329 Postgres.
* `cargo test --workspace --locked`: 160 passed, 0 failed (30 api + 130 indexer).
* `scripts/lint-migrations.sh` OK (no migration changes vs `origin/main`).
* `scripts/agent/verify-devnet.sh`: **VERIFY OK: 4 programs, 51 instructions, 4 snapshots,
  4 deploy boundaries, 0 chain upgrades** — full rebuild from the public devnet RPC.
