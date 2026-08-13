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
mkdir -p /opt/whitelist-indexer
chown deploy:deploy /opt/whitelist-indexer

# SSH key for GitHub Actions: generate a dedicated keypair (on your machine):
#   ssh-keygen -t ed25519 -f hetzner_deploy -C "gh-actions-whitelist-indexer"
# then install the public half:
install -d -m 700 -o deploy -g deploy /home/deploy/.ssh
echo "<contents of hetzner_deploy.pub>" >> /home/deploy/.ssh/authorized_keys
chown deploy:deploy /home/deploy/.ssh/authorized_keys
chmod 600 /home/deploy/.ssh/authorized_keys
```

Firewall (ufw or Hetzner Cloud firewall): allow `22` (SSH) and `50051` (gRPC). Port `3000`
(GraphQL playground) is optional — leave it closed unless you want the playground public;
Postgres is never exposed (it has no published port).

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
| `GHCR_PULL_TOKEN` | Optional: PAT with `read:packages`, only needed while the GHCR images are private. Alternatively make the three packages public and omit this. |

## 3. Deploying

- **Automatic**: every push to `main` builds the images and deploys.
- **Manual**: `Actions → Deploy to Hetzner → Run workflow`.

What the workflow does:

1. Builds and pushes `ghcr.io/<repo>/node`, `ghcr.io/<repo>/grpc`, `ghcr.io/<repo>/postgres`,
   tagged `latest` + commit SHA.
2. Uploads `docker-compose.yml` and a rendered `.env` (pinning the SHA-tagged images) to
   `/opt/whitelist-indexer`.
3. `docker compose pull && docker compose up -d --remove-orphans`, then verifies the node's
   `/ready` endpoint and prunes old images.

**Rollback**: fastest is on the server — edit `.env` to point
`NODE_IMAGE`/`GRPC_IMAGE`/`PG_IMAGE` at an earlier SHA tag and `docker compose up -d`
(images from the last 7 days are still present locally; older ones re-pull from GHCR).
Via GitHub: open the last good run of the Deploy workflow and choose **Re-run all jobs**
(`workflow_dispatch` cannot target an arbitrary commit), or `git revert` and push.

## 4. Verifying a deployment

```bash
ssh deploy@<host>
cd /opt/whitelist-indexer
docker compose ps                       # all four services Up, node+postgres healthy
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
cd /opt/whitelist-indexer
docker compose down
docker volume rm whitelist-indexer_pgdata   # check the name: docker volume ls
docker compose up -d
```

History is small (indexing starts at the deploy slot), so a full reindex is quick — the bulk
of the time is walking empty devnet slots between the program's activity and the tip.

### Upgrading SubQuery images

Bump the pinned tags (`subquerynetwork/subql-node-solana:v6.3.1` in `docker/node.Dockerfile`,
`subquerynetwork/subql-query:v2.25.0` in `docker-compose.yml`), push to `main`, and let CI
deploy. Schema-affecting changes to `schema.graphql` require a reindex (above).

### Rotating POSTGRES_PASSWORD

The `POSTGRES_PASSWORD` env var only takes effect when the `pgdata` volume is first
initialized — changing the GitHub secret alone will brick the stack on the next deploy
(postgres keeps the old password, every other service gets the new one). Rotate in this
order:

```bash
ssh deploy@<host>
cd /opt/whitelist-indexer
docker compose exec postgres psql -U postgres -c "ALTER USER postgres PASSWORD '<new>';"
```

then update the `POSTGRES_PASSWORD` GitHub secret and re-deploy. (Alternatively, wipe the
volume and reindex — history is small.)

### Disk/log hygiene

Container logs are capped (20 MB × 3 files per service) and the deploy workflow removes
images unused for 7+ days after every run (recent SHA tags stay available for rollback).
Postgres data lives in the `pgdata` named volume.

### Monitoring

- gRPC `GetIndexerStatus` exposes `last_processed_slot`, `chain_head_slot`, `lag_slots`,
  `healthy` — poll it from any uptime monitor with a gRPC probe, or check
  the standard gRPC health service (`grpc.health.v1.Health/Check`).
- The node's own HTTP endpoints (`:3000/ready`, `/health`, `/meta` inside the network) are
  used by the compose healthcheck.
