# realXmarket Whitelist Indexer

Indexes the [`xcavate-whitelist`](https://github.com/XcavateBlockchain/realxmarket-solana)
Solana program — the roles and compliance registry of the realXmarket protocol — on **Solana
devnet**, and serves it over GraphQL. Built on [Carbon](https://github.com/sevenlabs-hq/carbon)
(Rust), reading a live Yellowstone gRPC stream from Alchemy plus RPC-driven backfill/snapshot
paths for completeness. See [ARCHITECTURE.md](ARCHITECTURE.md) for the pipeline, schema, and
where the old SubQuery-based indexer's logic ended up; [DECISIONS.md](DECISIONS.md) for why
things are built this way; [RUNBOOK.md](RUNBOOK.md) for operating it.

| Program | Address (devnet) |
|---|---|
| xcavate_whitelist | `2vVARM46pPD4rcHdbXHnYA4vTGN14q6skQAzsQWcHUxn` |

Indexing starts at slot **483,386,556** (program deployment), so the dataset covers the
program's complete history. Only the whitelist program is indexed for now — three sibling
IDLs (`marketplace`, `property`, `regions`) sit in [`idls/`](idls/) unindexed; the layout
leaves room to add one later (see [DECISIONS.md ADR-19](DECISIONS.md#adr-19-whitelist-only-scope)).

## Quickstart (Docker)

```bash
git clone <this repo> && cd Indexer
cp .env.example .env          # fill in ALCHEMY_API_KEY + POSTGRES_PASSWORD
docker compose up --build
```

- GraphQL + GraphiQL: <http://localhost:3010/graphiql>
- Grafana: <http://localhost:3011> (user `admin`, password `GRAFANA_PASSWORD` — defaults to
  `admin` locally)
- Prometheus: bound to loopback only — `ssh -L 9090:localhost:9090 <host>` in production, or
  just `http://localhost:9090` locally

A fresh stack snapshots current state, backfills the program's full history, and starts
tailing the live stream — all within seconds at this program's data volume (see
`MIGRATION_LOG.md`'s Phase 7 verification for the timed run). `curl localhost:3010/health`
reports `backfill_complete: true` once caught up.

## Quickstart (bare cargo)

For iterating on the Rust code without rebuilding Docker images each time. Needs Rust
(workspace pins 1.88.0; published-crate MSRV 1.82) and a Postgres instance.

```bash
# disposable test Postgres
docker run -d --name carbon-mig-test-pg -e POSTGRES_PASSWORD=test -p 54329:5432 postgres:16

# offline: builds against the committed .sqlx query caches, not a live DB (both crates'
# caches are checked in under crates/{indexer,api}/.sqlx/) -- avoids needing migrations
# applied before the code even compiles.
SQLX_OFFLINE=true cargo build --workspace

export DATABASE_URL=postgres://postgres:test@localhost:54329/postgres
set -a; . ./.env; set +a       # ALCHEMY_API_KEY, etc — never printed
./target/debug/indexer run &   # applies migrations itself, then live stream +
                                # startup snapshot/backfill + reconciliation
./target/debug/api             # GraphQL on :3010
```

`indexer`/`api` read configuration entirely from the environment (no dotenv loading inside
the binary itself — see "Environment variables" below); `set -a; . ./.env; set +a` exports
the repo's `.env` into the shell before running either. `indexer run` (and `snapshot`/
`backfill`) apply pending `migrations/` automatically at startup (`sqlx::migrate!()`,
idempotent) — no separate migration step needed. On Windows, building `crates/indexer`
needs the MSVC linker fix and (for the Yellowstone gRPC dependency's vendored protobuf build)
a `protoc` override — see `.superpowers/sdd/carbon-migration-spec/env-notes.md` and
`task-3-report.md`'s "Environment" section; neither applies on Linux/CI.

## Environment variables

Source: [`.env.example`](.env.example), `crates/indexer/src/config.rs`,
`crates/api/src/config.rs`.

| Variable | Used by | Default | Purpose |
|---|---|---|---|
| `ALCHEMY_API_KEY` | indexer, api | *(required)* | Alchemy key with Solana Devnet enabled — the Yellowstone gRPC `X-Token` (indexer) and the JSON-RPC primary endpoint (both binaries' `getSlot`/`getBlockTime`/backfill calls). Both binaries refuse to start without it. |
| `POSTGRES_PASSWORD` | postgres, indexer, api | *(required)* | Stack-internal Postgres password. Applied only on first init of the `pgdata` volume — see [docs/deployment.md](docs/deployment.md#rotating-postgres_password). `DATABASE_URL` is composed from this inside `docker-compose.yml`, never a separate secret. |
| `RUST_LOG` | indexer, api | `info,hyper=warn,h2=warn,tonic=warn,rustls=warn` (indexer) / `info` (api) | `env_logger` filter syntax. |
| `GRAPHQL_PORT` | api (compose) | `3010` | Published host port for GraphQL + GraphiQL. |
| `CORS_ALLOWED_ORIGINS` | api | *(unset → allow all)* | Comma-separated list of origins allowed to call `/graphql` from a browser (CORS), e.g. `https://app.example.com,http://localhost:3000`. Unset, empty, or `*` allows every origin. |
| `GRAFANA_PORT` | grafana (compose) | `3011` | Published host port for Grafana. |
| `PROMETHEUS_PORT` | prometheus (compose) | `9090` | Host port Prometheus binds, **loopback only** (`127.0.0.1`) — it has no auth. Reach it over an SSH tunnel in production. |
| `GRAFANA_PASSWORD` | grafana (compose) | `admin` | Grafana admin password. Production deploy fails fast if unset. |
| `PG_IMAGE` / `INDEXER_IMAGE` / `API_IMAGE` | compose | *(unset → build locally)* | Pinned GHCR image tags; written by the deploy workflow. |
| `ALCHEMY_GRPC_URL` | indexer | `https://solana-devnet.g.alchemy.com` | Yellowstone gRPC host. |
| `ALCHEMY_RPC_URL` | indexer, api | `https://solana-devnet.g.alchemy.com/v2/$ALCHEMY_API_KEY` | JSON-RPC primary endpoint override. |
| `RPC_FALLBACK_URL` | indexer, api | `https://api.devnet.solana.com` | Public devnet RPC, used when the primary errors (throttling, plan limits). |
| `PROGRAM_ID` | indexer | `2vVARM46pPD4rcHdbXHnYA4vTGN14q6skQAzsQWcHUxn` | The whitelist program. |
| `BACKFILL_START_SLOT` | indexer | `483386556` | The program's deploy slot — the backfill floor. |
| `METRICS_ADDR` | indexer, api | `0.0.0.0:9464` (indexer) / `0.0.0.0:9465` (api) | Prometheus `/metrics` bind address — deliberately different per binary so they never collide on one host. |
| `RECONCILE_INTERVAL` | indexer | `300` (seconds) | How often the reconciliation supervisor re-walks the tip. See [ARCHITECTURE.md §5](ARCHITECTURE.md#5-contiguity-crawler-driven-reconciliation). |
| `DATABASE_URL` | indexer, api | — | **Not** a `.env.example` variable in the Docker path — `docker-compose.yml` composes it from `POSTGRES_PASSWORD`. Set it directly for the bare-cargo path. |

Every indexer/api variable beyond `ALCHEMY_API_KEY`/`DATABASE_URL` has a working default
baked into the binary; override only for a non-default deployment (`.env.example`'s
"Advanced" section shows the commented-out block `docker-compose.yml` passes through).

## Port map

| Port | Service | Exposure |
|---|---|---|
| 3010 | GraphQL + GraphiQL (`api`) | published (`GRAPHQL_PORT`) |
| 3011 | Grafana | published (`GRAFANA_PORT`) |
| 9090 | Prometheus | loopback only (`127.0.0.1:${PROMETHEUS_PORT}`) — SSH tunnel in production, see [RUNBOOK.md](RUNBOOK.md) |
| 9464 | `indexer` `/metrics` | internal (compose network only) |
| 9465 | `api` `/metrics` | internal (compose network only) |

`indexer` publishes nothing to the host — it has no HTTP surface a client needs, only
`/metrics` for Prometheus inside the compose network.

## Running a backfill by hand

`indexer run` already does this on startup (snapshot once, then backfill once, then hands
off to the reconciliation supervisor — see [ARCHITECTURE.md §4](ARCHITECTURE.md#4-backfill-ordering-stream-first--snapshot--history-walk)),
but both steps are also standalone subcommands, safe to re-run against a live production
`DATABASE_URL` at any time — every write on both paths is idempotent:

```bash
./target/debug/indexer snapshot                 # one-shot getProgramAccounts -> account state
./target/debug/indexer backfill                 # resumable history walk down to the floor
./target/debug/indexer backfill --floor <slot>   # stop early (see the caveat below)
```

`indexer backfill --floor` walks down to an operator-supplied slot instead of the program's
real deploy slot. It only marks `sync_state.backfill_complete = true` (which is what lets the
reconciliation supervisor start advancing `last_contiguous_slot`) if that floor is at or
below `sync_state.backfill_floor_slot` — i.e. if the walk actually reached genuine history
completeness. A higher `--floor` walks a partial range, logs a warning, and claims nothing;
the resume cursor is left in place so a later unrestricted `indexer backfill` picks up where
the partial walk stopped, rather than restarting from the tip. This guard exists because an
earlier version of this indexer could be tricked into unfreezing the reconciliation
supervisor over history it never actually walked — see `task-4-report.md`'s "Fix round 1" for
the full incident.

The reconciliation supervisor (part of `indexer run`, not a separate subcommand) is what
*keeps* `last_contiguous_slot` current after the initial backfill completes — it re-walks a
small window above the frontier every `RECONCILE_INTERVAL` seconds. See
[ARCHITECTURE.md §5](ARCHITECTURE.md#5-contiguity-crawler-driven-reconciliation) for why this
exists instead of trusting the live stream alone.

## Regenerating the decoder after an IDL change

`crates/whitelist-decoder` is **generated — never hand-edit it**. If `idls/xcavate_whitelist.json`
changes (a program upgrade, a new instruction), regenerate with the exact command Task 1
verified:

```bash
npx @sevenlabs-hq/carbon-cli@latest parse \
  -i ./idls/xcavate_whitelist.json \
  -o ./crates/whitelist-decoder \
  -s anchor \
  -c \
  --with-postgres true \
  --with-graphql true \
  --with-serde true
```

**0.12.0 pin caveat**: the installed CLI generates code against `carbon-core = "0.12.0"`, and
every other `carbon-*` dependency in this workspace (the Yellowstone datasource, the
transaction crawler, metrics) is pinned to match — see
[DECISIONS.md ADR-12](DECISIONS.md#adr-12-carbon-stack-pinned-at-0120). Regenerating with a
newer `carbon-cli` that targets a different `carbon-core` version means bumping every other
pin in the same commit, never partially. The generated crate's own `postgres`/`graphql`
feature artifacts (migrations, GraphQL resolvers) are **not** used by this repo's storage or
API layers — see [DECISIONS.md ADR-9](DECISIONS.md#adr-9-carbons-built-in-graphql-juniperaxum-over-postgraphilehasura)
and the ruling R10 note in `MIGRATION_LOG.md` — only the typed instruction/account
decoders are consumed.

## Sibling IDLs

`idls/marketplace.json`, `idls/property.json`, `idls/regions.json` are checked in but **not
indexed** — user decision, whitelist-only scope for this migration (see
[DECISIONS.md ADR-19](DECISIONS.md#adr-19-whitelist-only-scope)). Their addresses are already
in [`addresses.json`](addresses.json). Adding one later follows the same shape as the existing
program: a new decoder crate (generate the same way as above), its own migrations, and a
second `.instruction()`/`.account()` pair wired into `crates/indexer/src/pipeline.rs`.

## Repository layout

```
crates/whitelist-decoder/   generated Carbon decoder (never hand-edited)
crates/indexer/             the pipeline binary: run / backfill / snapshot / smoke-grpc
crates/api/                 the GraphQL API binary (Axum + Juniper), :3010
migrations/                 sqlx migrations (0001..0006), applied in filename order
idls/                       xcavate_whitelist.json (indexed) + 3 unindexed siblings
monitoring/                 Prometheus scrape config + alert rules + Grafana provisioning
docker/                     rust.Dockerfile (indexer+api), pg-Dockerfile, node.Dockerfile (rollback)
docker-compose.yml          the active stack: postgres, indexer, api, prometheus, grafana
docker-compose.subquery.yml the disabled SubQuery rollback stack
.github/workflows/          ci.yml, deploy.yml
grpc-api/                   old gRPC read API — unwired, rollback-only
docs/deployment.md          Hetzner ops runbook (server setup, secrets, deploy mechanics)
ARCHITECTURE.md, DECISIONS.md, RUNBOOK.md, MIGRATION_LOG.md   this migration's documentation
```

## Monitoring

Prometheus scrapes `indexer:9464` and `api:9465` every 10s; Grafana's **Indexer health**
dashboard (provisioned from [`monitoring/`](monitoring/) on every start) and six alerting
rules ([`monitoring/alerts.yml`](monitoring/alerts.yml), rules only — no Alertmanager, see
[DECISIONS.md ADR-20](DECISIONS.md#adr-20-alerts-as-prometheus-rules-only-no-alertmanager))
read it. See [RUNBOOK.md](RUNBOOK.md) for what each alert means and how to read the
dashboard when the indexer looks behind.

## Deployment

Pushing to `main` builds and pushes GHCR images, then deploys to a Hetzner server over SSH —
see [docs/deployment.md](docs/deployment.md) for server setup, secrets, and the deploy
mechanics, and [RUNBOOK.md](RUNBOOK.md) for operating a running deployment (including how to
roll back to the old SubQuery stack).
