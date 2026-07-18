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
- Upload limits if the 100 MB server and 25 MB workspace defaults are unsuitable.

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

### Backups

Load the environment and run:

```bash
set -a
. ./.env
set +a
./scripts/backup.sh
```

Each backup contains a PostgreSQL custom-format dump, a compressed file-volume archive, and
SHA-256 checksums. Copy backups off the VPS. Recommended schedule is every six hours with
30 days of local retention and daily offsite copies.

Verify a backup before relying on it:

```bash
cd backups/<timestamp>
sha256sum -c SHA256SUMS
pg_restore --list database.dump >/dev/null
tar -tzf files.tar.gz >/dev/null
```

### Restore

Stop writes and preserve the current volumes first:

```bash
docker compose stop vault auth nginx
docker compose up -d postgres
cat backups/<timestamp>/database.dump | docker compose exec -T postgres \
  pg_restore --clean --if-exists -U vault -d clipboard_vault
docker run --rm -v clipboard-vault_vault-files:/restore \
  -v "$PWD/backups/<timestamp>:/backup:ro" alpine \
  sh -c 'rm -rf /restore/* && tar -xzf /backup/files.tar.gz -C /restore'
docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d
```

Test login, item listing, and one file download after restoring.

### Upgrade

```bash
./scripts/backup.sh
git fetch --all --prune
git checkout <tested-tag-or-commit>
docker compose -f docker-compose.yml -f docker-compose.prod.yml build --pull
docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d
```

Review release notes and committed migrations before upgrading.

### Rollback

Checkout the previous tested commit and rebuild. If the release changed the database in an
incompatible way, restore the pre-upgrade database and file backup together.

## Troubleshooting

- Google says `redirect_uri_mismatch`: compare the complete callback URL character by
  character and confirm `PUBLIC_BASE_URL` uses HTTPS.
- Site is pending forever: the first admin must claim `/setup`; later accounts are
  approved at `/admin`.
- Upload returns 413: raise both `SERVER_MAX_UPLOAD_BYTES` and
  `NGINX_CLIENT_MAX_BODY_SIZE`, then restart Nginx and Rust.
- Authentication service unhealthy: inspect `docker compose logs auth` and verify all
  three secrets and PostgreSQL credentials.
- Certificate request fails: confirm DNS, ports 80/443, firewall rules, and that no other service
  owns those ports.
