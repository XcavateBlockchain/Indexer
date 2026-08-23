#!/usr/bin/env bash
# Build the upstream realxmarket-solana programs' IDLs and diff them against this repo's
# checked-in idls/ (docs/agentic-maintenance.md step "Build & diff").
#
# The upstream repo commits NO IDLs and has NO CI -- the only way to get an IDL is to run
# `anchor build` yourself, and the only build recipe upstream itself uses is
# deploy/deploy.sh's `NO_DNA=1 anchor build`, which this script mirrors. Two traps this
# script exists to neutralize:
#
#   * `anchor build` stamps each IDL's `address` from the source's declare_id!() -- which
#     matches NONE of the deployed devnet addresses (deploys were made from gitignored
#     keypairs; addresses.json is canonical, ADR-19). Every built IDL is therefore
#     normalized to the deployed address before diffing, or all four programs would look
#     "changed" on every run.
#   * an old anchor-cli (0.29-era) silently emits the OLD IDL format, which would make every
#     diff look catastrophically breaking. The CLI major.minor must match the anchor-lang
#     version in upstream's Cargo.lock (upstream pins no CLI version itself), checked below.
#
# REMEMBER (the race-condition design): upstream main is NOT what is deployed on-chain.
# A diff against main HEAD says what is COMING; scripts/agent/check-program-upgrades.py says
# what has ARRIVED. Never swap idls/ from this script's output alone -- follow
# agent/skills/upstream-sync/SKILL.md.
#
# Exit codes: 0 = all surfaces identical, 10 = additive changes only, 20 = something
# breaking, 1 = error. Per-program reports + summary.json land in $OUT_DIR.
#
# Env: UPSTREAM_URL, UPSTREAM_REF, AGENT_WORK_DIR, OUT_DIR, SKIP_BUILD (reuse target/idl
# from a previous run of the same checkout).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
UPSTREAM_URL="${UPSTREAM_URL:-https://github.com/XcavateBlockchain/realxmarket-solana}"
UPSTREAM_REF="${UPSTREAM_REF:-main}"
AGENT_WORK_DIR="${AGENT_WORK_DIR:-$HOME/.cache/realxmarket-agent}"
WORK_DIR="$AGENT_WORK_DIR/upstream"
IDL_TOOLS="$REPO_ROOT/scripts/agent/idl-tools.py"

err() { echo "error: $*" >&2; exit 1; }

command -v git >/dev/null || err "git not installed"
command -v anchor >/dev/null || err "anchor-cli not installed (install via avm)"
command -v python3 >/dev/null || err "python3 not installed"
command -v jq >/dev/null || err "jq not installed"

# --- fetch upstream --------------------------------------------------------------------------
if [ -d "$WORK_DIR/.git" ]; then
    git -C "$WORK_DIR" fetch origin "$UPSTREAM_REF" --quiet
else
    mkdir -p "$AGENT_WORK_DIR"
    git clone --quiet "$UPSTREAM_URL" "$WORK_DIR"
fi
git -C "$WORK_DIR" checkout --quiet --detach "origin/$UPSTREAM_REF" 2>/dev/null \
    || git -C "$WORK_DIR" checkout --quiet --detach "$UPSTREAM_REF"
SHA="$(git -C "$WORK_DIR" rev-parse --short HEAD)"
OUT_DIR="${OUT_DIR:-$AGENT_WORK_DIR/idls-$SHA}"
mkdir -p "$OUT_DIR"
echo "upstream $UPSTREAM_REF is at $SHA"

# --- toolchain check -------------------------------------------------------------------------
# upstream pins rust (rust-toolchain.toml -- rustup applies it automatically inside WORK_DIR)
# but NOT the anchor CLI; the CLI's major.minor must match Cargo.lock's anchor-lang or the
# emitted IDL format/content is not trustworthy.
WANT_ANCHOR="$(awk '/^name = "anchor-lang"$/{getline; gsub(/version = |"/,""); print; exit}' "$WORK_DIR/Cargo.lock")"
HAVE_ANCHOR="$(anchor --version | awk '{print $2}')"
[ -n "$WANT_ANCHOR" ] || err "could not read anchor-lang version from upstream Cargo.lock"
if [ "${WANT_ANCHOR%.*}" != "${HAVE_ANCHOR%.*}" ]; then
    err "anchor-cli $HAVE_ANCHOR does not match upstream anchor-lang $WANT_ANCHOR \
(need $(echo "${WANT_ANCHOR%.*}").x -- install with: avm install ${WANT_ANCHOR} && avm use ${WANT_ANCHOR})"
fi
echo "toolchain ok: anchor-cli $HAVE_ANCHOR vs upstream anchor-lang $WANT_ANCHOR"

# --- build -----------------------------------------------------------------------------------
if [ "${SKIP_BUILD:-}" != "1" ]; then
    echo "building IDLs (NO_DNA=1 anchor build -- upstream deploy.sh's own invocation) ..."
    (cd "$WORK_DIR" && NO_DNA=1 anchor build)
fi

# --- normalize + diff ------------------------------------------------------------------------
# Registry names equal the upstream lib names for all four programs (the whitelist crate is
# xcavate-whitelist but its lib -- and so its IDL file -- is xcavate_whitelist).
worst=0
summary="[]"
for name in $(jq -r '.programs | keys[]' "$REPO_ROOT/addresses.json"); do
    built="$WORK_DIR/target/idl/$name.json"
    [ -f "$built" ] || err "anchor build produced no $built (program renamed upstream?)"
    address="$(jq -r ".programs[\"$name\"]" "$REPO_ROOT/addresses.json")"
    python3 "$IDL_TOOLS" normalize "$built" --address "$address" -o "$OUT_DIR/$name.json"

    echo
    echo "=== $name ==="
    rc=0
    python3 "$IDL_TOOLS" diff "$REPO_ROOT/idls/$name.json" "$OUT_DIR/$name.json" \
        --json "$OUT_DIR/$name.diff.json" || rc=$?
    [ "$rc" -eq 1 ] && err "idl diff failed for $name"
    [ "$rc" -gt "$worst" ] && worst=$rc
    cls="$(jq -r .classification "$OUT_DIR/$name.diff.json")"
    summary="$(jq --arg n "$name" --arg c "$cls" '. + [{program: $n, classification: $c}]' <<<"$summary")"
done

jq -n --arg sha "$SHA" --arg ref "$UPSTREAM_REF" --argjson programs "$summary" \
    '{upstream_ref: $ref, upstream_sha: $sha, programs: $programs}' \
    > "$OUT_DIR/summary.json"

echo
echo "summary ($OUT_DIR/summary.json):"
jq . "$OUT_DIR/summary.json"
echo
echo "NOTE: this compared idls/ against upstream $UPSTREAM_REF@$SHA -- what is COMING, not"
echo "what is deployed. Run scripts/agent/check-program-upgrades.py to see what is live."
exit "$worst"
