# VPS deployment guide

This guide targets a fresh Ubuntu 24.04 VPS. Replace `vault.example.com` with your
real domain.

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
- `PUBLIC_BASE_URL`: `https://vault.example.com`.
- `POSTGRES_PASSWORD`: a unique random password.
- `BETTER_AUTH_SECRET`, `AUTH_BRIDGE_SECRET`, and
  `BOOTSTRAP_TOKEN`: three different random values.
- A complete client ID/secret pair for every Google, GitHub, or Discord provider you enable.
- Upload limits if the 10 GiB deployment ceiling, 100 MiB workspace default, 16 MiB Tus
  chunks, seven-day session expiry, or 20 GiB disk reserve are unsuitable.

`SERVER_MAX_UPLOAD_BYTES` is the absolute per-file ceiling. Keep
`LEGACY_MAX_UPLOAD_BYTES` at 100 MiB for the multipart compatibility endpoint and use Tus for
larger files. `UPLOAD_DISK_RESERVE_BYTES` protects the VPS from being filled by uploads; the
vault volume must have more free space than the reserve plus the file being uploaded.

Generate each secret separately:

```bash
openssl rand -hex 32
```

## 5. Configure social providers

Providers with two blank variables are disabled and hidden from the login page. Setting only
one value in a pair is a configuration error and makes both `--check` and auth startup fail.

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

Better Auth may link a provider to an existing user only when that provider verifies the
same email. Do not configure trusted providers, different-email linking, or replacement
provider IDs during a routine upgrade. Do not paste any provider secret into GitHub issues,
screenshots, or chat.

## 6. Start HTTP and issue HTTPS

```bash
chmod +x scripts/*.sh
./scripts/init-https.sh
```

The script:

1. Starts PostgreSQL, Better Auth, Rust, and temporary HTTP Nginx.
2. Uses the ACME webroot to request a Let’s Encrypt certificate.
3. Restarts Nginx with the HTTPS configuration.

Add certificate renewal to root’s crontab:

```cron
17 3 * * * cd /opt/clipboard-vault && ./scripts/renew-certificates.sh >> /var/log/clipboard-vault-certbot.log 2>&1
```

Adjust the repository path.

## 7. Claim the first administrator

1. Open `https://vault.example.com/login`.
2. Register the intended administrator with an enabled social provider or password.
3. Open `https://vault.example.com/setup`.
4. Enter `BOOTSTRAP_TOKEN` from the VPS `.env`.
5. Confirm the account opens its personal workspace and the Accounts page.

The claim endpoint refuses additional attempts after one global admin exists. Rotate the
bootstrap token afterward and restart the auth service:

```bash
docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d --force-recreate auth
```

## Operations

### Status and logs

```bash
docker compose -f docker-compose.yml -f docker-compose.prod.yml ps
docker compose -f docker-compose.yml -f docker-compose.prod.yml logs --tail=200 vault auth nginx
curl -fsS https://vault.example.com/health/live
curl -fsS https://vault.example.com/health/ready
```

### Storage pressure and cleanup

The application reserves 20 GiB of free space by default. If an upload would cross that
boundary it returns `507 Insufficient Storage`; login, previews, downloads, and cleanup remain
available. Do not reduce the reserve without accounting for PostgreSQL, container images,
logs, and operating-system updates on the same filesystem.

Workspace owners and admins can review reserved bytes plus the oldest and largest items in
the storage screen. Purging requires explicit selection and workspace-name confirmation.
No age-based or low-space process deletes vault data automatically.

Permanent purge is irreversible, and a code rollback cannot recover deleted PostgreSQL
rows or file bytes.

### Upgrade

Routine releases use the guarded operator command. Run the read-only preflight first, then
deploy the same tested tag or commit:

```bash
git fetch --all --prune
./scripts/upgrade.sh --check <tested-tag-or-commit>
./scripts/upgrade.sh <tested-tag-or-commit>
```

`--check` verifies the local ref, clean tracked checkout, Docker/Compose versions, `.env`,
OAuth pairs, merged production Compose configuration, Docker-root disk reserve, and the
running database. It does not check out code, build/tag images, back up data, or recreate a
container. Fetch the ref first because check mode deliberately does not modify Git refs.

The upgrade records the old commit and image IDs, writes a compressed PostgreSQL custom-format
backup under `backups/`, and tags the current auth and vault images. It then checks out the
exact commit and builds both new images while the old containers continue serving. Only auth
and vault are recreated with `--no-build --wait`; PostgreSQL and Nginx remain running. Internal
and public readiness must pass before `.deployment/last-success.env` is recorded.

Rust builds use a pinned `cargo-chef` recipe, and auth has a narrow Docker context. Warm builds
reuse base, Rust dependency, pnpm dependency, and application layers. Use the slower base-image
refresh during scheduled security maintenance:

```bash
./scripts/upgrade.sh --check <tested-tag-or-commit>
./scripts/upgrade.sh --refresh-base <tested-tag-or-commit>
```

If the build fails, the old containers remain untouched. If startup or either readiness check
fails, the script retags the recorded images, force-recreates only auth and vault, verifies the
old deployment, and returns to the old commit. Backups and rollback image tags are retained;
prune them only after the new release has been stable and another verified backup exists.

### First rollout of the guarded upgrader

The currently deployed commit may not yet contain `scripts/upgrade.sh`. Fetch the target,
extract its script to a temporary path without checking out the application, then run its
preflight and upgrade from the repository root:

```bash
git fetch --all --tags --prune
git show <tested-tag-or-commit>:scripts/upgrade.sh > /tmp/clipboard-vault-upgrade.sh
chmod 700 /tmp/clipboard-vault-upgrade.sh
/tmp/clipboard-vault-upgrade.sh --check <tested-tag-or-commit>
/tmp/clipboard-vault-upgrade.sh <tested-tag-or-commit>
rm /tmp/clipboard-vault-upgrade.sh
```

Before this first rollout, add the new optional OAuth variables to `.env` (blank pairs are
valid), register provider callbacks, and verify the minimum Docker versions. The auth schema
does not change: existing `account` rows already store multiple `(providerId, accountId)`
identities. Keep `BETTER_AUTH_SECRET` and existing provider IDs unchanged so existing sessions,
passwords, and Google identities stay valid.

### Rollback

Roll back to the images and commit recorded by the most recent successful upgrade:

```bash
./scripts/upgrade.sh --rollback
```

Rollback does not restore the database backup. This release has no destructive auth or vault
migration, so old and new code share the schema. GitHub/Discord account rows remain harmless
when old code is running, but users who registered only through one of those providers cannot
reauthenticate until the new auth image returns; their already-valid sessions continue to
work. No code rollback can reverse explicit item purges or other destructive user actions.

To inspect a retained database backup:

```bash
pg_restore --list backups/postgres-<timestamp>.dump
```

## Troubleshooting

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
  resumable Python example; the server `HEAD` offset is authoritative.
- Authentication service unhealthy: inspect `docker compose logs auth`, verify all three
  application secrets and PostgreSQL credentials, and make sure every OAuth pair is either
  complete or completely blank.
- Certificate request fails: confirm DNS, ports 80/443, firewall rules, and that no other service
  owns those ports.
