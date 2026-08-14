# Deployment runbook (Hetzner)

The stack runs on a single Hetzner server via docker-compose. CI builds images; the server
only pulls and runs them.

## 1. One-time server preparation

Any Debian/Ubuntu Hetzner Cloud or dedicated server works. As root (or with sudo):

```bash
# Docker Engine + compose plugin (official convenience script)
curl -fsSL https://get.docker.com | sh

# Dedicated deploy user with docker rights, no sudo
adduser --disabled-password --gecos "" deploy
usermod -aG docker deploy

# Deploy directory owned by the deploy user
mkdir -p /opt/indexer
chown deploy:deploy /opt/indexer

# SSH key for GitHub Actions: generate a dedicated keypair (on your machine):
#   ssh-keygen -t ed25519 -f hetzner_deploy -C "gh-actions-indexer"
# then install the public half:
install -d -m 700 -o deploy -g deploy /home/deploy/.ssh
echo "<contents of hetzner_deploy.pub>" >> /home/deploy/.ssh/authorized_keys
chown deploy:deploy /home/deploy/.ssh/authorized_keys
chmod 600 /home/deploy/.ssh/authorized_keys
```

Firewall (ufw or Hetzner Cloud firewall): allow `22` (SSH) and `50051` (gRPC). Ports `3010`
(GraphQL playground) and `3011` (Grafana) are optional — leave them closed unless you want
them reachable from outside. Grafana at least has a password; the playground has none.
Postgres and Prometheus are never publicly reachable: Postgres has no published port, and
Prometheus is bound to `127.0.0.1` (reach it through an SSH tunnel, see §5). If a published
port is already taken on the server, move it with the `GRAPHQL_PORT`/`GRPC_PORT`/
`GRAFANA_PORT` repository variables below rather than editing the compose file.

## 2. GitHub repository secrets

`Settings → Secrets and variables → Actions`:

| Secret | Value |
|---|---|
| `HETZNER_HOST` | Server IP or hostname |
| `HETZNER_USER` | `deploy` |
| `HETZNER_SSH_KEY` | Contents of the **private** key (`hetzner_deploy`) |
| `HETZNER_SSH_PORT` | Optional, defaults to `22` |
| `HETZNER_KNOWN_HOSTS` | Optional but recommended: output of `ssh-keyscan -H <host>`, pins the host key (otherwise the workflow trusts-on-first-use) |
| `ALCHEMY_API_KEY` | Alchemy key with Solana Devnet enabled |
| `POSTGRES_PASSWORD` | Any strong password (stack-internal only) |
| `GRAFANA_PASSWORD` | Grafana admin password. **Required** — the deploy fails fast if unset, because Grafana is published on `GRAFANA_PORT` and would otherwise fall back to `admin`/`admin`. |
| `GHCR_PULL_TOKEN` | Optional: PAT with `read:packages`, only needed while the GHCR images are private. Alternatively make the three packages public and omit this. |

### Repository variables

Same page, **Variables** tab. All are optional and change only the *host* port that the
service is published on — the container-internal ports stay `3000`/`50051`/`9090`, so
healthchecks, scrape targets and inter-service URLs are unaffected. Use these when a port is
already taken on the server (`Bind for 0.0.0.0:3010 failed: port is already allocated`).

| Variable | Default | Effect |
|---|---|---|
| `GRAPHQL_PORT` | `3010` | Host port for the GraphQL playground |
| `GRPC_PORT` | `50051` | Host port for the gRPC API |
| `GRAFANA_PORT` | `3011` | Host port for Grafana |
| `PROMETHEUS_PORT` | `9090` | Host port for Prometheus, bound to `127.0.0.1` only — changing it only changes what an SSH tunnel targets |

Must be a bare port number in `1-65535`; the deploy fails fast with a clear error otherwise.
Changing `GRPC_PORT` means clients and the firewall rule must follow it.

## 3. Deploying

- **Automatic**: every push to `main` builds the images and deploys.
- **Manual**: `Actions → Deploy to Hetzner → Run workflow`.

What the workflow does:

1. Builds and pushes `ghcr.io/<repo>/node`, `ghcr.io/<repo>/grpc`, `ghcr.io/<repo>/postgres`,
   tagged `latest` + commit SHA.
2. Uploads `docker-compose.yml`, the `monitoring/` directory and a rendered `.env` (pinning
   the SHA-tagged images) to `/opt/indexer`. Unlike the application code, the Prometheus and
   Grafana configs are bind-mounted rather than baked into an image, so they have to be on
   the server; `monitoring/` is replaced wholesale on each deploy, so deleting a file from
   the repo also removes it from the server.
3. `docker compose pull && docker compose up -d --remove-orphans`, then verifies the node's
   `/ready` endpoint plus Prometheus and Grafana, and prunes old images.

**Rollback**: fastest is on the server — edit `.env` to point
`NODE_IMAGE`/`GRPC_IMAGE`/`PG_IMAGE` at an earlier SHA tag and `docker compose up -d`
(images from the last 7 days are still present locally; older ones re-pull from GHCR).
Via GitHub: open the last good run of the Deploy workflow and choose **Re-run all jobs**
(`workflow_dispatch` cannot target an arbitrary commit), or `git revert` and push.

## 4. Verifying a deployment

```bash
ssh deploy@<host>
cd /opt/indexer
docker compose ps                       # all six services Up, node+postgres healthy
docker compose logs -f subquery-node    # should show blocks being processed
docker compose exec postgres psql -U postgres \
  -c "select key, value from app._metadata where key in ('lastProcessedHeight','targetHeight');"
```

From anywhere (gRPC):

```bash
grpcurl -plaintext -proto grpc-api/proto/whitelist.proto \
  <host>:50051 realxmarket.whitelist.v1.WhitelistService/GetIndexerStatus
```

`lag_slots` should shrink toward ~0 as the indexer catches up with the devnet tip.

## 5. Operations

### Reindex from scratch (e.g. after a devnet ledger reset, or a schema change)

```bash
cd /opt/indexer
docker compose down
docker volume rm indexer_pgdata   # check the name: docker volume ls
docker compose up -d
```

History is small (indexing starts at the deploy slot), so a full reindex is quick — the bulk
of the time is walking empty devnet slots between the program's activity and the tip.

### Upgrading SubQuery images

Bump the pinned tags (`subquerynetwork/subql-node-solana:v6.3.1` in `docker/node.Dockerfile`,
`subquerynetwork/subql-query:v2.25.0` in `docker-compose.yml`), push to `main`, and let CI
deploy. Schema-affecting changes to `schema.graphql` require a reindex (above). The
monitoring images (`prom/prometheus`, `grafana/grafana`) are pinned in `docker-compose.yml`
the same way and can be bumped independently.

### Rotating POSTGRES_PASSWORD

The `POSTGRES_PASSWORD` env var only takes effect when the `pgdata` volume is first
initialized — changing the GitHub secret alone will brick the stack on the next deploy
(postgres keeps the old password, every other service gets the new one). Rotate in this
order:

```bash
ssh deploy@<host>
cd /opt/indexer
docker compose exec postgres psql -U postgres -c "ALTER USER postgres PASSWORD '<new>';"
```

then update the `POSTGRES_PASSWORD` GitHub secret and re-deploy. (Alternatively, wipe the
volume and reindex — history is small.)

### Disk/log hygiene

Container logs are capped (20 MB × 3 files per service) and the deploy workflow removes
images unused for 7+ days after every run (recent SHA tags stay available for rollback).
Postgres data lives in the `pgdata` named volume; Prometheus keeps 15 days of metrics in
`promdata` (`--storage.tsdb.retention.time`), and Grafana's own state lives in `grafanadata`.

### Monitoring

**Grafana** — `http://<host>:3011`, user `admin`, password = the `GRAFANA_PASSWORD` secret.
The Prometheus datasource and the "Indexer health" dashboard are provisioned from
`monitoring/` on every start, so a recreated container comes back with them already loaded.
Panels edited in the UI live only in the `grafanadata` volume — export the JSON and commit it
over `monitoring/grafana/dashboards/indexer-health.json` to make a change survive. With 3011
closed in the firewall (the recommended default), tunnel it:

```bash
ssh -L 3011:localhost:3011 deploy@<host>    # then open http://localhost:3011
```

**Prometheus** — bound to `127.0.0.1:9090` on the server, never publicly reachable, so it
always needs a tunnel:

```bash
ssh -L 9090:localhost:9090 deploy@<host>    # then open http://localhost:9090
```

To check the scrape is healthy without a tunnel, ask from inside the network:

```bash
cd /opt/indexer
docker compose exec -T subquery-node curl -s http://prometheus:9090/api/v1/targets \
  | grep -o '"health":"[a-z]*"'             # expect "health":"up"
```

A `down` target usually just means the node container is still booting — it serves `/metrics`
on the same internal port 3000 as `/ready`.

- gRPC `GetIndexerStatus` exposes `last_processed_slot`, `chain_head_slot`, `lag_slots`,
  `healthy` — poll it from any uptime monitor with a gRPC probe, or check
  the standard gRPC health service (`grpc.health.v1.Health/Check`).
- The node's own HTTP endpoints (`:3000/ready`, `/health`, `/meta` inside the network) are
  used by the compose healthcheck.
