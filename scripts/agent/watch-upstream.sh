#!/usr/bin/env bash
# The maintenance agent's trigger: poll the upstream realxmarket-solana repo's main branch
# and run a command whenever its HEAD moves (docs/agentic-maintenance.md "Trigger").
#
# Polling, not webhooks, on purpose: the agent host (the DGX box) sits behind NAT with no
# public endpoint, the upstream repo has no CI to send a repository_dispatch from, and a
# 5-minute poll of one commit endpoint is well inside anonymous GitHub API limits (uses gh
# with its auth when available, plain HTTPS otherwise).
#
# Debounce: upstream lands bursts of commits (and has added-then-dropped a feature within
# two days), so a HEAD younger than MIN_AGE_MINUTES is noted but NOT acted on until it has
# sat still -- the pipeline wants settled states, not every intermediate commit. Remember
# the deeper debounce lives in the pipeline itself: what matters is what the multisig
# DEPLOYS, and check-program-upgrades.py is the arbiter of that.
#
# On a settled new HEAD this runs ON_CHANGE_CMD with UPSTREAM_SHA exported. Default command
# just appends a line to $STATE_DIR/pending -- the always-on Hermes agent watches that file
# and starts agent/skills/upstream-sync/SKILL.md. State survives restarts in $STATE_DIR.
#
# Env: UPSTREAM_REPO (owner/name), POLL_SECONDS (300), MIN_AGE_MINUTES (30), STATE_DIR,
#      ON_CHANGE_CMD, ONCE=1 (single check, exit 0 = no change / 3 = change signalled).

set -euo pipefail

UPSTREAM_REPO="${UPSTREAM_REPO:-XcavateBlockchain/realxmarket-solana}"
POLL_SECONDS="${POLL_SECONDS:-300}"
MIN_AGE_MINUTES="${MIN_AGE_MINUTES:-30}"
STATE_DIR="${STATE_DIR:-${AGENT_WORK_DIR:-$HOME/.cache/realxmarket-agent}/watch}"
mkdir -p "$STATE_DIR"

fetch_head() {
    # -> "<sha> <commit-iso8601-date>" for the branch tip.
    if command -v gh >/dev/null 2>&1; then
        gh api "repos/$UPSTREAM_REPO/commits/main" \
            --jq '.sha + " " + .commit.committer.date' 2>/dev/null && return
    fi
    curl -fsS "https://api.github.com/repos/$UPSTREAM_REPO/commits/main" \
        | jq -r '.sha + " " + .commit.committer.date'
}

check_once() {
    local head sha date age_min last
    if ! head="$(fetch_head)"; then
        echo "$(date -Is) poll failed (network/rate limit) -- will retry" >&2
        return 0
    fi
    sha="${head%% *}"
    date="${head#* }"
    last="$(cat "$STATE_DIR/last_sha" 2>/dev/null || true)"
    if [ "$sha" = "$last" ]; then
        return 0
    fi
    age_min=$(( ($(date +%s) - $(date -d "$date" +%s)) / 60 ))
    if [ "$age_min" -lt "$MIN_AGE_MINUTES" ]; then
        echo "$(date -Is) new HEAD $sha is ${age_min}m old (< ${MIN_AGE_MINUTES}m) -- letting it settle"
        return 0
    fi
    echo "$(date -Is) upstream main moved: ${last:-<none>} -> $sha (settled ${age_min}m)"
    # Mark the sha consumed only AFTER the handler succeeded: writing it first would make
    # a failed ON_CHANGE_CMD permanently swallow the trigger (the next poll would see
    # sha == last_sha and never retry).
    if [ -n "${ON_CHANGE_CMD:-}" ]; then
        if ! UPSTREAM_SHA="$sha" bash -c "$ON_CHANGE_CMD"; then
            echo "$(date -Is) ON_CHANGE_CMD failed for $sha -- will retry next poll" >&2
            return 0
        fi
    else
        echo "$(date -Is) $sha" >> "$STATE_DIR/pending"
        echo "queued in $STATE_DIR/pending -- the maintenance agent picks it up from there"
    fi
    echo "$sha" > "$STATE_DIR/last_sha"
    return 3
}

if [ "${ONCE:-}" = "1" ]; then
    check_once && exit 0 || exit $?
fi

echo "watching $UPSTREAM_REPO main every ${POLL_SECONDS}s (state: $STATE_DIR)"
while true; do
    check_once || true
    sleep "$POLL_SECONDS"
done
