#!/usr/bin/env python3
"""IDL utilities for the maintenance pipeline (docs/agentic-maintenance.md).

Works on new-format Anchor IDLs (spec "0.1.0" -- top-level `address`, `metadata`,
`instructions`, `accounts`, `events`, `types`, each instruction/account/event carrying its
8-byte `discriminator`), which is what this repo's `idls/*.json` are and what an Anchor 1.x
`anchor build` emits. Stdlib only; no third-party imports.

Subcommands:

  normalize IDL --address ADDR [-o OUT]
      Rewrite the IDL's top-level `address` to ADDR and re-serialize in the repo's canonical
      form (2-space indent, key order preserved, trailing newline). `anchor build` stamps the
      address from the source's declare_id!, which does NOT match the deployed devnet
      addresses (deploys were made from gitignored keypairs) -- so a freshly built IDL must be
      normalized against addresses.json before it is diffed or committed. Everything else is
      left byte-exact.

  diff OLD NEW [--json OUT]
      Structural diff of two IDLs, classified for the update pipeline:
        exit 0  -- surface identical (metadata/docs may differ; says so on stdout)
        exit 10 -- ADDITIVE only: new instructions/accounts/events/types, nothing existing
                   changed or removed. The safe case: regenerate the decoder, map the new
                   surface, additive migrations.
        exit 20 -- BREAKING: something existing changed or was removed. Requires the
                   versioned-decoder procedure (agent/skills/versioned-decoder/SKILL.md).
        exit 1  -- error (unreadable file, old-format IDL, ...).
      "Changed" is judged on what affects decoding and mapping: discriminators, arg lists,
      account lists (order matters -- close positions are account-list indices), event
      fields, and the named type definitions they are built from. The top-level `address`
      and `metadata` are reported informationally and never affect the classification
      (diff normalized files if address noise is unwanted).
"""

import argparse
import json
import sys


def load_idl(path):
    try:
        with open(path, encoding="utf-8") as f:
            idl = json.load(f)
    except (OSError, json.JSONDecodeError) as e:
        sys.exit(f"error: cannot read {path}: {e}")
    if "metadata" not in idl or "instructions" not in idl:
        sys.exit(f"error: {path} does not look like an Anchor IDL")
    if idl.get("metadata", {}).get("spec") is None:
        sys.exit(
            f"error: {path} has no metadata.spec -- this looks like a pre-1.0 (old-format) "
            "IDL; it was probably built with an old anchor-cli (see build-upstream-idls.sh's "
            "toolchain check)"
        )
    return idl


def dump_idl(idl, path):
    text = json.dumps(idl, indent=2, ensure_ascii=False) + "\n"
    if path == "-":
        sys.stdout.write(text)
    else:
        with open(path, "w", encoding="utf-8") as f:
            f.write(text)


def cmd_normalize(args):
    idl = load_idl(args.idl)
    idl["address"] = args.address
    dump_idl(idl, args.out or args.idl)
    return 0


def by_name(items):
    return {item["name"]: item for item in items or []}


def instruction_surface(ix, types):
    """What matters for decoding + mapping one instruction: its discriminator, its args, and
    its account list in order (positions are load-bearing: `close =` positions are indices).
    Signer/writable/optional flags ride along; PDA derivation seeds do not affect decoding."""
    return {
        "discriminator": ix.get("discriminator"),
        "args": [
            {"name": a["name"], "type": resolve_type(a.get("type"), types)}
            for a in ix.get("args", [])
        ],
        "accounts": [
            {
                "name": acc.get("name"),
                "writable": acc.get("writable", False),
                "signer": acc.get("signer", False),
                "optional": acc.get("optional", False),
            }
            for acc in flatten_accounts(ix.get("accounts", []))
        ],
    }


def flatten_accounts(accounts):
    """Composite account groups nest one level in the IDL; decode positions are the flattened
    order."""
    out = []
    for acc in accounts:
        if "accounts" in acc:
            out.extend(flatten_accounts(acc["accounts"]))
        else:
            out.append(acc)
    return out


def resolve_type(ty, types, depth=0):
    """Structurally resolve a type reference so a change inside a named struct/enum is seen
    by everything built from it. Cycles/depth are capped defensively; at the cap the name is
    returned, which still changes when the name changes."""
    if depth > 12:
        return ty
    if isinstance(ty, str):
        defined = types.get(ty)
        if defined is not None:
            return {ty: resolve_type(defined.get("type"), types, depth + 1)}
        return ty
    if isinstance(ty, dict):
        if "defined" in ty:
            name = ty["defined"]
            generics = ty.get("generics")
            if isinstance(name, dict):  # {"defined": {"name": ..., "generics": [...]}}
                generics = name.get("generics", generics)
                name = name.get("name")
            defined = types.get(name)
            resolved = (
                {name: resolve_type(defined.get("type"), types, depth + 1)}
                if defined is not None
                else {"defined": name}
            )
            # Generic ARGUMENTS are part of the layout: Wrapper<u64> vs Wrapper<u32>
            # differ even though the named definition (with its opaque placeholder) is
            # identical -- dropping them would classify a breaking change as identical.
            if generics:
                resolved = {
                    "of": resolved,
                    "generics": resolve_type(generics, types, depth + 1),
                }
            return resolved
        # `docs` never affects layout at ANY nesting level (struct fields, enum variants,
        # ...); keeping it would classify a comment-only edit as breaking.
        return {
            k: resolve_type(v, types, depth + 1) for k, v in ty.items() if k != "docs"
        }
    if isinstance(ty, list):
        return [resolve_type(v, types, depth + 1) for v in ty]
    return ty


def account_surface(acc, types):
    return {
        "discriminator": acc.get("discriminator"),
        "layout": resolve_type(acc["name"], types),
    }


def event_surface(ev, types):
    return {
        "discriminator": ev.get("discriminator"),
        "layout": resolve_type(ev["name"], types),
    }


def compare(kind, old_items, new_items, old_surface, new_surface):
    """Each side's surface is computed with its OWN type table -- resolving a new item
    against the old side's types (or vice versa) would hide or invent changes."""
    old_names, new_names = set(old_items), set(new_items)
    return {
        "kind": kind,
        "added": sorted(new_names - old_names),
        "removed": sorted(old_names - new_names),
        "changed": sorted(
            name
            for name in old_names & new_names
            if old_surface(old_items[name]) != new_surface(new_items[name])
        ),
    }


def cmd_diff(args):
    old, new = load_idl(args.old), load_idl(args.new)
    old_types = by_name(old.get("types"))
    new_types = by_name(new.get("types"))

    sections = [
        compare(
            "instructions",
            by_name(old.get("instructions")),
            by_name(new.get("instructions")),
            lambda ix: instruction_surface(ix, old_types),
            lambda ix: instruction_surface(ix, new_types),
        ),
        compare(
            "accounts",
            by_name(old.get("accounts")),
            by_name(new.get("accounts")),
            lambda acc: account_surface(acc, old_types),
            lambda acc: account_surface(acc, new_types),
        ),
        compare(
            "events",
            by_name(old.get("events")),
            by_name(new.get("events")),
            lambda ev: event_surface(ev, old_types),
            lambda ev: event_surface(ev, new_types),
        ),
    ]
    # Standalone type defs: a changed-but-unreferenced type is still reported (a referenced
    # one already shows up as a changed instruction/account/event).
    old_t, new_t = set(old_types), set(new_types)
    sections.append(
        {
            "kind": "types",
            "added": sorted(new_t - old_t),
            "removed": sorted(old_t - new_t),
            "changed": sorted(
                n
                for n in old_t & new_t
                if resolve_type(n, old_types) != resolve_type(n, new_types)
            ),
        }
    )

    breaking = any(s["removed"] or s["changed"] for s in sections)
    additive = any(s["added"] for s in sections)
    classification = "breaking" if breaking else ("additive" if additive else "identical")

    info = {}
    if old.get("address") != new.get("address"):
        info["address"] = {"old": old.get("address"), "new": new.get("address")}
    if old.get("metadata") != new.get("metadata"):
        info["metadata"] = {"old": old.get("metadata"), "new": new.get("metadata")}

    report = {
        "old": args.old,
        "new": args.new,
        "classification": classification,
        "sections": sections,
        "informational": info,
    }
    if args.json:
        with open(args.json, "w", encoding="utf-8") as f:
            json.dump(report, f, indent=2)
            f.write("\n")

    print(f"classification: {classification.upper()}")
    for s in sections:
        if s["added"] or s["removed"] or s["changed"]:
            print(f"  {s['kind']}:")
            for label in ("added", "removed", "changed"):
                if s[label]:
                    print(f"    {label}: {', '.join(s[label])}")
    for key, val in info.items():
        print(f"  note ({key}, non-classifying): {val['old']} -> {val['new']}")
    if classification == "identical" and info:
        print("surface identical; only informational fields differ")

    return {"identical": 0, "additive": 10, "breaking": 20}[classification]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_norm = sub.add_parser("normalize", help="rewrite the IDL address to the deployed one")
    p_norm.add_argument("idl")
    p_norm.add_argument("--address", required=True)
    p_norm.add_argument("-o", "--out", help="output path (default: in place; '-' for stdout)")

    p_diff = sub.add_parser("diff", help="classify the structural difference of two IDLs")
    p_diff.add_argument("old")
    p_diff.add_argument("new")
    p_diff.add_argument("--json", help="also write the machine-readable report here")

    args = parser.parse_args()
    sys.exit({"normalize": cmd_normalize, "diff": cmd_diff}[args.cmd](args))


if __name__ == "__main__":
    main()
