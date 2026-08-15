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
