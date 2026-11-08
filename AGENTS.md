# Clipboard Vault agent guide

Clipboard Vault is a self-hosted workspace for explicitly submitted text, code, links, and files. Since 3.0 it is one Rust binary in one container with one data volume: the Axum application, native authentication, an embedded SQLite database, server-rendered pages, and the uploaded files.

## Live deployment and upgrade policy

This application is deployed on a VPS. Treat every change as an upgrade to a live production installation with existing users, database records (PostgreSQL on 2.x, SQLite on 3.x), authentication records, API keys, environment configuration, and files on a persistent volume. Do not reason about this repository as a disposable or greenfield deployment.

For every requested feature, bug fix, dependency upgrade, or architectural change, provide an explicit compatibility impact before implementation and repeat the important result in the final handoff. Use one of these classifications:

- **Backward-compatible:** can be deployed over the current VPS version without changing existing data, configuration, clients, or rollback behavior.
- **Migration required:** preserves existing data but requires an ordered database migration, environment/configuration update, service restart, client update, or deployment step.
- **Breaking or not safely upgradeable:** invalidates existing data/configuration/API behavior, requires downtime or destructive conversion, prevents old containers from reading new state, or cannot be rolled back safely.

The compatibility impact must identify every affected surface, not merely say that a change is breaking. Check the SQLite schema and migrations, identity tables (users, sessions, workspaces, memberships, API keys), API and form contracts, cookies, stored file layout, Tus upload sessions, the `/data` volume, environment variables, Docker Compose, Nginx, the RivetPanel template, browser/API clients, backups, deployment order, and rollback behavior as applicable.

Production may still be on 2.x (PostgreSQL + the Node.js auth service) until the documented 2.x → 3.0 import has been run; see "Moving from 2.x to 3.0" in `DEPLOYMENT.md`. That move is the one accepted breaking migration. Within 3.x, migrations must stay additive: an older 3.x binary starts on a database a newer one migrated (`set_ignore_missing(true)`), which is what makes image rollback work.

If a proposed feature would break the running system or cannot be upgraded safely from the old version to the new version:

1. Tell the user before making the breaking change.
2. Explain exactly what becomes incompatible or irreversible.
3. Provide the safest migration, backup, maintenance-window, or staged-rollout option when one exists.
4. Do not silently delete, rewrite, or abandon production data, API keys, upload bytes, or auth records.
5. Do not describe a feature as complete until its VPS deployment and upgrade path are documented.

When no breaking impact is found, say so explicitly and note any operational steps still required. New features must preserve in-place upgrades from the currently deployed version unless the user knowingly accepts a documented breaking migration.

## Read the scoped guide first

Before editing, follow the `AGENTS.md` nearest to the files in scope:

| Area | Guide | Responsibility |
| --- | --- | --- |
| Rust application, identity and API | `src/AGENTS.md` | Axum routes, native auth, authorization, SQLite, file storage, previews, resumable uploads, backups, and the 2.x import |
| Server-rendered UI | `templates/AGENTS.md` | Askama templates, page forms, semantic markup, and backend/template contracts |
| Browser assets | `static/AGENTS.md` | Shared design system, themes, client behavior, uploads, previews, and DOM hooks |

A frontend change commonly touches both `templates/` and `static/`; read both frontend guides. Cross-cutting behavior may also require the relevant backend guide. Root-level deployment, migration, test, or documentation work follows this file.

## Architecture

- One process, `clipboard-vault`, serves pages, `/api/v1/*`, Tus uploads, `/api/auth/*`, health checks, and `/static/*` on port 8080. Nginx (Compose install) or the platform proxy (RivetPanel) terminates TLS in front of it.
- Identity is native (`src/identity/`): users, password hashes, sessions, workspaces, memberships, API keys, social sign-in, approvals, and server settings. There is no auth service, no `/internal/*` bridge, and no bridge secret.
- Data lives in one SQLite database, `$DATA_DIR/vault.db`, in WAL mode: a pool of read-only connections for queries and exactly one write connection; multi-statement writes use `BEGIN IMMEDIATE` (`Db::begin`).
- File bytes are streamed to `$DATA_DIR/uploads`; the database stores their metadata and relative storage keys. Large payloads must not be buffered in memory.
- A background task writes a daily `VACUUM INTO` snapshot to `$DATA_DIR/backups/`; global admins can download one at `/admin/backup`.
- The Rust service renders Askama templates and serves the browser assets without a frontend build step.

## Request and data flow

### Browser page request

1. The proxy forwards the request to the application.
2. `identity::session_from_headers` hashes the `vault_session` cookie, loads the session, user and memberships, and picks the active workspace from the `vault_workspace` cookie only if the user belongs to it.
3. The handler authorizes the workspace and role, queries the read pool, builds an Askama model, and renders HTML.
4. `refresh_session` middleware slides the session expiry at most once a day and re-issues the cookie.
5. `templates/base.html` loads `/static/theme.js` (blocking, so the stored theme is applied before first paint), `/static/app.css`, `/static/app.js`, and vendored preview libraries.

### Browser authentication request

1. `static/app.js` posts JSON to `/api/auth/*` (same paths and JSON shapes Better Auth used).
2. `src/identity/routes.rs` checks `Origin` against `PUBLIC_BASE_URL`, applies the per-address rate limit, and validates the request.
3. It writes through the single writer and sets or clears the `vault_session` cookie.
4. The browser navigates to a rendered page.

### API or upload request

1. Rust accepts a session cookie, `Authorization: Bearer`, or `X-API-Key` where supported.
2. An API key is hashed and looked up; its row decides the workspace and permissions. Clients may not choose another workspace ID.
3. Rust checks role or key permissions, updates metadata in a short write transaction, and streams file bytes to or from the volume outside any transaction.
4. Multipart uploads are the compatibility path; large uploads use the Tus endpoints in `src/uploads.rs` and the resumable client in `static/app.js`.

## Repository map

- `src/lib.rs`: application state, router, shared middleware, security headers.
- `src/main.rs`: subcommands (`serve`, `healthcheck`, `backup`, `import-postgres`) and server lifecycle.
- `src/db.rs`: SQLite pools, pragmas, the stored timestamp format, write transactions, snapshots.
- `src/identity/`: sessions and cookies (`mod.rs`), public auth routes (`routes.rs`), passwords (`password.rs`), API keys (`keys.rs`), workspaces (`workspaces.rs`), global administration (`admin.rs`), social sign-in (`oauth.rs`), rate limits (`rate_limit.rs`).
- `src/auth.rs`: request authentication for vault routes (API key or session).
- `src/pages.rs`: server-rendered page handlers and Askama view models.
- `src/api.rs`: JSON APIs, item operations, search, storage status, content, and previews.
- `src/uploads.rs`: Tus creation, offsets, chunk writes, finalization, and cleanup.
- `src/storage.rs`: safe filenames, streaming, hashing, deduplication, and blob persistence.
- `src/backup.rs`: daily snapshots and the admin download.
- `src/import.rs`: the one-time 2.x PostgreSQL import.
- `templates/`: Askama HTML templates.
- `static/app.css`, `static/app.js`, `static/theme.js`: the browser layer (see `static/AGENTS.md`).
- `migrations/`: SQLite schema migrations (embedded in the binary).
- `migrations-postgres/`: the archived 2.x PostgreSQL schema, used by the import test.
- `nginx/`: optional public routing, request limits, and HTTPS configuration.
- `tests/app.rs`: end-to-end router tests against a temporary database.
- `tests/import_postgres.rs`: 2.x import test (runs when `IMPORT_TEST_DATABASE_URL` is set).
- `tests/test_security_contract.py`: cross-file security and architecture contracts.

## System invariants

- Never read or monitor the user's clipboard. The browser may only touch it on an explicit user gesture: `navigator.clipboard.writeText` for Copy buttons, and the `paste` event's `clipboardData` when the person presses Ctrl/Cmd+V on the vault page. Never call `navigator.clipboard.read`/`readText`, poll, or listen for `clipboardchange`.
- Never trust identity headers supplied by the public request. Rust removes legacy identity headers and authenticates from its own session and API-key tables.
- Never accept a caller-provided organization as authority. Resolve it from the session or verified API key and enforce role or permission checks server-side.
- Store only hashes of secrets: session tokens, API keys, invitation and reset tokens (SHA-256), passwords (Argon2id; legacy scrypt is verified then replaced). Never log or render them after creation.
- Every state-changing `/api/auth/*` route checks `Origin` against `PUBLIC_BASE_URL` before doing anything else, and is rate limited.
- Read `X-Forwarded-For` only when `TRUST_PROXY_HEADERS=true`, and only its right-most entry.
- All writes go through the single write connection; multi-statement writes use `db.begin()` (`BEGIN IMMEDIATE`). Never hash, stream, or copy a file while a write transaction is open.
- Timestamps are stored as fixed-width UTC TEXT written by `db::ts`/`db::now`. Never let SQL generate a timestamp or bind a raw `DateTime`.
- Stream uploads and downloads. Preserve size ceilings, free-space reserve checks, safe paths, authoritative Tus offsets, and partial-file cleanup.
- Sanitize rendered Markdown and HTML. Preserve sandboxing, restrictive security headers, forced downloads, and preview size limits.
- Never commit `.env`, secrets, complete API keys, setup tokens, OAuth credentials, or production database URLs.
- Never write inline `<script>` blocks or `on*=` handlers in templates. The page CSP is
  `script-src 'self'` with no nonce, so inline JavaScript is silently dead in production.
- Never reference `/static/...` from a template without the `|asset` filter. The fingerprint it
  appends is what stops a browser from pairing new markup with a cached old stylesheet.

## Cross-layer change checklist

When changing a user-facing capability, check all contracts it crosses:

1. Route registration in `src/lib.rs` (`build_router` or `auth_routes`).
2. Authorization and organization scoping.
3. Request/response model or Askama view model.
4. Template markup and `data-*` hooks.
5. Browser behavior and error handling.
6. An additive SQLite migration or file-storage lifecycle change, if state changes.
7. Nginx body/streaming rules for new upload endpoints, and the RivetPanel template when the service's interface changes.
8. Security-contract and service tests.
9. README, deployment, or API documentation when public behavior changes.
10. VPS upgrade classification, ordered deployment steps, backup needs, and rollback compatibility.

## Validation

Run the checks relevant to the changed scope:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
python -m pytest -q tests
docker compose config --quiet
```

`cargo test` runs the router end to end against a temporary SQLite database. Set `IMPORT_TEST_DATABASE_URL` to a disposable PostgreSQL database to include the 2.x import test (CI does). The GitHub Actions workflow builds the image once, loads that exact image into the integration stack, and exercises all enabled social-provider redirects. On Windows, Rust compilation needs a compatible linker; use CI, Docker, WSL, or Linux when it is unavailable locally.
