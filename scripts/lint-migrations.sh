#!/usr/bin/env bash
# Migration policy lint (ADR-25's additive-only rule), run by CI and by the maintenance
# agent before every migration PR. Two classes of check:
#
#   1. History is immutable. Relative to BASE_REF, no file under migrations/ may be modified,
#      deleted, or renamed -- sqlx checksums applied files, so editing one bricks every
#      existing database at its next startup. Only NEW files with a strictly higher 4-digit
#      number are allowed.
#
#   2. New migrations must be additive. A destructive statement in a new file fails the lint:
#      DROP TABLE, DROP COLUMN, ALTER COLUMN (type/nullability rewrites), RENAME, DELETE,
#      TRUNCATE, and UPDATE (backfill UPDATEs are how columns silently rot -- 0007's was
#      correct only under an ordering argument its header spells out). A statement that is
#      genuinely needed and argued (the 0007 pattern: state the correctness argument in the
#      header) is unlocked per-file, per-keyword with a marker line:
#
#          -- lint: allow UPDATE -- <one-line justification>
#
#      DROP VIEW / CREATE OR REPLACE VIEW are always allowed: views are derived, not data,
#      and replacing one is the sanctioned way to change it (see 0005).
#
# Usage: lint-migrations.sh [BASE_REF]     (default: origin/main, then main)
# Exit 0 = clean, 1 = violation, 2 = cannot resolve BASE_REF.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

BASE_REF="${1:-${BASE_REF:-}}"
# `rev-parse --verify` alone accepts ANY syntactically-valid 40-hex sha without checking the
# object exists; `^{commit}` forces a real lookup, so a dead sha (force-push, wrong remote)
# is exit 2 instead of a silent "no migration changes" pass.
if [ -z "$BASE_REF" ]; then
    if git rev-parse --verify --quiet 'origin/main^{commit}' >/dev/null; then BASE_REF=origin/main
    elif git rev-parse --verify --quiet 'main^{commit}' >/dev/null; then BASE_REF=main
    else echo "lint-migrations: cannot resolve a base ref (pass one)" >&2; exit 2; fi
fi
git rev-parse --verify --quiet "$BASE_REF^{commit}" >/dev/null \
    || { echo "lint-migrations: no such commit: $BASE_REF" >&2; exit 2; }

# Diff the WORKING TREE against the merge base, not committed HEAD against BASE_REF:
# that way staged-but-uncommitted new migrations and uncommitted edits to applied ones are
# caught in local pre-PR runs too (in CI the working tree equals HEAD, so it is the same
# diff). The merge base keeps a moved-ahead origin/main's own new migrations from showing
# up as local deletions.
base_commit="$(git merge-base "$BASE_REF" HEAD 2>/dev/null || git rev-parse "$BASE_REF")"

status=0

# --- 1. immutability + numbering -------------------------------------------------------------
new_files=()
while IFS=$'\t' read -r state file _; do
    [ -n "$state" ] || continue
    case "$state" in
        A) new_files+=("$file") ;;
        M|D|R*|C*) echo "VIOLATION: $file is ${state} -- applied migrations are immutable (sqlx checksums them)"; status=1 ;;
    esac
done < <(git diff --name-status "$base_commit" -- migrations/)

# Untracked new migrations (working-tree runs before anything is staged or committed).
while IFS= read -r file; do
    new_files+=("$file")
done < <(git ls-files --others --exclude-standard migrations/)

if [ "$status" -eq 0 ] && [ ${#new_files[@]} -eq 0 ]; then
    echo "lint-migrations: no migration changes vs $BASE_REF"
    exit 0
fi

max_existing="$(git ls-tree -r --name-only "$base_commit" -- migrations/ \
    | sed -n 's|^migrations/\([0-9]\{4\}\)_.*\.sql$|\1|p' | sort -n | tail -1)"
seen="$max_existing"
for file in $(printf '%s\n' "${new_files[@]}" | sort -u); do
    base="$(basename "$file")"
    if ! [[ "$base" =~ ^([0-9]{4})_[a-z0-9_]+\.sql$ ]]; then
        echo "VIOLATION: $file does not match NNNN_snake_case.sql"; status=1; continue
    fi
    num="${BASH_REMATCH[1]}"
    if [ -n "$seen" ] && [ "$((10#$num))" -le "$((10#$seen))" ]; then
        echo "VIOLATION: $file does not increase the migration number (last: $seen)"; status=1
    fi
    seen="$num"

    # --- 2. additive-only statements ---------------------------------------------------------
    # Strip line and block comments so prose about DROP/UPDATE never trips the lint, then
    # FLATTEN to one line before scanning -- SQL keywords can legally be split across
    # newlines ("DROP\n  TABLE"), and a line-based grep would wave that through.
    sql="$(sed 's/--.*$//' "$file" | tr '\n' ' ' \
        | sed 's|/\*[^*]*\*\+\([^/*][^*]*\*\+\)*/| |g' | tr -s '[:space:]' ' ')"
    for keyword in "DROP TABLE" "DROP COLUMN" "ALTER COLUMN" "RENAME" "DELETE" "TRUNCATE" "UPDATE"; do
        pattern="${keyword/ /[[:space:]]+}"
        if grep -Eqi "(^|[^A-Za-z_])${pattern}([^A-Za-z_]|$)" <<<"$sql"; then
            if grep -Eqi "^--[[:space:]]*lint:[[:space:]]*allow[[:space:]]+${keyword}([^A-Za-z_]|$)" "$file"; then
                echo "note: $file uses '$keyword' under an explicit 'lint: allow' marker"
            else
                echo "VIOLATION: $file contains '$keyword' -- not additive (add '-- lint: allow $keyword -- <why>' only with a written correctness argument)"
                status=1
            fi
        fi
    done
done

[ "$status" -eq 0 ] && echo "lint-migrations: OK (base $BASE_REF)"
exit "$status"
