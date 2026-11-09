# Clipboard Vault

A lightweight, self-hosted team vault for pasted text and arbitrary files.

Clipboard Vault 3.x is one Rust/Axum binary in one container. It serves the pages, the JSON
API, resumable uploads, and its own sign-in (password plus optional Google, GitHub, or
Discord). Metadata, accounts, sessions, and API keys live in an SQLite database; file bytes
stay on disk next to it. Everything the server writes is under one data volume, `/data`.

Upgrading from 2.x (PostgreSQL plus a separate Node.js auth service)? That is not an in-place
upgrade. Follow [Moving from 2.x to 3.0](DEPLOYMENT.md#moving-from-2x-to-30).

## What it does

- Manual paste composer with text, code, HTML, and URL detection.
- Resumable, streamed file uploads up to 10 GiB without loading the whole file into memory.
- Inline previews for text, code, safe HTML/Markdown, images, and ranged PDFs.
- Private workspaces with owner, admin, and member roles.
- An approval queue for new accounts, which global admins can turn off.
- Password sign-in plus optional Google, GitHub, and Discord sign-in, built into the server.
- Workspace API keys with read, write, and delete permissions.
- Ready-to-run Python multipart and resumable file uploaders.
- Trash, restore, search, tags, exports, and activity records.
- Daily consistent database snapshots and an admin snapshot download.
- Docker Compose, Nginx, Certbot, health checks, and GitHub Actions.

Clipboard Vault never reads or monitors the browser clipboard on its own. It sees clipboard
data in exactly two moments the user initiates: pressing Ctrl+V (or Cmd+V) on the vault page,
which saves the pasted image or puts pasted text into the capture box, and clicking a Copy
button, which writes to the clipboard.

## Architecture

```mermaid
flowchart LR
  Browser["Browser or Python script"] --> Proxy["Nginx + HTTPS (or another proxy)"]
  Proxy --> Vault["clipboard-vault (one process)"]
  Vault --> DB["/data/vault.db (SQLite, WAL)"]
  Vault --> Files["/data/uploads/"]
  Vault --> Backups["/data/backups/"]
```

- **One process.** Pages, `/api/v1/*`, Tus uploads, `/api/auth/*`, admin actions, health
  checks, and `/static/*` are all served by the same binary on port 8080. There is no
  separate auth service, no internal bridge, and no shared secret between services.
- **SQLite on the data volume.** The database is `$DATA_DIR/vault.db` in WAL mode, with one
  write connection and a pool of read connections. Every change goes through the single
  writer, so writes are serialized.
- **Files next to it.** Uploaded bytes are streamed to `$DATA_DIR/uploads/`. The database
  stores their metadata and a relative `storage_key`; large payloads are never buffered in
  memory.
- **Snapshots.** Once a day the server writes `$DATA_DIR/backups/vault-YYYY-MM-DD.db` with
  `VACUUM INTO` and keeps the newest `BACKUP_KEEP` (7 by default).
- **Proxy.** The Compose file puts Nginx in front for HTTPS and request limits; Nginx sends
  every path, including `/api/auth/`, to `vault:8080`. Behind another proxy you can drop
  Nginx and publish the vault port instead.

The container runs as uid 10001 on a distroless base (no shell), works with a read-only root
filesystem, and writes only to `/data` (plus a `/tmp` tmpfs). It typically uses 64–192 MiB of
memory.

### Request flow

**Page request.** The browser sends the `vault_session` cookie. The server hashes the cookie
value, looks the session up in SQLite, resolves the user, approval state, workspace, and role,
queries the vault, and renders an Askama template. Pages load `/static/theme.js` first (so the
stored theme applies before first paint), then `/static/app.css`, `/static/app.js`, and the
vendored preview libraries.

**Sign-in.** `static/app.js` posts to `/api/auth/*`. The server checks that the request's
`Origin` matches `PUBLIC_BASE_URL`, applies per-address rate limits, verifies the password or
completes the social sign-in, and sets or clears the session cookie. The browser then
navigates back to a server-rendered page.

**API or upload request.** The server accepts a session cookie, `Authorization: Bearer`, or
`X-API-Key`. An API key decides the workspace and its permissions; clients cannot choose
another workspace ID. The server checks the role or key permission, updates metadata in a
transaction, and streams file bytes to or from `/data/uploads`. Multipart uploads are the
compatibility path; large uploads use Tus.

## Quick start

Requirements:

- Docker Engine 25 or newer with Docker Compose 2.20.2 or newer.
- A domain name for production social sign-in and HTTPS.
- A client ID and secret for each social provider you enable.

```bash
cp .env.example .env
# Edit .env and replace every placeholder.
docker compose up --build
```

Two settings are required. Generate each one separately:

```bash
openssl rand -hex 32
```

| Variable | Purpose |
| --- | --- |
| `VAULT_SECRET` | At least 32 characters. Signs the short-lived social sign-in state cookie. Changing it only interrupts sign-ins in progress. |
| `SETUP_TOKEN` | At least 32 characters. Claims the first administrator on `/setup`. |

The server refuses to start without them. Never commit the resulting `.env` file.

For password-only local testing, set `DOMAIN=localhost` and `PUBLIC_BASE_URL=http://localhost`
in `.env`, then open exactly `http://localhost` (not `127.0.0.1`: sign-in requests must come
from the `PUBLIC_BASE_URL` origin). Register an account, visit `/setup`, and enter
`SETUP_TOKEN`. After that succeeds, the setup route refuses all future claims.

You can also run the binary without Docker. It serves `static/` from the working directory,
so run it from the repository root:

```bash
VAULT_SECRET=$(openssl rand -hex 32) SETUP_TOKEN=$(openssl rand -hex 32) \
  DATA_DIR=./data PUBLIC_BASE_URL=http://localhost:8080 cargo run
```

A new secret on every start is fine for `VAULT_SECRET`, but note the `SETUP_TOKEN` you used
until you have claimed the first admin.

For a real VPS, follow [DEPLOYMENT.md](DEPLOYMENT.md).

### Configuration

| Variable | Default | Notes |
| --- | --- | --- |
| `VAULT_SECRET` | required | See above. |
| `SETUP_TOKEN` | required | See above. Replaces the 2.x `BOOTSTRAP_TOKEN`. |
| `PUBLIC_BASE_URL` | `http://localhost:8080` | The exact address people use, such as `https://vault.example.com`. Used for the sign-in `Origin` check, social callbacks, and secure cookies (`https://` turns on `Secure`). |
| `DATA_DIR` | `/data` | Holds `vault.db`, `uploads/`, and `backups/`. |
| `UPLOAD_ROOT` | `$DATA_DIR/uploads` | Where file bytes are stored. |
| `TRUST_PROXY_HEADERS` | `false` | When `true`, the right-most `X-Forwarded-For` entry is the client address used for rate limits. Enable it only behind a proxy that appends that header. The Compose file sets it to `true` because its Nginx does. |
| `BACKUP_KEEP` | `7` | Daily snapshots to keep. `0` disables them. |
| `DATABASE_READ_CONNECTIONS` | `8` | Size of the SQLite read pool. |
| `PORT` | `8080` | HTTP port inside the container. |
| `SERVER_MAX_UPLOAD_BYTES` | 10 GiB | Absolute per-file ceiling. |
| `LEGACY_MAX_UPLOAD_BYTES` | 100 MiB | Limit for the multipart endpoint. |
| `TUS_CHUNK_SIZE_BYTES` | 16 MiB | Tus chunk size. |
| `TUS_SESSION_TTL_SECONDS` | 7 days | Idle Tus sessions expire after this. |
| `UPLOAD_DISK_RESERVE_BYTES` | 20 GiB | Uploads are refused before free space falls below this. |
| `GOOGLE_`, `GITHUB_`, `DISCORD_CLIENT_ID` / `_CLIENT_SECRET` | blank | Optional social sign-in; set both values of a pair. |
| `RUST_LOG` | `clipboard_vault=info,tower_http=info` | Log filter. |

`DOMAIN`, `NGINX_LEGACY_CLIENT_MAX_BODY_SIZE`, `NGINX_TUS_CLIENT_MAX_BODY_SIZE`, and
`VAULT_MEMORY_LIMIT` (default `256m`) are read by Docker Compose and Nginx, not by the binary.

These 2.x variables are retired and no longer read: `DATABASE_URL`, `BETTER_AUTH_SECRET`,
`BETTER_AUTH_URL`, `AUTH_BRIDGE_SECRET`, `AUTH_INTERNAL_URL`, `AUTH_PROXY_ENABLED`,
`BOOTSTRAP_TOKEN`, and the `POSTGRES_*` variables. If one of the first seven reaches the
binary, it logs a startup warning so a half-migrated `.env` is noticed.

## Social authentication

Social providers are enabled only when both variables in their credential pair are set. A
partial pair fails startup with the missing variable named in the error; no credentials are
ever returned by the provider-discovery endpoint.

Register these exact production callbacks in the relevant provider console. They are the same
as in 2.x, so existing provider registrations keep working:

```text
https://vault.your-domain.com/api/auth/callback/google
https://vault.your-domain.com/api/auth/callback/github
https://vault.your-domain.com/api/auth/callback/discord
```

Set one or more complete pairs in the private VPS `.env`:

```dotenv
GOOGLE_CLIENT_ID=...
GOOGLE_CLIENT_SECRET=...
GITHUB_CLIENT_ID=...
GITHUB_CLIENT_SECRET=...
DISCORD_CLIENT_ID=...
DISCORD_CLIENT_SECRET=...
PUBLIC_BASE_URL=https://vault.your-domain.com
DOMAIN=vault.your-domain.com
```

For Google, create a Web application OAuth client. For GitHub OAuth Apps, request
`user:email`; GitHub Apps additionally need read-only Email addresses permission. Discord
uses only `identify` and `email`, with no bot or guild permissions.

A social identity is linked to an existing user only when the provider verifies the same
email address. Different-email linking and automatic linking without a verified email are not
possible. A provider account without a verified email is rejected rather than assigned a
placeholder address. Sign-in uses the authorization-code flow with PKCE; its state lives in a
10-minute cookie signed with `VAULT_SECRET`.

To add another provider, extend the provider registry in `src/identity/oauth.rs`, add its
credential pair, tests, and callback documentation, and release it normally. The login page
does not need provider-specific browser logic.

For example, in Google Cloud Console:

1. Create or select a project.
2. Open APIs & Services, then Credentials.
3. Create an OAuth client ID with application type Web application.
4. Add this exact authorized redirect URI:

```text
https://vault.your-domain.com/api/auth/callback/google
```

5. Put the client ID and secret into the private VPS `.env`.

The URL scheme, hostname, path, and trailing slash must match exactly. Production callbacks
require HTTPS and normally cannot use a raw VPS IP address.

## Accounts and workspaces

- Registration creates a pending account and a session with no vault access.
- A global admin approves or rejects the account.
- Approval creates the user’s personal workspace.
- Workspace owners and admins create private invitation links.
- Owners control limits, members, ownership transfer, and deletion; admins control limits,
  non-owner members, items, and all workspace keys.
- Invited users still need global approval.
- Invitation links are stored as hashes, work once, and expire after 48 hours.
- No email service runs. A global admin can create a one-time password-reset link and send
  it to the user manually.

Global administrators manage the server from **Administration**:

- Open or close registration. When closed, `/api/auth/sign-up/email` and first-time social
  sign-ins are refused, and only administrators create accounts.
- Require approval for new accounts, or let them open their workspace immediately.
- Set the default per-file upload limit (10 GiB unless lowered; capped by
  `SERVER_MAX_UPLOAD_BYTES`). Workspace owners can still choose their own limit.
- Create approved accounts with a password, grant or remove the administrator role, disable
  and re-enable accounts, sign an account out everywhere, issue reset links, and delete an
  account together with the workspaces only it owns.
- Download a fresh, consistent database snapshot with **Download database snapshot** on the Admin page (`/admin/backup`).

The first admin is protected by the private `SETUP_TOKEN`. Register the intended account,
visit `/setup`, and claim the instance before sharing the site.

### Sign-in security

- Passwords are 8–128 characters and hashed with Argon2id. Accounts imported from 2.x keep
  their Better Auth scrypt hashes, which verify normally and are replaced with Argon2id on the
  next successful sign-in.
- The session cookie `vault_session` holds 32 random bytes (`HttpOnly`, `SameSite=Lax`, and
  `Secure` when `PUBLIC_BASE_URL` is HTTPS). Only its SHA-256 is stored. Sessions last 7 days
  and slide forward at most once a day.
- Every state-changing `/api/auth/*` request must carry an `Origin` equal to
  `PUBLIC_BASE_URL`; others are refused with `INVALID_ORIGIN`.
- Rate limits are kept in memory per client address: sign-in 10 per minute, sign-up 5 per
  5 minutes, password reset 3 per 5 minutes. API keys are limited to 120 requests per minute
  each. Limits reset when the process restarts.

## API keys

Open Keys & team inside a workspace. Create a named key, choose its expiration and optional
permissions, then copy it immediately. The complete key is shown once.

The key decides the workspace. Scripts cannot submit another workspace ID.

Keys look like `cv_live_…` and only their SHA-256 hash is stored. The format and hashing are
the same as in 2.x, so keys created on 2.x keep working after the move to 3.0.

### Paste text

```bash
curl -X POST "https://vault.example.com/api/v1/items" \
  -H "Authorization: Bearer cv_live_REPLACE_ME" \
  -H "Content-Type: application/json" \
  -d '{
    "payload": "fn main() { println!(\"hello\"); }",
    "type": "auto",
    "virtual_path": "/snippets/rust",
    "tags": ["rust", "example"]
  }'
```

### Upload a file

```bash
curl -X POST "https://vault.example.com/api/v1/items" \
  -H "X-API-Key: cv_live_REPLACE_ME" \
  -F "file=@./report.pdf" \
  -F "virtual_path=/reports/2026" \
  -F 'tags=["report","monthly"]'
```

Both `Authorization: Bearer` and `X-API-Key` are supported.

The multipart endpoint is retained for compatibility and accepts files up to 100 MiB.
Larger files use the Tus 1.0 endpoint. The browser uploads in 16 MiB chunks and can pause,
retry, or resume from the last server-confirmed offset after an interruption. Tus upload
sessions expire after seven idle days.

The deployment ceiling is 10 GiB per file. A workspace starts at 100 MiB; an owner or admin
may raise that workspace limit up to the deployment ceiling. Uploads are rejected with
`507 Insufficient Storage` before the upload volume would cross its configured free-space
reserve. Clipboard Vault never deletes files automatically.

Soft-deleted items are listed with `GET /api/v1/items?trash=true`, restored with
`POST /api/v1/items/{id}/restore`, and permanently removed with
`POST /api/v1/items/{id}/purge`. Several items at once go through
`POST /api/v1/items/bulk` with `{"action": "trash" | "restore" | "purge", "ids": [...]}`
(purge only affects items already in trash), and `POST /api/v1/trash/empty` permanently deletes
the whole trash. Both need the `items:delete` permission.

### Python client (no dependencies)

`static/vault.py` is a single-file client that uses only the Python standard library (3.8+).
Creating a key in the browser offers a copy with the server address and the new key filled in;
**Keys and team** also links a blank copy that asks for them the first time it runs.

**Full-screen interface.** Run it with no arguments, or double-click it on Windows:

```bash
python vault.py
```

* **Browse the vault**: page through everything (Left/Right), search (`/`), filter by type
  (`t`), open an item to read its full text, select several items (Space, `a` for the page; the
  selection survives page changes) and download them (`d`) with live progress. `x` moves items to
  the trash; `Tab` switches to the trash, where `r` restores and `p` deletes for good.
* **Upload files**: a file browser. Space selects a file or a whole folder, selections are kept
  while you move between folders, `g` jumps to any typed or pasted path, `p` lists drives and
  recent places, `/` filters the current folder. Folders can be uploaded with their sub-folder
  structure. Uploads run in parallel, report per-file progress, and can be cancelled with Esc
  (large files resume the next time you upload them).
* **New text note**, **Storage**, and **Settings** (server, key, download folder, parallel transfers).

If the window closes too quickly after an error on Windows, nothing is lost: with no arguments the
script waits for Enter before closing when something went wrong.

**Network folders (UNC).** The script can live on a share such as `\\fileserver\tools\vault.py` and
be started by double-click from there. It never relies on the current directory (Windows
replaces a UNC working directory with `C:\Windows`), keeps its settings in your user profile, and
accepts `\\server\share\folder` paths for uploads and downloads (also with the quotes that
Explorer's "Copy as path" adds). Paths longer than 260 characters are handled. Access uses your
signed-in Windows account; if a share needs other credentials, connect to it first
(`net use \\server\share`). On Windows the interface needs Windows 10 or newer (any console, including
Windows Terminal).

**Where the key lives.** The first run asks for the server address and an API key and saves them in
`%APPDATA%\ClipboardVault\config.json` (`~/.config/clipboard-vault/config.json` elsewhere; set
`VAULT_CONFIG` to move it). Use the **blank** copy of the script on a shared folder: the
personalised copy from the key-created page has the key written inside it, so everyone who can read the
file can use the key.

Priority, highest first: `--server`/`--key`, `VAULT_SERVER`/`VAULT_API_KEY`, saved settings,
the values written into the script.

**Commands** (unchanged, for scripts):

```bash
python vault.py up report.pdf screenshots/        # parallel, resumable uploads of any size
python vault.py up photos/ --keep-structure -f /trips   # recreate sub-folders in the vault
python vault.py ls invoice                        # search
python vault.py get 3f2a9c1e -o ~/Downloads        # resumable download (IDs can be shortened)
git diff | python vault.py paste --tag diff       # save text from stdin
python vault.py cat 3f2a9c1e                       # print to stdout
python vault.py rm 3f2a9c1e && python vault.py empty-trash --yes
```

Files up to 90 MiB stream through one multipart request; larger files (or `--resumable`)
use Tus and resume automatically when the command is run again. `get` without `-o` saves into the
current folder, or into your Downloads folder when the current folder is not usable. Hidden
files and folders are skipped when a folder is uploaded.

### Older Python examples

```bash
python -m pip install requests
python examples/upload_file.py ./report.pdf \
  --server https://vault.example.com \
  --api-key cv_live_REPLACE_ME \
  --path /reports/2026 \
  --tag report --tag monthly
```

Use the resumable example for larger files or unreliable connections:

```bash
python examples/upload_resumable.py ./archive.tar.zst \
  --server https://vault.example.com \
  --api-key cv_live_REPLACE_ME \
  --path /archives
```

The resumable example prints the session URL. Re-run it with `--upload-url` to continue a
known session from the authoritative server offset.

### Previews and downloads

The dashboard loads previews only when requested. Text and code are syntax-highlighted up to
1 MiB, displayed as plain text up to 10 MiB, and truncated to the first 10 MiB above that.
Markdown and HTML rendered previews are sanitized and always retain an inert source view.
Common raster images up to 50 MiB, SVG (rendered as an image inside a sandboxing CSP), PDFs,
and browser-playable audio and video (MP3, M4A, WAV, FLAC, Ogg, MP4, MOV, WebM) are displayed
inline; media, PDF, and download responses support byte ranges. Pasted text opens in a reader
with line wrapping, Markdown and HTML rendering, syntax highlighting, copy, and a `.txt`
download. The original Download action remains available for every file, including
files that are unsafe or too large to preview.

The API returns a consistent error body:

```json
{
  "error": {
    "code": "PAYLOAD_TOO_LARGE",
    "message": "upload exceeds the allowed size",
    "detail": { "received": 120000000, "limit": 104857600 }
  }
}
```

### Route summary

Vault API (session cookie or API key):

| Method | Route | Purpose |
| --- | --- | --- |
| `POST` | `/api/v1/items` | Create pasted JSON content or upload one multipart file (100 MiB maximum) |
| `GET` | `/api/v1/items` | List/search active items; add `trash=true` for trash |
| `GET/PATCH/DELETE` | `/api/v1/items/{id}` | Read, edit metadata, or move an item to trash |
| `POST` | `/api/v1/items/{id}/restore` | Restore a trashed item |
| `POST` | `/api/v1/items/{id}/purge` | Permanently remove a trashed item and unreferenced bytes |
| `POST` | `/api/v1/items/bulk` | Trash, restore, or purge up to 1,000 items in one request |
| `POST` | `/api/v1/trash/empty` | Permanently delete every trashed item in the workspace |
| `GET/HEAD` | `/api/v1/items/{id}/content` | Stream stored bytes (or a text item as `.txt`); supports Range and `?download=1` |
| `GET/HEAD` | `/api/v1/items/{id}/preview` | Stream a classified, authenticated preview response |
| `OPTIONS/POST` | `/api/v1/uploads` | Discover Tus support or create a resumable upload |
| `GET/HEAD/PATCH/DELETE` | `/api/v1/uploads/{id}` | Read status, inspect offset, append, or cancel a Tus upload |
| `GET` | `/api/v1/uploads/{id}/status` | Read finalization state and completed item ID |
| `GET` | `/api/v1/storage` | Workspace usage, reservations, free space, and cleanup candidates |
| `POST` | `/api/v1/storage/purge` | Explicitly purge selected file items (owner/admin) |
| `GET` | `/api/v1/items/export?format=json|csv` | Export active item metadata and text |
| `GET` | `/api/v1/tags` | List workspace tags |

Authentication (used by the browser; same paths and JSON shapes as 2.x):

| Method | Route | Purpose |
| --- | --- | --- |
| `POST` | `/api/auth/sign-in/email` | Sign in with `{email, password}` and set the session cookie |
| `POST` | `/api/auth/sign-up/email` | Register with `{name, email, password}` (honors closed registration and approval) |
| `POST` | `/api/auth/sign-out` | Delete the session and clear the cookie |
| `POST` | `/api/auth/reset-password` | Set a new password with `{token, newPassword}` from a reset link |
| `POST` | `/api/auth/vault/bootstrap` | Claim the first administrator with `{token}` (`SETUP_TOKEN`) |
| `GET` | `/api/auth/vault/providers` | List the enabled social providers |
| `POST` | `/api/auth/sign-in/social` | Start a Google, GitHub, or Discord sign-in |
| `GET` | `/api/auth/callback/{provider}` | Social sign-in callback (register this in the provider console) |

Administration and health:

| Method | Route | Purpose |
| --- | --- | --- |
| `GET` | `/admin/backup` | Download a fresh database snapshot (global admins only) |
| `GET` | `/health/live` | The process is up |
| `GET` | `/health/ready` | The process is up and the database answers |

## Development checks

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
python -m pytest -q tests
docker compose config --quiet
```

`cargo test` runs the page, API, and auth tests against temporary SQLite files; no database
server is needed. The PostgreSQL import test (`tests/import_postgres.rs`) is skipped unless
`IMPORT_TEST_DATABASE_URL=postgresql://…` points at a disposable PostgreSQL database; it drops
and recreates the schema there. To run it locally:

```bash
# In one terminal (stops and removes itself on Ctrl+C):
docker run --rm -p 127.0.0.1:55432:5432 -e POSTGRES_PASSWORD=test postgres:17-alpine
# In another:
IMPORT_TEST_DATABASE_URL=postgresql://postgres:test@127.0.0.1:55432/postgres \
  cargo test --all-features --test import_postgres
```

CI runs it against a PostgreSQL service.

On Windows, local Rust compilation needs a compatible linker. It is not necessary to install
Visual Studio if development and builds are performed inside Linux, Docker, WSL, CI, or the
VPS. Production images compile Rust inside the official Alpine Rust container.

## Documentation

- [VPS deployment, HTTPS, backups, upgrades, rollback, and moving from 2.x](DEPLOYMENT.md)
- [Security model and reporting](SECURITY.md)
- [.env configuration template](.env.example)

## License

MIT
