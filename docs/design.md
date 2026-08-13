# realXmarket Whitelist Indexer — Design

**Date:** 2026-08-13
**Scope:** Index the `xcavate-whitelist` Solana program (devnet) with SubQuery; expose the data over gRPC (and GraphQL); dockerize; deploy to Hetzner via GitHub Actions.

## 1. Goal

Provide a queryable, always-up-to-date view of the realXmarket role/compliance registry:

- Who holds which role (`RegionalOperator`, `RealEstateInvestor`, `RealEstateDeveloper`, `Lawyer`, `LettingAgent`, `SpvConfirmation`)
- Whether each assignment is KYC-compliant (`Compliant` / `Revoked`)
- Who the whitelist admins and the sudo authority are
- A full audit trail of every whitelist action

Source of truth: program `2vVARM46pPD4rcHdbXHnYA4vTGN14q6skQAzsQWcHUxn` on **Solana devnet**
(from `addresses.json`; deployed at slot **483,386,556**).

## 2. Key decision: where gRPC fits

The request was "an indexer using gRPC and SubQuery". These two meet at different layers:

- **Ingestion** — SubQuery's Solana node (`@subql/node-solana`) consumes **JSON-RPC only**
  (`getSlot`/`getBlock` polling via `@solana/kit`). It cannot consume Yellowstone/Geyser gRPC
  streams, and Alchemy's Solana gRPC offering is therefore not usable by SubQuery. Ingestion
  uses the **Alchemy JSON-RPC devnet endpoint** (`https://solana-devnet.g.alchemy.com/v2/<key>`,
  key read from `.env`).
- **Serving** — gRPC is delivered as the **query API layer**: a small TypeScript gRPC service
  (`grpc-api/`) that reads the SubQuery-populated Postgres schema and answers typed queries
  (e.g. `CheckAccess(user, role)`). SubQuery's stock GraphQL playground stays available as a
  secondary read path.

## 3. Architecture

```
Solana devnet (Alchemy JSON-RPC) ──poll──► subql-node-solana ──write──► Postgres (schema "app")
                                                                          │            │
                                                       subql-query (GraphQL :3000)     │
                                                                                grpc-api (gRPC :50051)
```

Four containers via docker-compose on a single Hetzner host:

| Service        | Image                                        | Role                             |
|----------------|----------------------------------------------|----------------------------------|
| `postgres`     | postgres:16 + btree_gist                     | storage                          |
| `subquery-node`| baked: subql-node-solana:v6.3.1 + this repo  | indexer                          |
| `graphql-engine`| subquerynetwork/subql-query:v2.25.0         | GraphQL API (internal/optional)  |
| `grpc-api`     | baked: node:22 + grpc-api/                   | public gRPC API                  |

The indexer project is **baked into an image** (not volume-mounted) so a deploy is an atomic
image swap: CI builds the node, gRPC and postgres images, pushes to GHCR, the server pulls.

## 4. Program interface (derived from source, verified on-chain)

No IDL is committed in `realxmarket-solana` and none is published on-chain, so this repo
**hand-authors** `idls/xcavate_whitelist.idl.json` (Anchor 1.0 / spec `0.1.0` format), which
`@subql/common-solana` codegen converts internally via Codama into typed instruction decoders.

Discriminators were computed (`sha256("global:<name>")[0..8]`) and verified against a live
devnet transaction (`assign_role` = `ffae7db4cb9bca83`, arg `0x04` = LettingAgent — matches the
`letting_agent` wallet in `addresses.json`).

Nine instructions are indexed, each with its own handler:

| Instruction         | Effect indexed                                                        |
|---------------------|-----------------------------------------------------------------------|
| `initialize_config` | Creates singleton `Config`; sets sudo authority                       |
| `update_authority`  | Proposes pending authority (two-step handover; re-propose overwrites) |
| `accept_authority`  | Completes handover                                                    |
| `add_admin`         | Creates `Admin` PDA `["admin", key]`                                  |
| `remove_admin`      | Closes `Admin` PDA (arg `admin_key` identifies the admin)             |
| `assign_role`       | Creates `RoleAccount` `["role", user, role_byte]`, permission=Compliant |
| `remove_role`       | Closes `RoleAccount` (admin-initiated)                                |
| `renounce_role`     | Closes `RoleAccount` (holder-initiated; same on-chain event as remove — disambiguated by instruction) |
| `set_permission`    | Flips `Compliant`/`Revoked` on an assignment                          |

Enum orders are load-bearing (borsh stores variant indices): `Role` variants 0–5 in declaration
order (seed byte == variant index), `AccessPermission` 0=Compliant, 1=Revoked.

Indexing is **instruction-based** (not event-log-based): instruction data + account lists are
never truncated, whereas Solana log messages can be. Anchor events exist in logs but are only
a redundant signal here — every event's payload is recoverable from the instruction itself.

## 5. Data model (schema.graphql)

- **`Config`** (id = `"config"`): `authority`, `pendingAuthority?`, last-update metadata.
- **`Admin`** (id = admin pubkey): `active`, `addedBy`, added/removed block+time+tx. Removal
  marks `active=false` rather than deleting — history preserved.
- **`RoleAssignment`** (id = `<user>-<roleByte>`): `user`, `role`, `permission`, `active`,
  `rentPayer`, `assignedBy`, assignment/update/removal metadata, `removalKind`
  (`REMOVED`/`RENOUNCED`). Same soft-delete approach; a re-assignment after removal reuses the
  id (PDA semantics) and resets the audit fields — full history lives in `WhitelistAction`.
- **`WhitelistAction`** (id = `<txSig>-<ixPath>`): append-only audit log — one row per indexed
  instruction: `type`, `subject?` (affected address), `role?`, `permission?`, `actor` (signer),
  `blockHeight`, `blockTime`, `txSignature`. Indexed on `subject`, `type`, `actor`, `txSignature`.

Block coordinates in entities are Solana **block heights** (what the indexer runtime exposes
per transaction), not slots; slots appear only in indexer-progress metadata. The two axes
differ by millions on devnet (skipped slots) — `txSignature` is the canonical cross-reference
to the chain.

## 6. gRPC API (grpc-api/)

`@grpc/grpc-js` + `@grpc/proto-loader` with `proto-loader-gen-types` for compile-time typing.
(ts-proto was considered; proto-loader keeps the toolchain pure-JS — no protoc binary — which
matters for a small cross-platform repo. Documented tradeoff.)

`proto/whitelist.proto` — service `WhitelistService`:

- `CheckAccess(user, role) → {has_role, compliant}` — the primary integration query
- `GetConfig() → {authority, pending_authority}`
- `GetRoleAssignment(user, role)` / `ListRoleAssignments(filters, paging)`
- `ListAdmins(active_only, paging)`
- `ListActions(filters, paging)` — audit trail
- `GetIndexerStatus() → {last_processed_slot, chain_head_slot?, healthy}` — reads SubQuery's
  `_metadata` table so consumers can detect lag
- standard gRPC health service (`grpc.health.v1.Health`)

Reads Postgres directly (`pg` pool, read-only queries against schema `app`).

## 7. Ops & deployment

- **Config**: single `.env` (gitignored): `ALCHEMY_API_KEY`, `POSTGRES_PASSWORD`. Compose
  interpolates the Alchemy endpoint into the node's `--network-endpoint` flag at runtime, so
  the key is neither committed nor baked into images. `.env.example` documents every variable.
- **startBlock** = 483,386,556 (deploy slot — full program history).
- **CI/CD** (`.github/workflows/`):
  - `ci.yml` — PRs: codegen + build both packages.
  - `deploy.yml` — push to `main` (+ manual dispatch): build & push all three images (node, grpc, postgres) to GHCR →
    ssh to Hetzner → upload compose file → `docker compose pull && up -d`. Secrets:
    `HETZNER_HOST`, `HETZNER_USER`, `HETZNER_SSH_KEY`, `ALCHEMY_API_KEY`, `POSTGRES_PASSWORD`.
- **Known risks** (documented in README):
  - Alchemy free tier (30M CU/month, 500 CU/s) vs. devnet's ~2.5 slots/s of `getBlock` polling —
    batch size/workers are tuned to outpace block production while staying under the CU/s cap;
    public devnet RPC is configured as a fallback endpoint. Sustained tail-following costs
    ~5–7M CU/day, so long-term either a paid tier or public-RPC-primary is advised.
  - Devnet ledger resets can orphan history; recovery = wipe DB volume and reindex (minutes,
    given the program's small history).
  - `--unfinalized-blocks=true` handles short reorgs.

## 8. Out of scope (for now)

- The other three programs (`regions`, `marketplace`, `property`) — the schema and layout
  deliberately leave room (per-program IDL files, one datasource each) to add them later.
- Mainnet: switch `chainId` + endpoint + `startBlock` when the program deploys there.
- Push-based ingestion (Yellowstone gRPC) — would require replacing SubQuery, not extending it.
