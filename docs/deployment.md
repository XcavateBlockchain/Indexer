# Deployment runbook (Hetzner)

The stack runs on a single Hetzner server via docker-compose. CI builds images; the server
only pulls and runs them. This covers server setup and deploy mechanics; for day-2 operations
(reading the slot-lag panel, backfills, alerts, rolling back) see
[../RUNBOOK.md](../RUNBOOK.md).

**Migration note**: this doc covers the active Carbon/Rust stack (`docker-compose.yml`:
postgres, indexer, api, prometheus, grafana). The previous SubQuery-based stack
(`docker-compose.subquery.yml`) is preserved as a rollback path — see
[../DECISIONS.md ADR-21](../DECISIONS.md#adr-21-subquery-rollback-path-preserved) and
[../RUNBOOK.md](../RUNBOOK.md#rolling-back-to-subquery). Sections below that apply only to
that rollback stack are marked as such.

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

Firewall (ufw or Hetzner Cloud firewall): allow `22` (SSH). Ports `3010` (GraphQL +
GraphiQL) and `3011` (Grafana) are optional — leave them closed unless you want them
reachable from outside; Grafana at least has a password, GraphiQL has none (it's protected
by the API's own DoS guards, not a login — see `crates/api/src/guards.rs`). Postgres and
Prometheus are never publicly reachable: Postgres has no published port, and Prometheus is
bound to `127.0.0.1` (reach it through an SSH tunnel, see §5). The old stack additionally
needed `50051` (the SubQuery-era gRPC API) — no longer relevant to the active stack; only
open it if you've rolled back to `docker-compose.subquery.yml`. If a published port is
already taken on the server, move it with the `GRAPHQL_PORT`/`GRAFANA_PORT` repository
variables below rather than editing the compose file.

## 2. GitHub repository secrets

`Settings → Secrets and variables → Actions`:

| Secret | Value |
|---|---|
| `HETZNER_HOST` | Server IP or hostname |
| `HETZNER_USER` | `deploy` |
| `HETZNER_SSH_KEY` | Contents of the **private** key (`hetzner_deploy`) |
| `HETZNER_SSH_PORT` | Optional, defaults to `22` |
| `HETZNER_KNOWN_HOSTS` | Optional but recommended: output of `ssh-keyscan -H <host>`, pins the host key (otherwise the workflow trusts-on-first-use) |
| `ALCHEMY_API_KEY` | Alchemy key with Solana Devnet enabled (Yellowstone gRPC on a paid plan — see `MIGRATION_LOG.md`'s Phase 0 recon) |
| `POSTGRES_PASSWORD` | Any strong password (stack-internal only) |
| `GRAFANA_PASSWORD` | Grafana admin password. **Required** — the deploy fails fast if unset, because Grafana is published on `GRAFANA_PORT` and would otherwise fall back to `admin`/`admin`. |
| `GHCR_PULL_TOKEN` | Optional: PAT with `read:packages`, only needed while the GHCR images are private. Alternatively make the packages public and omit this. |

Unchanged from the pre-migration stack — this migration introduced no new secrets (verified
in `task-8-report.md`'s secrets accounting).

### Repository variables

Same page, **Variables** tab. All are optional and change only the *host* port that the
service is published on — the container-internal ports (`3010`, `3000`, `9090`) stay fixed,
so healthchecks, scrape targets and inter-service URLs are unaffected. Use these when a port
is already taken on the server (`Bind for 0.0.0.0:3010 failed: port is already allocated`).

| Variable | Default | Effect |
|---|---|---|
| `GRAPHQL_PORT` | `3010` | Host port for GraphQL + GraphiQL |
| `GRAFANA_PORT` | `3011` | Host port for Grafana |
| `PROMETHEUS_PORT` | `9090` | Host port for Prometheus, bound to `127.0.0.1` only — changing it only changes what an SSH tunnel targets |

Must be a bare port number in `1-65535`; the deploy fails fast with a clear error otherwise.

`GRPC_PORT` (the old stack's gRPC API port variable) is no longer read by the rendering
script — safe to delete from repo Settings → Variables whenever convenient; leaving it in
place does not fail the workflow.

## 3. Deploying

- **Automatic**: every push to `main` builds the images and deploys.
- **Manual**: `Actions → Deploy to Hetzner → Run workflow`.

What the workflow does:

1. Builds and pushes `ghcr.io/<repo>/indexer`, `ghcr.io/<repo>/api`, `ghcr.io/<repo>/postgres`,
   tagged `latest` + commit SHA (`docker/rust.Dockerfile`'s `indexer`/`api` targets, plus
   `docker/pg-Dockerfile`, unchanged).
2. Uploads `docker-compose.yml`, the `monitoring/` directory and a rendered `.env` (pinning
   the SHA-tagged images) to `/opt/indexer`. Unlike the application code, the Prometheus and
   Grafana configs are bind-mounted rather than baked into an image, so they have to be on
   the server; `monitoring/` is replaced wholesale on each deploy, so deleting a file from
   the repo also removes it from the server.
3. `docker compose pull && docker compose up -d --remove-orphans`, then verifies `api`'s
   `/health` endpoint plus Prometheus and Grafana (checked in-network, via `docker compose
   exec api curl ...`), and prunes old images.

**Rollback (staying on the Carbon/Rust stack)**: fastest is on the server — edit `.env` to
point `INDEXER_IMAGE`/`API_IMAGE`/`PG_IMAGE` at an earlier SHA tag and `docker compose up -d`
(images from the last 7 days are still present locally; older ones re-pull from GHCR). Via
GitHub: open the last good run of the Deploy workflow and choose **Re-run all jobs**
(`workflow_dispatch` cannot target an arbitrary commit), or `git revert` and push.

Rolling back to an *older* SHA can also be the right move against a *newer* deploy: an
image built from newer `main` is not automatically better than an older one. The 2026-09-03
incident was exactly this — a fresh build's binary demanded a glibc newer than the pinned
runtime provided, so both `indexer` and `api` crash-looped at exec (`GLIBC_2.38' not
found`), and rolling the image tags back to the last good SHA was the immediate mitigation.
The build-side fix (builder stage rebuilt on the runtime's own base for
parity-by-construction) is recorded in `MIGRATION_LOG.md` (2026-09-03 glibc-parity entry);
the host-side procedure is `RUNBOOK.md` → `## Container crash-loops with a GLIBC error`.

**Rollback to the old SubQuery stack**: a different operation entirely — see
[../RUNBOOK.md](../RUNBOOK.md#rolling-back-to-subquery).

## 4. Verifying a deployment

```bash
ssh deploy@<host>
cd /opt/indexer
docker compose ps                # all 5 services Up, indexer+api+postgres healthy
docker compose logs -f indexer   # should show the subscribe gate passing, then
                                  # snapshot/backfill/reconciliation progress
curl -s http://localhost:3010/health
```

`/health` returns `{"last_contiguous_slot", "backfill_complete", "chain_tip_slot",
"slot_lag", "healthy"}` — `healthy: true` once the database is reachable, `backfill_complete:
true` once the initial history walk has finished (usually within a couple minutes at this
program's data volume — see `task-7-report.md`'s timed verification run). GraphQL is
equivalent for scripting against:

```bash
curl -s -X POST http://localhost:3010/graphql -H "Content-Type: application/json" \
  -d '{"query":"{ syncStatus { lastContiguousSlot chainTipSlot slotLag backfillComplete } }"}'
```

`slotLag` should shrink toward ~0 as the indexer catches up with the devnet tip — see
[../RUNBOOK.md "Is the indexer behind?"](../RUNBOOK.md#is-the-indexer-behind) if it doesn't.

## 5. Operations

### Reindex from scratch (e.g. after a devnet ledger reset, or a schema change)

```bash
cd /opt/indexer
docker compose down
docker volume rm indexer_pgdata   # check the name: docker volume ls
docker compose up -d
```

History is small (indexing starts at the deploy slot), so a full reindex is quick — see
[../RUNBOOK.md "Devnet ledger reset"](../RUNBOOK.md#devnet-ledger-reset) for the full
procedure and what triggers it (that section also spells out what the volume drop takes
with it). The derived property-metadata table (ADR-27) is wiped too and refills itself:
the live fetcher (or a one-shot `docker compose exec indexer indexer fetch-metadata`)
re-downloads the documents from the assets' live `metadata_uri`s.

Run this once right after the ADR-26 redeploy change deploys: the new binary indexes the
new program addresses correctly even against the old volume (`sync_state` keys by program
id, so the new programs seed and rebuild on their own; the first snapshot sweeps the old
addresses' state rows closed), but the wipe is what clears the abandoned deployments' dead
history and the reshaped tables' padding defaults.

### Upgrading images

Images are built and tagged by CI on every push to `main` — there's no manual version pin to
bump for the active `indexer`/`api`/`postgres` images the way the old stack's
`subquerynetwork/subql-node-solana`/`subql-query` tags had to be bumped by hand. Monitoring
images (`prom/prometheus`, `grafana/grafana`) are still pinned directly in
`docker-compose.yml` and bumped the same way as before: edit the tag, push to `main`, let CI
deploy.

Schema-affecting changes to the Postgres schema (a new `migrations/NNNN_*.sql` file) apply
automatically on the next `indexer` startup — `crates/indexer` runs `sqlx::migrate!()` at
launch, idempotently.

**Rollback-stack only**: bumping `subquerynetwork/subql-node-solana`/`subql-query` pins in
`docker/node.Dockerfile`/`docker-compose.subquery.yml` only matters if you've rolled back to
that stack — see [../RUNBOOK.md](../RUNBOOK.md#rolling-back-to-subquery).

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
volume and reindex — history is small.) Note this volume is shared with the SubQuery
rollback stack's `app` schema (`ADR-21`) — rotating the password here rotates it for both
stacks, since it's the same postgres instance either way.

### Disk/log hygiene

Container logs are capped (20 MB × 3 files per service) and the deploy workflow removes
images unused for 7+ days after every run (recent SHA tags stay available for rollback).
Postgres data lives in the `pgdata` named volume (shared with the SubQuery rollback stack,
see above); Prometheus keeps 15 days of metrics in `promdata`
(`--storage.tsdb.retention.time`), and Grafana's own state lives in `grafanadata`.

### Property image mirror (object storage)

The indexer mirrors marketplace `PropertyAssets` `propertyImages` URIs to object
storage as 720×720 centred-crop JPEGs and exposes the compressed URIs via
`PropertyMetadata.propertyImageThumbnails` (ADR-31). The feature is **off by default** —
it activates only when object-storage configuration is present.

**One-time Hetzner setup** (Object Storage, e.g. `fsn1`):

1. Create a **public-read bucket** (e.g. `indexer-property-images`) — thumbnails are
   served directly from the bucket URL to API consumers, no CDN in front.
2. In the project's access keys, create a key with a **write-only** bucket policy on
   that bucket (upload/delete only — the mirror never reads back through the API,
   public reads go through the bucket's anonymous policy).
3. Set the GitHub **repository secrets** (all five, all-or-nothing — see below):

   | Secret | Hetzner value |
   |---|---|
   | `OBJECT_STORAGE_ENDPOINT` | `https://<region>.<your-objectstorage-domain>`, e.g. `https://fsn1.your-objectstorage.com` — the domain is assigned per customer and shown in the Hetzner Console; **bare `scheme://host`**, no path, no bucket (the endpoint host must carry no port) |
   | `OBJECT_STORAGE_BUCKET` | the bucket name |
   | `OBJECT_STORAGE_REGION` | the bucket's location code — the first label of the endpoint host, e.g. `fsn1` or `nbg1` |
   | `OBJECT_STORAGE_ACCESS_KEY` | the access key's ID |
   | `OBJECT_STORAGE_SECRET_KEY` | the access key's secret |

   Optional **repository variables**: `OBJECT_STORAGE_PUBLIC_BASE_URL` (default:
   `{scheme}://{bucket}.{endpoint-host}`, derived virtual-hosted — correct for Hetzner;
   only override if a CDN/proxy fronts the bucket) and `IMAGE_MIRROR_INTERVAL`
   (default 30 seconds).
   `docker-compose.yml` passes all seven through; a fresh `git pull` +
   `docker compose up -d` on the server picks them up on the next deploy.

**All-or-nothing semantics**: with zero of the five set, the mirror is disabled (the
`property_images_pending` gauge is absent in Prometheus — that absence is the
dashboard's "disabled" signal). With *some* set and any missing, the indexer fails
fast at startup naming the missing variables — there is no half-configured state.

**First boot / manual catch-up**: the running mirror drains pending work automatically
(≤50 images per 30 s cycle, per-image backoff 30 s → 1 h on failure), but for a big
initial backfill run the one-shot subcommand — it exits once the work set is empty and
refuses to run without object-storage config:

```bash
docker compose exec -T indexer indexer mirror-images
```

Forced retries (e.g. after fixing an upstream URI 404) are a SQL reset:

```bash
docker compose exec -T postgres psql -U postgres -c \
  "UPDATE marketplace_property_image SET next_attempt_at = now() WHERE last_error IS NOT NULL;"
```

**Verifying**: `docker compose logs -f indexer | grep -i 'image mirror'` (per-cycle
`N uploaded, N failed, M pending` lines, never the key), or `GET <public base URL>/
properties/<assetPubkey>/<i>/<sha256-hex(uri)>.jpg`, or
`propertyAsset(assetId: …) { propertyImageThumbnails }` via the API. Failing uploads
alert after 15 minutes (`PropertyImageMirrorFailing`) — see the RUNBOOK alert list.

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
docker compose exec -T api curl -s http://prometheus:9090/api/v1/targets \
  | grep -o '"health":"[a-z]*"'             # expect two "health":"up" (indexer + api jobs)
```

A `down` target usually just means the `indexer`/`api` container is still booting — both
expose `/metrics` as soon as they're up (`indexer:9464`, `api:9465`).

- `curl http://localhost:3010/health` (or the `syncStatus` GraphQL field) exposes
  `last_contiguous_slot`, `chain_tip_slot`, `slot_lag`, `healthy` — poll it from any uptime
  monitor.
- See [../RUNBOOK.md "Alert list"](../RUNBOOK.md#alert-list) for what each of the eight
  Prometheus alerting rules (`monitoring/alerts.yml`) means when it fires — rules only, no
  Alertmanager (`ADR-20`), so nothing pages anyone automatically; check Prometheus's
  `/alerts` page or Grafana's alerting view.

### Post-upgrade backfill re-walk

After an on-chain program upgrade (the `ProgramUpgradeDetected` alert / RUNBOOK "After a
program upgrade"), once the updated images are deployed, heal the window between the
upgrade slot and the new decoder going live with a full history re-walk on the server:

```bash
cd /opt/indexer && docker compose exec -T indexer indexer backfill
```

Safe at any time: every write is idempotent (`ON CONFLICT DO NOTHING` / slot-guarded), so
the re-walk only adds what was missed. At current devnet volume it finishes in about a
minute.

### Mainnet (placeholder)

Nothing is deployed on mainnet, and this whole document is the **devnet** production
deployment. The intended mainnet shape (own database and deploy target, own
`addresses.mainnet.json` — a placeholder exists at the repo root — promotion only after
devnet verification) is sketched in
[agentic-maintenance.md §8](agentic-maintenance.md#8-mainnet-placeholder); nothing below
the placeholder exists yet, on purpose.
