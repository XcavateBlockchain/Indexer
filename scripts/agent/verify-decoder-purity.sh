#!/usr/bin/env bash
# Prove the generated decoder crates are pristine generator output (README "Regenerating a
# decoder"): regenerate each one from its IDL into a temp dir with the PINNED carbon-cli and
# diff against the checked-in crate. Any difference means either (a) someone hand-edited a
# generated crate -- forbidden, the crates must stay byte-identical to what the generator
# emits so a regen can never smuggle in a lost local change -- or (b) the IDL under idls/
# changed without its decoder being regenerated.
#
# The CLI version is pinned (ADR-12: the generated code must target carbon-core =0.12.0;
# a newer CLI can generate against a newer core and force a whole-workspace pin bump).
# Override with CARBON_CLI_VERSION only as part of a deliberate, all-pins-in-one-commit
# carbon upgrade.
#
# Usage: verify-decoder-purity.sh [program ...]   (default: all four)
# Exit 0 = every checked crate is byte-identical to fresh generator output.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARBON_CLI_VERSION="${CARBON_CLI_VERSION:-0.12.0}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

command -v npx >/dev/null || { echo "error: npx not installed" >&2; exit 1; }

# Registry name -> crate dir (only the whitelist's differs; README "name mapping").
crate_dir() {
    case "$1" in
        xcavate_whitelist) echo "whitelist-decoder" ;;
        *) echo "$1-decoder" ;;
    esac
}

programs=("$@")
[ ${#programs[@]} -gt 0 ] || programs=(xcavate_whitelist regions marketplace property)

status=0
for name in "${programs[@]}"; do
    dir="$(crate_dir "$name")"
    echo "== $name -> crates/$dir (carbon-cli $CARBON_CLI_VERSION) =="
    npx --yes "@sevenlabs-hq/carbon-cli@$CARBON_CLI_VERSION" parse \
        -i "$REPO_ROOT/idls/$name.json" \
        -o "$TMP/$dir" \
        -s anchor -c \
        --with-postgres true --with-graphql true --with-serde true >/dev/null
    if diff -r --exclude target "$REPO_ROOT/crates/$dir" "$TMP/$dir"; then
        echo "   pristine"
    else
        echo "   DIFFERS from generator output (hand-edit or stale regen)" >&2
        status=1
    fi
done
exit "$status"
