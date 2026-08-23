#!/usr/bin/env python3
"""Probe the chain for upgrades of the indexed programs (ADR-24's out-of-band check).

For every program in addresses.json: read its program account (owner must be the BPF
upgradeable loader; its data is the ProgramData account's address -- no PDA math needed),
then read the ProgramData account, whose header carries the LAST DEPLOY SLOT. Compare that
against addresses.json's deploy_slots (the version-1 slots compiled into the indexer's
registry) and, with --graphql URL, against what the indexer itself has recorded in
`program_upgrades` (the `programUpgrades` query).

This is the belt to the indexer's braces: the in-pipeline recorder (crates/indexer/src/
upgrades.rs) sees upgrades through the data paths, while this script asks the chain
directly -- run it before trusting any upstream diff (build-upstream-idls.sh tells you what
is COMING; this tells you what has ARRIVED) and after a multisig executes an upgrade.

Exit codes: 0 = no program upgraded past its known boundary, 10 = at least one upgrade the
indexer's known boundaries do not cover, 1 = error. Stdlib only.

Layout facts (solana_loader_v3_interface::state::UpgradeableLoaderState, bincode):
  program account data   = enum tag 2 (u32 LE) + programdata address (32 bytes)
  programdata account    = enum tag 3 (u32 LE) + slot (u64 LE) + Option<Pubkey> authority
A program owned by loader-v4 (or anything but loader-v3) is reported loudly: the recorder
only understands loader-v3, so such a migration needs maintenance attention by itself.
"""

import argparse
import base64
import json
import struct
import sys
import urllib.request

LOADER_V3 = "BPFLoaderUpgradeab1e11111111111111111111111"

B58_ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


def b58encode(data: bytes) -> str:
    n = int.from_bytes(data, "big")
    out = ""
    while n:
        n, r = divmod(n, 58)
        out = B58_ALPHABET[r] + out
    return "1" * (len(data) - len(data.lstrip(b"\0"))) + (out or "")


def rpc(url, method, params):
    body = json.dumps(
        {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
    ).encode()
    req = urllib.request.Request(url, body, {"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as resp:
        reply = json.load(resp)
    if "error" in reply:
        sys.exit(f"error: {method} failed: {reply['error']}")
    return reply["result"]


def get_account(url, pubkey):
    result = rpc(url, "getAccountInfo", [pubkey, {"encoding": "base64"}])
    value = result.get("value")
    if value is None:
        return None
    return value["owner"], base64.b64decode(value["data"][0])


def last_deploy_slot(url, program_address):
    """(owner, last_deploy_slot | None). None slot => not a loader-v3 upgradeable program."""
    acct = get_account(url, program_address)
    if acct is None:
        return None, None
    owner, data = acct
    if owner != LOADER_V3:
        return owner, None
    if len(data) < 36 or struct.unpack_from("<I", data)[0] != 2:
        sys.exit(f"error: {program_address} is loader-v3-owned but not a Program account")
    programdata = b58encode(data[4:36])
    pd = get_account(url, programdata)
    if pd is None:
        sys.exit(f"error: programdata {programdata} of {program_address} does not exist")
    _, pd_data = pd
    if len(pd_data) < 12 or struct.unpack_from("<I", pd_data)[0] != 3:
        sys.exit(f"error: {programdata} is not a ProgramData account")
    return owner, struct.unpack_from("<Q", pd_data, 4)[0]


def indexer_boundaries(graphql_url):
    """program registry name -> highest boundary slot the indexer has recorded."""
    query = "{ programUpgrades { program upgradeSlot } }"
    body = json.dumps({"query": query}).encode()
    req = urllib.request.Request(
        graphql_url, body, {"Content-Type": "application/json"}
    )
    with urllib.request.urlopen(req, timeout=30) as resp:
        reply = json.load(resp)
    if reply.get("errors"):
        sys.exit(f"error: programUpgrades query failed: {reply['errors']}")
    # GraphQL enum spellings (XCAVATE_WHITELIST) -> registry names (xcavate_whitelist).
    out = {}
    for row in reply["data"]["programUpgrades"]:
        name = row["program"].lower()
        out[name] = max(out.get(name, 0), int(row["upgradeSlot"]))
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--addresses",
        default=None,
        help="path to addresses.json (default: repo root above this script)",
    )
    parser.add_argument("--rpc", default=None, help="RPC URL (default: addresses.json's cluster)")
    parser.add_argument(
        "--graphql",
        default=None,
        help="indexer GraphQL URL (e.g. http://localhost:3010/graphql) to also compare "
        "against the indexer's recorded boundaries",
    )
    args = parser.parse_args()

    import os

    addresses_path = args.addresses or os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "..", "..", "addresses.json"
    )
    with open(addresses_path, encoding="utf-8") as f:
        addresses = json.load(f)
    rpc_url = args.rpc or addresses["cluster"]
    known = {
        name: int(slot) for name, slot in addresses.get("deploy_slots", {}).items()
    }
    recorded = indexer_boundaries(args.graphql) if args.graphql else None

    upgraded = False
    print(f"cluster: {rpc_url}")
    for name, address in addresses["programs"].items():
        owner, slot = last_deploy_slot(rpc_url, address)
        expected = known.get(name)
        if owner is None:
            print(f"  {name:18} {address}  MISSING ON-CHAIN (devnet reset? wrong cluster?)")
            upgraded = True
            continue
        if slot is None:
            print(
                f"  {name:18} {address}  owner {owner} is NOT loader-v3 -- the upgrade "
                "recorder cannot see this program's upgrades; maintenance needed"
            )
            upgraded = True
            continue
        line = f"  {name:18} last deploy slot {slot}"
        if expected is not None and slot > expected:
            baseline = expected
            if recorded is not None:
                baseline = max(baseline, recorded.get(name, 0))
            if slot > baseline:
                line += f"  UPGRADED past known boundary {baseline}"
                upgraded = True
            else:
                line += f"  upgraded, boundary {baseline} already recorded by the indexer"
        elif expected is not None and slot < expected:
            line += f"  BEFORE expected deploy slot {expected} -- redeployed (devnet reset?)"
            upgraded = True
        else:
            line += "  unchanged (still the version-1 deploy)"
        print(line)

    sys.exit(10 if upgraded else 0)


if __name__ == "__main__":
    main()
