# Agent guide

You are working on the realXmarket Solana **indexer**: Rust + Carbon 0.12.0, four devnet
programs, Postgres storage, GraphQL API. If you are the always-on maintenance agent, your
mission and its rules are `docs/agentic-maintenance.md` (ADR-23/24/25 in `DECISIONS.md`);
this file is the ground rules every agent obeys and the map of where everything is.

## Hard rules — violating any of these breaks production or provenance

1. **Never commit to or push `main`.** Every push to `main` auto-deploys to the production
   host (`.github/workflows/deploy.yml`). Work on a branch (`agent/<yyyymmdd>-<topic>`),
   open a PR, and let a human merge it.
2. **Never run `cargo fmt --all`.** It reformats the workspace-*excluded* generated decoder
   crates, destroying their byte-identical-to-generator provenance. Plain `cargo fmt` (what
   CI runs) is always safe.
3. **Never hand-edit `crates/*-decoder*`.** They are carbon-cli output, regenerated
   byte-identically from `idls/` (`scripts/agent/verify-decoder-purity.sh` proves it). The
   single sanctioned exception is the one package-rename line made by
   `scripts/agent/freeze-decoder-version.sh`.
4. **Never edit a migration that exists on `main`.** sqlx checksums applied files; editing
   one crash-loops every existing database at startup. New migrations are additive-only —
   `scripts/lint-migrations.sh` (also a CI job) is the law, and its `-- lint: allow` marker
   demands a written correctness argument.
5. **Never touch the legacy SubQuery files**: root `package.json`, `project.ts`,
   `schema.graphql`, `tsconfig.json`, `src/`, `grpc-api/`, `docker/node.Dockerfile`,
   `docker-compose.subquery.yml`. They are the inert rollback path (ADR-21). In particular,
   root `schema.graphql` is **not** the live API schema — the live schema is Rust code in
   `crates/api/src/graphql/`.
6. **The chain outranks the repo.** `idls/` must always decode what is *deployed* on
   devnet (`scripts/agent/check-program-upgrades.py` tells you), never blindly track
   upstream `main` HEAD. An **additive** IDL update may land ahead of the chain (a
   superset decoder still decodes everything deployed); a **breaking** one must go through
   the versioned-decoder mechanism (ADR-25), never an early swap.
7. **Document as you go** — this repo records everything: design decisions get an ADR
   (`DECISIONS.md`, the three-paragraph house format), ops changes get `RUNBOOK.md` +
   `docs/deployment.md` rows, and every shipped change gets a dated section in
   `MIGRATION_LOG.md`. Imitate the heavy `//!` module-doc and migration-header style you
   see everywhere; comments cite ADRs.

## Task → skill

Load the whole skill file before starting the task; each is a complete, verified procedure.

| Situation | Skill |
| --- | --- |
| Upstream `realxmarket-solana` main moved (the watcher queued a SHA) | `agent/skills/upstream-sync/SKILL.md` |
| IDL diff is **additive** → regenerate a decoder + map the new surface | `agent/skills/regen-decoders/SKILL.md` |
| IDL diff is **breaking** → freeze old decoder, add slot-routed version | `agent/skills/versioned-decoder/SKILL.md` |
| Schema work: new tables/columns/CHECK widenings/indexes | `agent/skills/write-migration/SKILL.md` |
| Any change is ready: verification gauntlet → PR → multisig → post-upgrade | `agent/skills/verify-and-ship/SKILL.md` |

Tooling for all of the above: `scripts/agent/` (its README summarizes each script and its
exit codes).

## Build, test, verify

```bash
# Long-lived compile-check Postgres (sqlx macros + DB tests):
#   docker run -d --name carbon-mig-test-pg -e POSTGRES_PASSWORD=test -p 54329:5432 postgres:16
export DATABASE_URL=postgres://postgres:test@localhost:54329/postgres
cargo sqlx migrate run --source migrations          # once per new migration (fresh DBs only track
                                                    #   from here; sqlx checksums applied files)

cargo fmt && cargo fmt --check                      # never --all
cargo clippy --workspace --all-targets -- -D warnings
SQLX_OFFLINE=true cargo build --workspace --locked  # what CI/Docker build
cargo test --workspace --locked                     # needs DATABASE_URL
(cd crates/indexer && cargo sqlx prepare --check -- --lib)      # after any query/schema change:
(cd crates/api && cargo sqlx prepare --check -- --bin api)      #   drop --check to regenerate, commit .sqlx/
bash scripts/lint-migrations.sh
bash scripts/agent/verify-devnet.sh                 # full rebuild vs live devnet, ~1 min, no API key
```

`indexer snapshot` / `indexer backfill` work against the public devnet RPC with no
credentials; only the live gRPC path (`indexer run`) needs `ALCHEMY_API_KEY`.

## Map

| Where | What |
| --- | --- |
| `addresses.json` | Canonical program addresses + deploy slots (ADR-19); pinned to the registry by tests |
| `idls/` | IDLs of the *deployed* programs; `idls/versions/` archives frozen ones |
| `crates/<p>-decoder` | Generated decoders (see rule 3) |
| `crates/indexer` | Pipeline: registry (`programs.rs`), mappers (`mapping/`), batcher, sync machinery, upgrade recorder (`upgrades.rs`) |
| `crates/api` | GraphQL API (juniper, hand-written; `programUpgrades` serves the version timeline) |
| `migrations/` | sqlx migrations, additive-only, auto-applied at indexer startup |
| `monitoring/` | Prometheus rules (incl. `ProgramUpgradeDetected`) + Grafana |
| `docs/agentic-maintenance.md` | The maintenance-loop design; read it once in full |
| `ARCHITECTURE.md`, `DECISIONS.md`, `RUNBOOK.md`, `docs/deployment.md` | How it works / why / day-2 ops / deploy mechanics |
