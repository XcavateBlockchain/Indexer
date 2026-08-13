# whitelist-grpc-api

A read-only gRPC API (`realxmarket.whitelist.v1.WhitelistService`) over the Postgres
database written by the SubQuery whitelist indexer. It serves role assignments,
admins, config, the raw action log, and indexer status, plus the standard
`grpc.health.v1.Health` service (SERVING iff the database answers `SELECT 1`,
cached for 5 seconds).

The server automatically detects SubQuery **historical indexing**: if the entity
tables have a `_block_range` column, every query is restricted to current rows
via `upper_inf(_block_range)`.

## Environment variables

| Variable              | Default     | Description                                    |
| --------------------- | ----------- | ---------------------------------------------- |
| `GRPC_HOST`           | `0.0.0.0`   | Bind address                                   |
| `GRPC_PORT`           | `50051`     | Bind port (also used by `dist/healthcheck.js`) |
| `DB_HOST`             | `localhost` | Postgres host                                  |
| `DB_PORT`             | `5432`      | Postgres port                                  |
| `DB_USER`             | `postgres`  | Postgres user                                  |
| `DB_PASS`             | *(empty)*   | Postgres password                              |
| `DB_DATABASE`         | `postgres`  | Database name                                  |
| `DB_SCHEMA`           | `app`       | Schema the SubQuery indexer writes to          |
| `DB_POOL_MAX`         | `10`        | Max pooled connections                         |
| `SHUTDOWN_TIMEOUT_MS` | `10000`     | Grace period before forced shutdown            |

No `.env` file support — provide the environment directly (Docker, compose, k8s).

## Run locally

```sh
npm install
npm run build        # generates proto types into src/generated, then tsc
DB_HOST=localhost DB_PASS=postgres npm start
```

The server starts even when the database is unreachable; it logs a warning and
the health service reports `NOT_SERVING` until the DB comes back. All logs are
single-line JSON on stdout (one line per RPC: method, ms, status code).

## Docker

```sh
docker build -t whitelist-grpc-api .
docker run --rm -p 50051:50051 \
  -e DB_HOST=host.docker.internal -e DB_PASS=postgres -e DB_SCHEMA=app \
  whitelist-grpc-api
```

The image includes a `HEALTHCHECK` that runs `node dist/healthcheck.js`
(exit 0 = SERVING, exit 1 otherwise).

## Calling it with grpcurl

Server **reflection is not implemented** (kept out deliberately to avoid extra
dependencies), so pass the proto files to `grpcurl` with `-import-path`/`-proto`.
Run these from the `grpc-api/` directory:

CheckAccess:

```sh
grpcurl -plaintext \
  -import-path ./proto -proto whitelist.proto \
  -d '{"user": "7fUAJdStEuGbc3sM84cKRL6yYaaSstyLSU4ve5oovLS7", "role": "ROLE_REGIONAL_OPERATOR"}' \
  localhost:50051 realxmarket.whitelist.v1.WhitelistService/CheckAccess
```

GetIndexerStatus:

```sh
grpcurl -plaintext \
  -import-path ./proto -proto whitelist.proto \
  -d '{}' \
  localhost:50051 realxmarket.whitelist.v1.WhitelistService/GetIndexerStatus
```

Health check:

```sh
grpcurl -plaintext \
  -import-path ./proto -proto health/v1/health.proto \
  -d '{"service": ""}' \
  localhost:50051 grpc.health.v1.Health/Check
```

Notes:

- Enum filter fields treat `*_UNSPECIFIED` (or leaving the field out) as
  "no filter"; list requests use proto3 `optional` so e.g. `"active": false`
  is a real filter, distinct from omitting it.
- `page_size` defaults to 50 and is capped at 500; `page` is 0-based.
- `GetRoleAssignment` returns soft-deleted (inactive) assignments too;
  `CheckAccess.has_role` is only true for an **active** assignment.

## Where the SQL lives

Every SQL string is in `src/db/queries.ts`. If the live SubQuery schema drifts
from the assumptions documented there (column names, enum spellings, table
names), that is the only file to adjust.
