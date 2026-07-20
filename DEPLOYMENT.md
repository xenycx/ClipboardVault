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

Install Docker Engine and the Compose plugin from Docker’s official Ubuntu repository. Verify:

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
- Google client ID and secret from Google Cloud.
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

## 5. Configure Google

Create a Google OAuth Web application and add:

```text
https://vault.example.com/api/auth/callback/google
```

Set `GOOGLE_CLIENT_ID` and `GOOGLE_CLIENT_SECRET` in `.env`.
Do not paste either secret into GitHub issues, screenshots, or chat.

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
2. Register the intended administrator with Google or password.
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

```bash
git fetch --all --prune
git checkout <tested-tag-or-commit>
docker compose -f docker-compose.yml -f docker-compose.prod.yml build --pull
docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d
```

Before upgrading, record the running commit and verify that the upload volume has more than
the configured reserve available. Review release notes and committed migrations. After the
restart, verify both health routes, resume an interrupted upload, and smoke-test text, image,
PDF preview, ranged download, and explicit cleanup.

### Rollback

Checkout the previous tested commit and rebuild the services. This rolls back application
code only; it cannot undo a destructive data operation or an incompatible migration.

## Troubleshooting

- Google says `redirect_uri_mismatch`: compare the complete callback URL character by
  character and confirm `PUBLIC_BASE_URL` uses HTTPS.
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
- Authentication service unhealthy: inspect `docker compose logs auth` and verify all
  three secrets and PostgreSQL credentials.
- Certificate request fails: confirm DNS, ports 80/443, firewall rules, and that no other service
  owns those ports.
