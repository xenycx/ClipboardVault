# VPS deployment guide

This guide installs Clipboard Vault 3.x on a fresh Ubuntu 24.04 VPS with Docker Compose.
Replace `vault.example.com` with your real domain.

3.x runs as one container (`vault`) with one volume mounted at `/data`, optionally behind the
bundled Nginx for HTTPS. If the server already runs Clipboard Vault 2.x (PostgreSQL plus a
separate auth service), do not follow the install steps over it: go to
[Moving from 2.x to 3.0](#moving-from-2x-to-30).

## 1. Prepare DNS

Create an A record:

| Name | Value |
| --- | --- |
| `vault` | Your VPS IPv4 address |

Add an AAAA record only if IPv6 is correctly configured. Wait until
`vault.example.com` resolves to the VPS before requesting a certificate.

## 2. Secure the server

Log in using an account with sudo access:

```bash
sudo apt update
sudo apt upgrade -y
sudo apt install -y ca-certificates curl git ufw
sudo ufw allow OpenSSH
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp
sudo ufw enable
```

Use SSH keys, disable password SSH login after confirming key access, and keep the operating
system patched.

## 3. Install Docker

Install Docker Engine 25 or newer and Docker Compose 2.20.2 or newer from Docker's official
Ubuntu repository. The startup health probes and guarded upgrade command require those
minimum versions. Verify:

```bash
docker --version
docker compose version
```

Add the deployment user to the Docker group only if you accept that this grants root-equivalent
control. Otherwise prefix Docker commands with sudo.

## 4. Download and configure Clipboard Vault

```bash
git clone <your-repository-url> clipboard-vault
cd clipboard-vault
cp .env.example .env
chmod 600 .env
```

Edit `.env`:

- `DOMAIN`: the hostname only, such as `vault.example.com`.
- `PUBLIC_BASE_URL`: `https://vault.example.com`. It must match the address in the browser
  exactly, because sign-in requests are refused when their `Origin` differs.
- `VAULT_SECRET` and `SETUP_TOKEN`: two different random values of at least 32 characters.
- A complete client ID/secret pair for every Google, GitHub, or Discord provider you enable.
- `BACKUP_KEEP` if seven daily database snapshots is the wrong number (`0` disables them).
- Upload limits if the 10 GiB deployment ceiling, 100 MiB workspace default, 16 MiB Tus
  chunks, seven-day session expiry, or 20 GiB disk reserve are unsuitable.

`SERVER_MAX_UPLOAD_BYTES` is the absolute per-file ceiling. Keep
`LEGACY_MAX_UPLOAD_BYTES` at 100 MiB for the multipart compatibility endpoint and use Tus for
larger files. `UPLOAD_DISK_RESERVE_BYTES` protects the VPS from being filled by uploads; the
data volume must have more free space than the reserve plus the file being uploaded.

Generate each secret separately:

```bash
openssl rand -hex 32
```

Everything the application writes goes to the Compose volume `vault-data` (Docker names it
`clipboard-vault_vault-data`), mounted at `/data`:

| Path | Contents |
| --- | --- |
| `/data/vault.db` (plus `vault.db-wal`, `vault.db-shm`) | The live SQLite database: accounts, sessions, API keys, workspaces, items, blob metadata, upload sessions, activity |
| `/data/uploads/` | Uploaded file bytes |
| `/data/backups/` | Daily `vault-YYYY-MM-DD.db` snapshots and `pre-upgrade-*.db` snapshots |

The container runs as uid 10001 with a read-only root filesystem and a `/tmp` tmpfs. The
volume must stay writable by that user.

## 5. Configure social providers

Providers with two blank variables are disabled and hidden from the login page. Setting only
one value in a pair is a configuration error: the vault refuses to start, and
`./scripts/upgrade.sh --check` fails.

Register the callback for each enabled provider exactly:

```text
https://vault.example.com/api/auth/callback/google
https://vault.example.com/api/auth/callback/github
https://vault.example.com/api/auth/callback/discord
```

Set the matching pair in `.env`:

```dotenv
GOOGLE_CLIENT_ID=
GOOGLE_CLIENT_SECRET=
GITHUB_CLIENT_ID=
GITHUB_CLIENT_SECRET=
DISCORD_CLIENT_ID=
DISCORD_CLIENT_SECRET=
```

Google needs an OAuth Web application. A GitHub OAuth App must request `user:email`; a
GitHub App also needs read-only access to Email addresses. Discord uses only the `identify`
and `email` scopes and does not need bot permissions. Discord phone-only accounts and GitHub
accounts that return no verified email cannot sign in; users see a safe message and no
placeholder identity is created.

A provider is linked to an existing user only when that provider verifies the same email.
Do not paste any provider secret into GitHub issues, screenshots, or chat.

## 6. Start HTTP and issue HTTPS

```bash
chmod +x scripts/*.sh
./scripts/init-https.sh
```

The script:

1. Starts the vault and a temporary HTTP Nginx.
2. Uses the ACME webroot to request a Let’s Encrypt certificate.
3. Restarts Nginx with the HTTPS configuration.

Nginx forwards every path, including `/api/auth/`, to `vault:8080` and appends the client
address to `X-Forwarded-For`. That is why the Compose file sets `TRUST_PROXY_HEADERS=true`.

Add certificate renewal to root’s crontab:

```cron
17 3 * * * cd /opt/clipboard-vault && ./scripts/renew-certificates.sh >> /var/log/clipboard-vault-certbot.log 2>&1
```

Adjust the repository path.

### Behind another reverse proxy

If the host already has a proxy that terminates TLS (Traefik, Caddy, a panel), you do not need
the bundled Nginx and certbot. Drop those services, publish the vault port to that proxy, and
set `PUBLIC_BASE_URL` to the public HTTPS address. Set `TRUST_PROXY_HEADERS=true` only if that
proxy appends the real client address to `X-Forwarded-For`; otherwise leave it `false`, or
clients can choose their own rate-limit address. Make sure the proxy allows request bodies of
at least `TUS_CHUNK_SIZE_BYTES` plus overhead on `/api/v1/uploads` and `LEGACY_MAX_UPLOAD_BYTES`
on `/api/v1/items`, and does not buffer whole uploads.

## 7. Claim the first administrator

1. Open `https://vault.example.com/login`.
2. Register the intended administrator with an enabled social provider or password.
3. Open `https://vault.example.com/setup`.
4. Enter `SETUP_TOKEN` from the VPS `.env`.
5. Confirm the account opens its personal workspace and the Accounts page.

The claim endpoint refuses additional attempts after one global admin exists. The server still
requires `SETUP_TOKEN` to be set, so if you rotate it afterward, put a new random value in
`.env` and recreate the vault:

```bash
docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d vault
```

## 8. Verify the interface

Every page is rendered by the server, so a bad deployment is visible in a browser
immediately. Walk this list after the first start and after every upgrade.

1. Open `/login`. The split sign-in screen shows the product panel beside the card, and the
   page must not flash the wrong theme while loading.
2. Sign in and open the vault. The sidebar, workspace switcher, and Paste/Upload composer
   render, and the top bar shows the search button.
3. Press `Ctrl K` (`Cmd K` on macOS). The command palette opens and lists navigation commands.
4. Save a short paste. It appears as a card showing its type, size, and age.
5. Upload a file larger than one chunk and confirm progress, pause, and resume.
6. Delete that item, open Trash in the sidebar, and restore it.
7. As a workspace owner or admin, open Storage. The capacity gauge and the sidebar disk meter
   show real numbers.
8. Switch the theme in the top bar, reload, and confirm the choice survives.

Then confirm the browser is being sent the current assets:

```bash
curl -sI https://vault.example.com/static/app.css | grep -i cache-control
curl -s https://vault.example.com/login | grep -o "/static/app.css?v=[a-f0-9]*"
```

The stylesheet must return `cache-control: public, max-age=31536000, immutable`, and the page
must reference a `?v=` fingerprint. That fingerprint is a hash of everything in `static/`,
computed when the container starts. Because it changes with the assets, a browser never pairs
freshly rendered markup with a cached copy of the previous stylesheet, and unchanged assets
stay cached across restarts. Rendered pages themselves are sent with `cache-control: no-store`
so workspace content is not written to a shared or on-disk cache.

## Operations

### Status and logs

```bash
docker compose -f docker-compose.yml -f docker-compose.prod.yml ps
docker compose -f docker-compose.yml -f docker-compose.prod.yml logs --tail=200 vault nginx
curl -fsS https://vault.example.com/health/live
curl -fsS https://vault.example.com/health/ready
```

`/health/live` reports that the process is up; `/health/ready` also queries the database. The
runtime image has no shell or curl. Its Docker health check runs the binary itself
(`clipboard-vault healthcheck`), and you can do the same:

```bash
docker compose -f docker-compose.yml -f docker-compose.prod.yml exec vault /usr/local/bin/clipboard-vault healthcheck
```

### Storage pressure and cleanup

The application reserves 20 GiB of free space by default. If an upload would cross that
boundary it returns `507 Insufficient Storage`; login, previews, downloads, and cleanup remain
available. The reserve is measured on the filesystem under `/data`, which also holds the
database and its snapshots. Do not reduce it without accounting for those, container images,
logs, and operating-system updates on the same filesystem.

Workspace owners and admins can review reserved bytes plus the oldest and largest items in
the storage screen. Purging requires explicit selection and workspace-name confirmation.
No age-based or low-space process deletes vault data automatically.

Permanent purge is irreversible. A code rollback cannot recover purged rows or file bytes, and
restoring a database snapshot brings back the rows but not the deleted bytes.

### Backups

Copying a live SQLite file can capture it mid-write, so never treat a raw copy of `vault.db`
as a backup. The vault writes consistent snapshots instead:

- **Daily.** Once a day the server writes `/data/backups/vault-YYYY-MM-DD.db` with
  `VACUUM INTO` and keeps the newest `BACKUP_KEEP` (default 7; `0` disables snapshots).
- **On demand, in the browser.** On the Admin page, **Download database snapshot** (or
  `https://vault.example.com/admin/backup`) gives a global administrator a fresh snapshot.
- **On demand, on the server.**

  ```bash
  docker compose -f docker-compose.yml -f docker-compose.prod.yml exec vault \
    /usr/local/bin/clipboard-vault backup /data/backups/manual-$(date -u +%Y%m%dT%H%M%SZ).db
  ```

  Only files named `vault-*.db` are pruned automatically. `manual-*` and `pre-upgrade-*`
  snapshots stay until you delete them.

Uploaded files are not inside the database. A complete off-server backup is the `backups/`
and `uploads/` folders of the data volume, for example:

```bash
docker run --rm -v clipboard-vault_vault-data:/data:ro -v "$PWD/backups":/out alpine \
  tar -C /data -czf /out/vault-data-$(date -u +%Y%m%d).tgz backups uploads
```

Any volume-level backup of `/data` also works, as long as you restore from a snapshot in
`backups/` rather than from the `vault.db` it happened to capture.

### Restore from a snapshot

Restoring replaces every row (accounts, sessions, keys, items) with the snapshot's state.
Files uploaded after the snapshot stay on disk but are no longer listed; files purged after
the snapshot do not come back.

1. Stop the vault:

   ```bash
   docker compose -f docker-compose.yml -f docker-compose.prod.yml stop vault
   ```

2. Copy the snapshot over `vault.db`, and remove `vault.db-wal` and `vault.db-shm` so SQLite
   does not apply the old write-ahead log to the restored file. This moves the current files
   aside instead of deleting them. Replace the snapshot name:

   ```bash
   docker run --rm -v clipboard-vault_vault-data:/data alpine sh -c '
     set -e
     aside=/data/backups/replaced-$(date -u +%Y%m%dT%H%M%SZ)
     mkdir "$aside"
     for f in vault.db vault.db-wal vault.db-shm; do
       if [ -e "/data/$f" ]; then mv "/data/$f" "$aside/"; fi
     done
     cp /data/backups/vault-2026-11-08.db /data/vault.db
     chown -R 10001:10001 /data/vault.db "$aside"
   '
   ```

3. Start the vault and check readiness:

   ```bash
   docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d vault
   curl -fsS https://vault.example.com/health/ready
   ```

Delete the `replaced-*` folder once you no longer need the pre-restore state.

## Upgrading within 3.x

### Upgrade

Use `scripts/upgrade.sh` for every 3.x → 3.x upgrade on a Compose install. It refuses to run
on a host where 2.x PostgreSQL or auth containers are running.

Frontend releases carry no database or configuration change. The Rust binary compiles the
Askama templates, and the runtime image ships `static/`, so a normal image upgrade replaces the
interface atomically. The asset fingerprint changes with the files, so browsers pull the new
stylesheet and script on their next request. Ask anyone with a tab already open to reload it
once; an old tab keeps running the old script until it does.

Run the read-only preflight first, then deploy the same tested tag or commit:

```bash
git fetch --all --prune
./scripts/upgrade.sh --check <tested-tag-or-commit>
./scripts/upgrade.sh <tested-tag-or-commit>
```

`--check` verifies the local ref, a clean tracked checkout, `.env` (`PUBLIC_BASE_URL`,
`VAULT_SECRET`, `SETUP_TOKEN`, and complete OAuth pairs), Docker/Compose versions, that no 2.x
stack is running, the current and target production Compose configuration, the Docker-root
disk reserve, and that the vault service is running. It does not check out code, build or tag
images, write a snapshot, or recreate a container. Fetch the ref first because check mode
deliberately does not modify Git refs.

The upgrade:

1. Writes a consistent database snapshot with `clipboard-vault backup` to
   `/data/backups/pre-upgrade-<timestamp>.db`. If that fails, nothing is changed.
2. Tags the running image as `clipboard-vault:rollback-<timestamp>` and records the old commit.
3. Checks out the exact target commit and builds the new image while the old container keeps
   serving. If the build fails, the old container is untouched.
4. Recreates only the vault with `--no-build --wait`; Nginx stays running.
5. Waits for the in-container health check and the public `/health/ready`. If either fails, it
   retags the rollback image, recreates the vault, verifies it, and returns to the old commit.
6. Records `.deployment/last-success.env`.

Rust builds use a pinned `cargo-chef` recipe, so warm builds reuse the base, dependency, and
application layers. Use the slower base-image refresh during scheduled security maintenance:

```bash
./scripts/upgrade.sh --check <tested-tag-or-commit>
./scripts/upgrade.sh --refresh-base <tested-tag-or-commit>
```

Rollback image tags and `pre-upgrade-*` snapshots are retained; prune them only after the new
release has been stable and another verified snapshot exists.

### Rollback

Roll back to the image and commit recorded by the most recent successful upgrade:

```bash
./scripts/upgrade.sh --rollback
```

Rollback restores the previous image only. Database rows are not rewound: 3.x migrations are
additive, and an older 3.x binary starts on a database that a newer one migrated. If you also
need the data as it was before the upgrade, restore the `pre-upgrade-<timestamp>.db` snapshot
the script names, using [Restore from a snapshot](#restore-from-a-snapshot); writes made since
the upgrade are then lost. No rollback can reverse explicit item purges or other destructive
user actions.

## RivetPanel one-click app

The 3.0 RivetPanel template runs a single service. It is not in the catalog yet: the image
first has to be published (below), and then the template in the RivetPanel repository updated.

| Setting | Value |
| --- | --- |
| Service | `app`, one container |
| Image | `ghcr.io/xenycx/clipboard-vault:3.0.0`, pinned (never `:latest`) |
| User | `10001:10001` |
| Volume | `data` mounted at `/data` (database, uploads, snapshots), included in RivetPanel backups |
| Health | `/health/live` on port 8080 |
| Variables | `SETUP_TOKEN` and `VAULT_SECRET`, generated at install |
| Fixed environment | `PUBLIC_BASE_URL` set to the site's address, `DATA_DIR=/data`, `TRUST_PROXY_HEADERS=true` |
| Memory limit | 192 MiB |

There is no Nginx: RivetPanel's proxy terminates TLS and appends the client address, which is
why `TRUST_PROXY_HEADERS` is `true`. If you attach a different domain later, change
`PUBLIC_BASE_URL` to match it, or sign-in fails with `INVALID_ORIGIN`.

RivetPanel can install the app on any host only if the image is in a registry the panel can
pull from (`docker.io`, `ghcr.io`, or `quay.io` by default). Publishing is a separate step
that has not been done yet. It pushes only the image, for both architectures, and never moves
an existing tag:

```bash
docker buildx build --platform linux/amd64,linux/arm64 \
  -t ghcr.io/xenycx/clipboard-vault:3.0.0 -t ghcr.io/xenycx/clipboard-vault:3.0 --push .
```

Log in to the registry with a token that can write packages first, and set the package to
public afterward. Each later release gets a new tag, and the template pins it.

## Moving from 2.x to 3.0

**Compatibility: breaking, not upgradeable in place.** 2.x stores everything in PostgreSQL and
authenticates through a separate Better Auth service; 3.0 stores everything in SQLite on its
own volume and authenticates in-process. So 3.0 is installed next to 2.x, filled by a one-time
import, verified, and only then put on the domain. `scripts/upgrade.sh` refuses to do this.

| Surface | Impact |
| --- | --- |
| PostgreSQL data | Copied into `/data/vault.db` by `clipboard-vault import-postgres`. The 2.x database is only read, never modified. Every ID is kept, so item URLs and API key IDs stay valid. |
| Better Auth schema | Retired. Users, password hashes, social accounts, workspaces, memberships, API keys, approval and ban state, and server settings are imported. Better Auth verification and invitation rows are not. |
| Sessions | Not imported. The cookie changes to `vault_session`, so everyone signs in once. |
| API keys and `vault.py` | Unchanged. The same `cv_live_…` keys verify the same way, against the same `/api/v1/*` routes and JSON. |
| Stored files | Unchanged layout. `storage_key` paths are relative and the files are copied as-is into `/data/uploads`. The host needs free space for a second copy of the uploads during the move. |
| Tus uploads in progress | Lost at cutover. Clients restart them; the new server has no session to resume. |
| Environment variables | Retired: `DATABASE_URL`, `BETTER_AUTH_SECRET`, `BETTER_AUTH_URL`, `AUTH_BRIDGE_SECRET`, `AUTH_INTERNAL_URL`, `AUTH_PROXY_ENABLED`, `BOOTSTRAP_TOKEN`, and the `POSTGRES_*` variables. New: `VAULT_SECRET` and `SETUP_TOKEN` (both required), plus optional `DATA_DIR`, `TRUST_PROXY_HEADERS`, `BACKUP_KEEP`, and `DATABASE_READ_CONNECTIONS`. Upload limits and OAuth pairs keep their names. |
| Compose and Nginx | The `postgres` and `auth` services are gone. One `vault` service uses the new `vault-data` volume at `/data`. Nginx sends everything, including `/api/auth/`, to `vault:8080`. Certificates are reused. |
| Browser | `static/app.js` calls the same `/api/auth/*` paths with the same JSON. Social callback URLs are unchanged. Open tabs should be reloaded once. |
| Backups | `pg_dump` is replaced by `VACUUM INTO` snapshots in `/data/backups` plus the `uploads/` folder. |
| Rollback | Keep the 2.x stack, its database, and its uploads untouched until 3.0 has run cleanly. Rolling back means returning to 2.x and losing writes made on 3.0 after cutover; there is no 3.0 → 2.x export. Use a short maintenance window and verify before announcing. |

### What the import does

```text
clipboard-vault import-postgres --from <postgres-url> --data-dir <dir>
                                [--uploads-from <dir>] [--dry-run]
```

- It refuses to run when the target database already has users.
- With `--uploads-from`, it copies every blob file keeping its `storage_key`, and skips files
  already present with the right size, so an interrupted copy can be resumed.
- It imports every table in one SQLite transaction and keeps all IDs. Sessions, Better Auth
  verification and invitation rows, and Tus sessions are skipped.
- It prints a source/target row count per table, checks that every blob row has its file with
  the recorded size, and exits non-zero on any mismatch.
- `--dry-run` imports into a temporary database, checks the files in the source directory,
  prints the report, and keeps nothing.

Run the import while the 3.0 vault is stopped, so it has the data volume to itself, and always
rehearse with `--dry-run` first.

If a real import reported a mismatch after it committed, the target now has users and a second
run is refused. Fix the cause, empty the 3.0 data volume (it holds only the imported copy), and
start the import again.

### Runbook: Docker Compose install

Run these in the existing 2.x checkout on the VPS. The names below are the Compose defaults
for the project `clipboard-vault`; confirm yours with `docker ps`, `docker network ls`, and
`docker volume ls`.

| 2.x object | Default name |
| --- | --- |
| Private network | `clipboard-vault_backend` |
| PostgreSQL volume | `clipboard-vault_postgres-data` |
| Uploads volume | `clipboard-vault_vault-files` |
| PostgreSQL host on that network | `postgres` |

1. **Prepare while 2.x still serves.** Check that the Docker filesystem has room for a second
   copy of the uploads, record the 2.x commit, keep its images and `.env`, and take a last
   PostgreSQL dump for safety:

   ```bash
   docker run --rm -v clipboard-vault_vault-files:/u:ro alpine du -sh /u
   df -h "$(docker info --format '{{.DockerRootDir}}')"
   git rev-parse HEAD > ../clipboard-vault-2x.commit
   docker image tag clipboard-vault:local clipboard-vault:2x-final
   docker image tag clipboard-vault-auth:local clipboard-vault-auth:2x-final
   cp .env .env.2x && chmod 600 .env.2x
   mkdir -p backups
   docker compose exec -T postgres pg_dump -Fc -U vault clipboard_vault > backups/pre-3.0.dump
   ```

2. **Check out 3.0 and build it.** Add `VAULT_SECRET` and `SETUP_TOKEN` to `.env` (two new
   random values from `openssl rand -hex 32`). Leave the 2.x lines in place for now: 3.0's
   Compose file does not pass them to the container, and the import needs `POSTGRES_PASSWORD`.
   Building retags `clipboard-vault:local`; the running 2.x container is not affected.

   ```bash
   git fetch --all --tags --prune
   git checkout --detach <3.0-tag>
   docker compose build vault
   ```

3. **Start the maintenance window.** Stop the 2.x Nginx, app, and auth containers, and leave
   PostgreSQL running:

   ```bash
   docker ps --filter label=com.docker.compose.project=clipboard-vault --format '{{.Names}}'
   docker stop clipboard-vault-nginx-1 clipboard-vault-vault-1 clipboard-vault-auth-1
   ```

4. **Create the 3.0 data volume without starting the vault.** This replaces the stopped 2.x
   `vault` container (its data is in volumes and its image is tagged `2x-final`):

   ```bash
   docker compose create vault
   ```

5. **Rehearse the import with `--dry-run`.** The one-off container joins the 2.x network and
   mounts the 3.0 data volume and the 2.x uploads volume read-only:

   ```bash
   set -a; . ./.env; set +a
   docker run --rm --network clipboard-vault_backend \
     -v clipboard-vault_vault-data:/data \
     -v clipboard-vault_vault-files:/old-uploads:ro \
     clipboard-vault:local import-postgres \
       --from "postgresql://${POSTGRES_USER:-vault}:${POSTGRES_PASSWORD}@postgres:5432/${POSTGRES_DB:-clipboard_vault}" \
       --data-dir /data --uploads-from /old-uploads --dry-run
   ```

   Every table must show the same source and target count, with no `MISMATCH` or
   `FILE PROBLEM` lines, and the command must end with `dry run complete`.

6. **Import.** Run the same command without `--dry-run`. It must end with
   `import complete: /data/vault.db`. Stop here if it reports any mismatch.

7. **Start 3.0.** Compose warns about orphan `postgres` and `auth` containers; that is
   expected, so do not pass `--remove-orphans` yet.

   ```bash
   docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d --wait --force-recreate vault nginx
   ```

8. **Verify.** `curl -fsS https://vault.example.com/health/ready`, sign in (expected once for
   everyone), open an image and a long text item, upload through the browser and through
   `vault.py`, and list items with an existing API key. Then walk
   [Verify the interface](#8-verify-the-interface).

9. **Stop 2.x PostgreSQL** once 3.0 is verified, and keep everything else for at least a week:
   the stopped containers, the `clipboard-vault_postgres-data` and
   `clipboard-vault_vault-files` volumes, the `2x-final` images, `.env.2x`, and
   `backups/pre-3.0.dump`.

   ```bash
   docker stop clipboard-vault-postgres-1
   ```

**Roll back to 2.x** (within that week) by stopping 3.0 and starting the recorded 2.x
deployment. Writes made on 3.0 in between are lost; the 3.0 data volume is left as it is.

```bash
docker compose -f docker-compose.yml -f docker-compose.prod.yml stop vault nginx
git checkout --detach "$(cat ../clipboard-vault-2x.commit)"
cp .env.2x .env
docker image tag clipboard-vault:2x-final clipboard-vault:local
docker image tag clipboard-vault-auth:2x-final clipboard-vault-auth:local
docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d --no-build
```

**Clean up** once you are sure you will not roll back. Removing the volumes permanently
deletes the 2.x database and the old copy of the uploads:

```bash
docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d --remove-orphans
docker volume rm clipboard-vault_postgres-data clipboard-vault_vault-files
docker image rm clipboard-vault:2x-final clipboard-vault-auth:2x-final clipboard-vault-auth:local
rm .env.2x
```

Then delete the retired 2.x variables from `.env`. From now on, upgrade with
`scripts/upgrade.sh`.

### Runbook: vault.xenyc.ge on RivetPanel

The 2.x stack is RivetPanel stack `e74fe2ba` (services app `c5abb40e`, auth `b4c11e7a`,
db `5de4da19`) on the 8gb host. Docker on that host needs `sudo`. This needs the 3.0 image
published and the 3.0 template in the catalog (see
[RivetPanel one-click app](#rivetpanel-one-click-app)).

1. **Rehearse.** Install 3.0 from the catalog as a second app (for example on
   `vault3.snappages.click`). Run the import against a copy of production (steps 4–7), check
   sign-in, a few items, a download, and the API keys. Then delete the rehearsal app.
2. **Install** 3.0 from the catalog on a new site. Leave the domain on 2.x.
3. **Maintenance window:** stop the 2.x `app` and `auth` services in RivetPanel, leaving `db`
   running.
4. **Stop the 3.0 app** in RivetPanel, so the import has its data volume to itself.
5. **Rehearse the import with `--dry-run`.** A one-off container on the host joins the 2.x
   stack's private network and mounts the 3.0 data volume and the 2.x uploads volume. The 2.x
   database password is shown under the db service's variables in RivetPanel; `read -rs`
   keeps it out of your shell history:

   ```bash
   docker network ls | grep e74fe2ba        # the 2.x stack network
   read -rs DB_PASSWORD
   sudo docker run --rm --network <that network> --user 10001:10001 \
     -v /var/lib/rivetpanel/workspaces/<3.0 app id>/volumes/data:/data \
     -v /var/lib/rivetpanel/workspaces/c5abb40e-edbb-4549-ab60-2c080c7726f3/volumes/uploads:/old-uploads:ro \
     ghcr.io/xenycx/clipboard-vault:3.0.0 import-postgres \
       --from "postgresql://vault:${DB_PASSWORD}@db:5432/clipboard_vault" \
       --data-dir /data --uploads-from /old-uploads --dry-run
   ```

   Every table must show matching counts with no `MISMATCH` or `FILE PROBLEM` lines.
6. **Import:** run the same command without `--dry-run`. Stop if the count report shows any
   mismatch.
7. **Start the 3.0 app** in RivetPanel.
8. **Switch the domain:** remove `vault.xenyc.ge` from the 2.x site and add it to the 3.0 site.
   RivetPanel issues a new random verification value each time a domain is added to a site, so
   replace the value of the `_rivetpanel-verify.vault.xenyc.ge` TXT record in DNS with the new
   one the 3.0 site shows, then verify. The domain is not served between removal and
   verification. Set `PUBLIC_BASE_URL=https://vault.xenyc.ge` on the 3.0 app (sign-ins are
   refused with `INVALID_ORIGIN` until it matches). Traefik needs no change; it already sends
   the host to RivetPanel's sites. Moving back for a rollback needs another TXT change.
9. **Verify:** `/health/ready`, sign in (expected once), open an image and a long text item,
   upload through the browser and through `vault.py`, and list with an existing API key. A
   Cloudflare challenge blocks curl from the server to the public URL, so check health on the
   app's local port on the host instead.
10. **Keep 2.x stopped, not deleted,** for at least a week. To roll back, move the domain back
    and start 2.x `app` and `auth`. Writes made on 3.0 in between are lost.
11. Once you're satisfied, delete the 2.x stack, the old Compose install in `~/ClipboardVault`,
    and the local-only `2.0.0` images.

## Troubleshooting

- Sign-in or registration fails with `INVALID_ORIGIN`: `PUBLIC_BASE_URL` must match the address
  in the browser exactly, including scheme, hostname, and port (`https://vault.example.com`,
  not `http://`, not `www.`, not the IP address). Change it in `.env` and recreate the vault.
- Everyone is rate limited together (sign-in returns `TOO_MANY_REQUESTS` for all users at
  once): the server sees the proxy's address for every request. Behind a proxy that appends
  `X-Forwarded-For`, set `TRUST_PROXY_HEADERS=true`.
- The container cannot write `/data` (it exits at startup creating the data or upload
  directory, or opening `vault.db`): the volume must be writable by uid 10001. Named Docker
  volumes get that ownership from the image automatically; a bind-mounted host directory needs
  `sudo chown -R 10001:10001 <dir>`.
- The vault restarts or never becomes healthy: read `docker compose logs vault`. The usual
  causes are a missing or short `VAULT_SECRET` or `SETUP_TOKEN` (at least 32 characters each),
  a `PUBLIC_BASE_URL` that is not an absolute `http(s)` URL, or an OAuth pair with only one
  value set.
- The log warns `ignoring a 2.x setting that 3.0 no longer uses`: remove the named variable
  from the container's environment. It has no effect.
- `scripts/upgrade.sh` says the host runs 2.x: 2.x PostgreSQL or auth containers are still
  running. Finish [Moving from 2.x to 3.0](#moving-from-2x-to-30) first.
- A provider reports a redirect mismatch: compare its complete callback URL character by
  character and confirm `PUBLIC_BASE_URL` uses HTTPS.
- A social login reports no email: verify the provider account has a verified email. For a
  GitHub App, also grant read-only Email addresses permission. Discord phone-only accounts
  must add and verify an email or use another sign-in method.
- Site is pending forever: the first admin must claim `/setup`; later accounts are
  approved at `/admin`.
- Multipart upload returns 413: confirm it is no larger than `LEGACY_MAX_UPLOAD_BYTES` and
  `NGINX_LEGACY_CLIENT_MAX_BODY_SIZE`; use resumable upload for larger files.
- Tus chunk returns 413: keep client chunks at or below `TUS_CHUNK_SIZE_BYTES` and confirm
  `NGINX_TUS_CLIENT_MAX_BODY_SIZE` allows the chunk plus protocol overhead.
- Upload returns 507: free space through explicit workspace cleanup or increase VPS storage;
  do not bypass `UPLOAD_DISK_RESERVE_BYTES` without measuring available capacity.
- Upload is interrupted: reselect the same file in the browser or reuse its Tus URL with the
  resumable Python example; the server `HEAD` offset is authoritative. Uploads that were in
  progress during the move from 2.x must be started again.
- Certificate request fails: confirm DNS, ports 80/443, firewall rules, and that no other service
  owns those ports.
- The interface loads unstyled or misaligned: the image is missing its assets or the browser
  kept an old stylesheet. Confirm the stylesheet is served with
  `curl -sI https://vault.example.com/static/app.css`, then confirm the page requests a
  fingerprinted URL with
  `curl -s https://vault.example.com/login | grep -o "/static/app.css?v=[a-f0-9]*"`. A page that
  requests `/static/app.css` with no `?v=` is being served by an older build.
- The theme flashes on every page load: `/static/theme.js` is not being served. It must load
  before the stylesheet and must stay a separate file, because the page CSP forbids inline
  scripts.
- Buttons, the workspace switcher, uploads, or the command palette do nothing: check the browser
  console for CSP violations. `script-src 'self'` means an inline `<script>` block or an
  `onclick=` attribute is silently ignored; behavior belongs in `/static/app.js`.
- The sidebar disk meter never appears: it is rendered only for workspace owners and admins,
  because `/api/v1/storage` requires that role. This is expected for members.
