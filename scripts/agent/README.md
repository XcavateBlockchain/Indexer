# Maintenance-agent tooling

The executable half of the agentic-maintenance pipeline (`docs/agentic-maintenance.md`).
Every script is standalone, deterministic about its exit codes, and safe to re-run; the
procedures that string them together live in `agent/skills/`, and `AGENTS.md` is the entry
point that tells an agent which skill to load.

| Script | Job | Exit codes |
| --- | --- | --- |
| `watch-upstream.sh` | Poll upstream `realxmarket-solana` main; queue settled HEAD moves for the agent. | `0` no change, `3` change signalled (`ONCE=1`) |
| `build-upstream-idls.sh` | Clone/fetch upstream, `NO_DNA=1 anchor build` with a version-checked Anchor CLI, normalize the IDL addresses to `addresses.json`, diff against `idls/`. | `0` identical, `10` additive, `20` breaking |
| `idl-tools.py` | The `normalize` / `diff` primitives the build script uses; `diff` classifies structurally (discriminators, args, account order, resolved types). | same as above |
| `check-program-upgrades.py` | Ask the CHAIN what is deployed: read each program's ProgramData last-deploy slot, compare with `addresses.json` `deploy_slots` and (with `--graphql`) the indexer's recorded boundaries. | `0` unchanged, `10` upgraded/anomalous |
| `verify-devnet.sh` | Full rebuild into a disposable Postgres from the public devnet RPC (snapshot + backfill), then assert ground truth: zero undecodable accounts, complete backfills, config PDAs present, seeded version boundaries. | `0` verified |
| `verify-decoder-purity.sh` | Regenerate each decoder crate with the pinned carbon-cli and diff against the checked-in crate: proves nobody hand-edited generated code and no IDL changed without a regen. | `0` pristine |
| `freeze-decoder-version.sh` | Step one of the versioned-decoder procedure: snapshot the current decoder crate as `crates/<p>-decoder-vN` (single sanctioned one-line rename), archive the IDL under `idls/versions/`. | `0` frozen |
| `../lint-migrations.sh` | ADR-25's additive-only migration policy (also a CI job): immutable history, strictly-increasing numbering, no destructive SQL without an in-file `-- lint: allow` argument. | `0` clean |

The mental model behind the split (the race-condition design, `DECISIONS.md` ADR-23):

* `build-upstream-idls.sh` tells you what is **coming** (upstream main),
* `check-program-upgrades.py` tells you what has **arrived** (the chain),
* the indexer's own `program_upgrades` table / `programUpgrades` GraphQL query tells you
  what has been **observed and recorded**.

The indexer must always be ready for the chain before the chain moves — never update
`idls/` to match upstream main alone.

Host prerequisites (the DGX box): rust + cargo, docker, python3, jq, git, `npx` (for
carbon-cli), and an Anchor 1.x CLI matching upstream's `Cargo.lock` (`build-upstream-idls.sh`
refuses a mismatched one and prints the `avm` command to fix it). No Alchemy key is needed
for any script here — everything uses the public devnet RPC.
