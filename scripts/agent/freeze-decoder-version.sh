#!/usr/bin/env bash
# Freeze the CURRENT decoder crate of a program as an immutable versioned copy, ahead of
# regenerating crates/<p>-decoder from a new IDL. This is step one of the versioned-decoder
# procedure (agent/skills/versioned-decoder/SKILL.md, ADR-25):
#
#   * crates/<p>-decoder        stays the CURRENT version and keeps regenerating byte-
#                               identically from idls/<p>.json (purity story unchanged);
#   * crates/<p>-decoder-vN     is the frozen pre-upgrade copy, never regenerated again --
#                               its provenance is this script + git history + the archived
#                               IDL at idls/versions/<p>/vN.json.
#
# The frozen copy differs from generator output by EXACTLY ONE LINE: the package `name` in
# its Cargo.toml gains a `-vN` suffix (two path deps with one package name cannot coexist in
# a cargo graph). That is the one sanctioned deviation from "never hand-edit a generated
# crate", it is made by this script rather than a hand, and the script verifies nothing else
# changed. The lib will import as `carbon_<p>_decoder_vN`.
#
# Usage: freeze-decoder-version.sh <registry_name> <version>
#   e.g. freeze-decoder-version.sh marketplace 1
#
# After this script, the remaining wiring is on you (the skill has the checklist): root
# Cargo.toml `exclude` entry, docker/rust.Dockerfile COPY line, the indexer's dependency +
# versioned mapper, and the archived IDL's slot range once the upgrade slot is known.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
name="${1:?usage: freeze-decoder-version.sh <registry_name> <version>}"
version="${2:?usage: freeze-decoder-version.sh <registry_name> <version>}"

case "$name" in
    xcavate_whitelist) dir="whitelist-decoder" ;;
    regions|marketplace|property) dir="$name-decoder" ;;
    *) echo "error: unknown registry name '$name'" >&2; exit 1 ;;
esac

src="$REPO_ROOT/crates/$dir"
dst="$REPO_ROOT/crates/$dir-v$version"
idl_src="$REPO_ROOT/idls/$name.json"
idl_dst="$REPO_ROOT/idls/versions/$name/v$version.json"

[ -d "$src" ] || { echo "error: $src does not exist" >&2; exit 1; }
[ ! -e "$dst" ] || { echo "error: $dst already exists" >&2; exit 1; }
[ ! -e "$idl_dst" ] || { echo "error: $idl_dst already exists" >&2; exit 1; }

cp -r "$src" "$dst"
rm -rf "$dst/target"

old_pkg="$(grep -m1 '^name = ' "$dst/Cargo.toml" | sed 's/^name = "\(.*\)"$/\1/')"
new_pkg="$old_pkg-v$version"
sed -i "0,/^name = \"$old_pkg\"$/s//name = \"$new_pkg\"/" "$dst/Cargo.toml"

# Prove the copy differs from the source by exactly the package-name line.
if [ "$(diff -r --exclude target "$src" "$dst" | grep -c '^[<>]')" != "2" ]; then
    echo "error: the frozen copy differs from the source by more than the package name:" >&2
    diff -r --exclude target "$src" "$dst" >&2 || true
    exit 1
fi

mkdir -p "$(dirname "$idl_dst")"
cp "$idl_src" "$idl_dst"

echo "frozen: crates/$dir-v$version (package $new_pkg, lib $(echo "$new_pkg" | tr '-' '_'))"
echo "archived IDL: idls/versions/$name/v$version.json"
echo
echo "still to wire (see agent/skills/versioned-decoder/SKILL.md):"
echo "  1. root Cargo.toml: add \"crates/$dir-v$version\" to the workspace exclude list"
echo "  2. docker/rust.Dockerfile: add a COPY line for crates/$dir-v$version"
echo "  3. crates/indexer/Cargo.toml: depend on the frozen crate"
echo "  4. regenerate crates/$dir from the NEW idls/$name.json (verify-decoder-purity.sh)"
echo "  5. versioned mapper routing on the recorded upgrade slot (program_upgrades)"
