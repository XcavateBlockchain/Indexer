#!/usr/bin/env bash
# End-to-end devnet verification: rebuild the whole dataset from nothing into a disposable
# Postgres and assert it looks like devnet (docs/agentic-maintenance.md step "Verify").
#
# This is the cheapest strong check this stack has: the four programs' complete devnet
# footprint is tiny (tens of signatures/accounts), so a full snapshot + backfill against the
# PUBLIC devnet RPC takes about a minute and exercises decoders, mappers, migrations, the
# batcher, and the upgrade recorder (ADR-24) in one pass -- the same evidence Phase 10's
# migration sign-off used. Run it before opening any decoder/migration PR.
#
# What it asserts:
#   * migrations apply from zero (they run at process startup);
#   * `indexer snapshot` decodes EVERY account of every program (zero undecodable -- an
#     undecodable account is exactly the IDL-drift signal this pipeline exists to catch);
#   * `indexer backfill` walks every program's history to completion;
#   * each program's config PDA (addresses.json ground truth) landed in its state table;
#   * program_upgrades holds each program's seeded deploy boundary, and any 'chain' rows
#     are surfaced for the operator to read;
#   * program_instructions is non-empty for every program.
#
# Needs: docker, cargo, jq. Uses only the public devnet RPC (no ALCHEMY_API_KEY). The
# throwaway Postgres container is created on VERIFY_PG_PORT (default 54331 -- deliberately
# NOT 54329, which is the long-lived sqlx compile-check container) and removed on exit.
#
# Exit 0 = every assertion held. Non-zero = the working tree is not safe to ship.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PORT="${VERIFY_PG_PORT:-54331}"
CONTAINER="indexer-verify-pg-$PORT"
DB_URL="postgres://postgres:test@localhost:$PORT/postgres"

err() { echo "VERIFY FAILED: $*" >&2; exit 1; }
psql_c() { docker exec "$CONTAINER" psql -U postgres -d postgres -tA -c "$1"; }

command -v docker >/dev/null || err "docker not installed"
command -v jq >/dev/null || err "jq not installed"

cleanup() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }
trap cleanup EXIT
cleanup
docker run -d --name "$CONTAINER" -e POSTGRES_PASSWORD=test -p "$PORT:5432" postgres:16 >/dev/null
# -h 127.0.0.1 forces the readiness probe over TCP: the image's first-boot entrypoint runs
# a temporary unix-socket-only server during initdb, and a socket-based pg_isready can
# answer "ready" from THAT server moments before it is shut down and the real one starts.
for _ in $(seq 1 30); do
    docker exec "$CONTAINER" pg_isready -h 127.0.0.1 -U postgres >/dev/null 2>&1 && break
    sleep 1
done
docker exec "$CONTAINER" pg_isready -h 127.0.0.1 -U postgres >/dev/null || err "postgres did not come up"

cd "$REPO_ROOT"
# SQLX_OFFLINE=true: the sqlx macros must compile against the checked-in .sqlx caches (what
# CI and the Docker build do), NOT against $DB_URL -- at compile time the fresh verify
# database has no schema yet (migrations run at process startup), so an online compile
# against it fails with "relation does not exist".
export SQLX_OFFLINE=true
echo "== snapshot (public devnet RPC, fresh DB on :$PORT) =="
snapshot_out="$(DATABASE_URL="$DB_URL" cargo run --quiet -p indexer -- snapshot 2>&1 | tee /dev/stderr)" \
    || err "indexer snapshot exited non-zero"
if grep -q "undecodable" <<<"$snapshot_out"; then
    err "snapshot reported undecodable accounts -- the checked-in IDLs do not match the deployed programs"
fi

echo "== backfill =="
DATABASE_URL="$DB_URL" cargo run --quiet -p indexer -- backfill \
    || err "indexer backfill exited non-zero"

echo "== assertions =="
programs_total="$(jq -r '.programs | length' addresses.json)"

incomplete="$(psql_c "SELECT count(*) FROM sync_state WHERE NOT backfill_complete")"
[ "$incomplete" = "0" ] || err "$incomplete program(s) did not complete backfill"
rows="$(psql_c "SELECT count(*) FROM sync_state")"
[ "$rows" = "$programs_total" ] || err "sync_state has $rows rows, expected $programs_total"

# Each program's config PDA (ground truth from addresses.json) must be in its state table.
declare -A config_table=(
    [whitelist]=config [regions]=regions_config
    [marketplace]=marketplace_config [property]=property_config
)
for key in whitelist regions marketplace property; do
    pda="$(jq -r ".configs[\"$key\"]" addresses.json)"
    hex="$(python3 -c "
import sys
A='123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'
s='$pda'; n=0
for c in s: n = n*58 + A.index(c)
print(n.to_bytes(32,'big').hex())")"
    found="$(psql_c "SELECT count(*) FROM ${config_table[$key]} WHERE pubkey = '\\x$hex'")"
    [ "$found" = "1" ] || err "$key config PDA $pda missing from ${config_table[$key]}"
done

# Every program produced instruction history.
empty="$(psql_c "SELECT count(*) FROM (
    SELECT s.program_id FROM sync_state s
    LEFT JOIN program_instructions pi ON pi.program_id = s.program_id
    GROUP BY s.program_id HAVING count(pi.signature) = 0) t")"
[ "$empty" = "0" ] || err "$empty program(s) have no program_instructions rows after backfill"

# Version boundaries: one seeded 'deploy' row per program, always. 'chain' rows mean the
# deployed programs have been upgraded -- not a failure, but the operator must know.
seeds="$(psql_c "SELECT count(*) FROM program_upgrades WHERE source = 'deploy'")"
[ "$seeds" = "$programs_total" ] || err "expected $programs_total seeded deploy boundaries, found $seeds"
chain_rows="$(psql_c "SELECT count(*) FROM program_upgrades WHERE source = 'chain'")"
if [ "$chain_rows" != "0" ]; then
    echo "NOTE: $chain_rows on-chain upgrade boundary(ies) recorded during this walk:"
    psql_c "SELECT encode(program_id,'hex'), upgrade_slot, signature FROM program_upgrades WHERE source='chain'"
    echo "      (the deployed programs are ahead of a plain v1 timeline -- see RUNBOOK.md"
    echo "      'After a program upgrade' before shipping anything)"
fi

sigs="$(psql_c "SELECT count(*) FROM program_instructions")"
accounts="$(psql_c "SELECT count(*) FROM sync_state WHERE snapshot_slot IS NOT NULL")"
echo "VERIFY OK: $rows programs, $sigs instructions, $accounts snapshots, $seeds deploy boundaries, $chain_rows chain upgrades"
