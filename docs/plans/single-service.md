# Plan: Clipboard Vault as one service

Status: **Phases 1–4 and 7 implemented** on branch `release/3.0` (not yet merged or released).
Phases 5–6 (publishing the image, the RivetPanel template) and the production runbook are still
to do; see "Implementation notes" at the end. Target release: **3.0.0**.

## Goal

Ship Clipboard Vault as **one container with one volume**, so it:

- installs from the RivetPanel one-click catalog on any RivetPanel server, not just the 8gb VPS;
- runs in about **64–192 MiB** of memory instead of **768 MiB** across three containers;
- needs one secret instead of four, and no reverse proxy of its own.

Today's stack has three services: the Rust app, a Node.js auth service (Hono + Better Auth),
and PostgreSQL. The plan folds the auth service into the Rust binary and replaces PostgreSQL
with an embedded SQLite database on the data volume.

## Decision

| Option | What runs | Memory | Work | Verdict |
| --- | --- | --- | --- | --- |
| A. Bundle today's services in one image (supervisor + Node + Rust + Postgres) | 3 processes, 1 container | ~600 MiB | Small | Rejected: still heavy, runs a database server inside an app container, fragile restarts and backups. |
| B. Rust + Node auth in one container, SQLite shared by both | 2 processes, 1 container | ~200 MiB | Medium–large (all SQL ported **and** Better Auth moved to SQLite) | Rejected: does most of C's work but keeps Node. |
| **C. Rust-native auth + SQLite** | **1 binary** | **~64–192 MiB** | Large | **Chosen.** One static binary, nothing else to run. |

Option C is feasible without forcing anyone to reset anything, because the existing secrets
are portable (checked against Better Auth 1.6.23 in `auth/node_modules`):

- **Passwords** are `scrypt` with N=16384, r=16, p=1, a 64-byte key, and the password
  NFKC-normalized. They are stored as `<salt>:<key>` in hex, where the 32-character hex salt
  string itself (its UTF-8 bytes) is the salt. Rust can verify these with the `scrypt` crate and
  rehash to Argon2id on the next successful sign-in.
- **API keys** are stored as SHA-256 of the full key, base64url without padding. A key is
  verified by hashing and comparing, so every existing `cv_live_…` key keeps working unchanged.
- **Sessions** cannot be carried over cheaply (they are signed Better Auth cookies). Everyone
  signs in once after the upgrade. This is the only user-visible break.

## Compatibility classification

**Breaking, not upgradeable in place.** The data moves from PostgreSQL to a new SQLite file, so
3.0 is installed next to 2.x and filled by a one-time import, then the domain is switched.

| Surface | Impact |
| --- | --- |
| PostgreSQL data | Copied into SQLite by `clipboard-vault import-postgres`. The 2.x database is not modified. |
| Better Auth schema | Retired. Users, password hashes, workspaces, memberships, API keys, approval and ban state are imported. |
| Sessions | Not imported. Everyone signs in once. |
| API keys and `vault.py` | Unchanged. Same keys, same `/api/v1/*` routes and JSON. |
| Stored files | Unchanged layout. `storage_key` paths are relative and copied as-is into `/data/uploads`. |
| Tus uploads in progress | Lost at cutover. Clients restart them (offsets are server-side, and the new server has no session). |
| Environment variables | `DATABASE_URL`, `BETTER_AUTH_SECRET`, `AUTH_BRIDGE_SECRET`, `AUTH_INTERNAL_URL`, `AUTH_PROXY_ENABLED` and `BOOTSTRAP_TOKEN` are retired. New: `VAULT_SECRET`, `SETUP_TOKEN`, `DATA_DIR`, `TRUST_PROXY_HEADERS`. Upload limits keep their names. |
| Compose and Nginx | Compose shrinks to one service. Nginx, if kept, sends everything to the one upstream. |
| Browser | `static/app.js` keeps calling the same `/api/auth/*` paths and JSON shapes. The cookie name changes. |
| Backups | `pg_dump` is replaced by a consistent SQLite snapshot (`VACUUM INTO`) plus the uploads folder. |
| Rollback | Keep the 2.x stack and its database untouched until 3.0 has run cleanly. Rolling back means returning to 2.x and losing writes made on 3.0 after cutover; there is no 3.0 → 2.x export. Use a short maintenance window and verify before announcing. |

## Target architecture

```
                 RivetPanel site / any reverse proxy (TLS)
                                 │
                         clipboard-vault (one process)
      ┌──────────────┬───────────┴───────────┬────────────────┐
  pages + static   /api/v1/* + Tus      /api/auth/* (native)   admin
      └──────────────┴───────────┬───────────┴────────────────┘
                         SQLite /data/vault.db (WAL)
                         files  /data/uploads/…
```

- One binary, statically linked (musl), on a distroless or Alpine base image. It runs as uid
  `10001`, works with a read-only root filesystem, and writes only to `/data` and `/tmp`.
- `/internal/*` and `AuthBridge` disappear. Every bridge call becomes an in-process function,
  so there is no shared secret to protect.
- `GET /health/live` reports the process is up; `GET /health/ready` adds a database check.

## Work breakdown

Estimates assume one person who knows this codebase. Each phase ends green on
`cargo fmt`, `cargo clippy -D warnings`, `cargo test` and `pytest tests`.

### Phase 1 — Storage layer on SQLite (≈ 4–6 days)

1. **Dependencies.** In `Cargo.toml`, switch sqlx features from `postgres` to `sqlite`. Add
   `scrypt`, `argon2`, `rand`, `base64` and `subtle`.
2. **Connection setup** (`src/db.rs`, new): `DATA_DIR/vault.db` with `journal_mode=WAL`,
   `foreign_keys=ON`, `busy_timeout=5000` and `synchronous=NORMAL`. Use two pools: a read pool
   (many connections) and a write pool with exactly one connection. All mutations go through
   the write pool with `BEGIN IMMEDIATE`, which replaces every `FOR UPDATE` and
   `pg_advisory_xact_lock`.
3. **New migrations.** Start a fresh `migrations/` series at `0001_init.sql` for SQLite and
   archive the Postgres ones under `migrations-postgres/`, which the import tool still needs
   for reference. The identity tables are in Phase 2; the vault tables map like this:

   | Postgres | SQLite |
   | --- | --- |
   | `UUID` | `TEXT` (hyphenated) |
   | `TIMESTAMPTZ` | `TEXT` RFC 3339, always written by Rust, never by SQL `now()` |
   | `JSONB` tags | `TEXT` holding a JSON array, queried with `json_each` |
   | `BIGINT` | `INTEGER` |
   | GIN `to_tsvector` index | FTS5 table `vault_items_fts(text_payload, original_filename, virtual_path)`, kept in sync by triggers, `tokenize='unicode61 remove_diacritics 2'` (handles Georgian) |

4. **Port the queries** (≈ 68 call sites in `api.rs`, `pages.rs`, `uploads.rs`, `storage.rs`):
   - `now()` (31 uses): bind `Utc::now()` from Rust. Mixing SQL-generated and Rust-generated
     timestamp strings breaks ordering, so this rule has no exceptions.
   - `= ANY($n)` (5): `IN (SELECT value FROM json_each(?))` with one JSON-array bind.
   - `::bigint` casts (10): remove them; SQLite integers are 64-bit. Delete the matching
     pytest contract and replace it with one that forbids Postgres casts.
   - `tags ? $n`: `EXISTS (SELECT 1 FROM json_each(vault_items.tags) WHERE value = ?)`.
   - `RETURNING`, `ON CONFLICT` and `count(*) FILTER (WHERE …)` work as-is (SQLite 3.35+).
   - Search: rewrite `search_tsquery` into an FTS5 query builder. It quotes each word and adds
     `*` for prefix matching (`"depl"* AND "loc"*`), and keeps today's tests, including the
     Georgian one.
5. **Disk checks.** `available_space` and the upload reserve logic are unchanged. They now
   measure the filesystem under `/data`.
6. **Backups.** A background task runs `VACUUM INTO '/data/backups/vault-<date>.db'` daily and
   keeps 7. Add an admin button that downloads a fresh snapshot. RivetPanel's volume backups
   then always contain a consistent copy, even if the live file was mid-write.

**Done when** every page and API test passes against a temporary SQLite file with no Docker or
Postgres. That is a real gain: today the database paths are only tested in CI with containers.

### Phase 2 — Native identity (≈ 6–9 days)

Create `src/identity/` with these tables (in the SQLite `0001_init.sql`):

`users` (id, email unique case-insensitive, name, password_hash, role, approval_status,
approved_at/by, banned, created_at), `oauth_accounts` (provider, provider_account_id unique,
user_id), `sessions` (token_hash, user_id, active_workspace, expires_at, last_seen_at, ip, user
agent), `workspaces` (id, name, slug), `memberships` (workspace_id, user_id, role), `api_keys`
(id, workspace_id, created_by, name, start, key_hash unique, permissions JSON, expires_at,
enabled, request_count, last_request_at), `password_resets` (token_hash, user_id, expires_at,
used_at), and `server_settings` (moved from `vault_server_settings`).

Replace the 17 bridge endpoints with functions:

| Today (`/internal/…`) | New function |
| --- | --- |
| `session` | `identity::session_from_cookie(headers)` |
| `verify-key` | `identity::verify_api_key(key, permission)` |
| `api-keys` list/create/delete | `identity::keys::{list, create, revoke}` |
| `admin/users`, `approve`, `reject`, `ban`, `set-role`, `create-user`, `delete-user`, `revoke-sessions`, `reset-link` | `identity::admin::*` |
| `workspaces/details`, `join`, `member-role`, `remove-member`, `transfer`, `delete` | `identity::workspaces::*` |

Keep every authorization rule the TypeScript code enforces today: not self, not the last
admin, owner-only transfer, and email confirmation for delete. Port the bridge tests into Rust
unit tests.

Public routes stay at the same paths and JSON shapes that `static/app.js` uses, so the browser
code barely changes:

| Route | Contract to keep |
| --- | --- |
| `POST /api/auth/sign-in/email` | `{email, password}`. 200 sets the cookie; errors `{message, code}` with `INVALID_EMAIL_OR_PASSWORD`, `BANNED_USER`, `TOO_MANY_REQUESTS`. |
| `POST /api/auth/sign-up/email` | `{name, email, password}`. Honours registration-closed (`REGISTRATION_CLOSED`) and require-approval. |
| `POST /api/auth/sign-out` | Clears the cookie and deletes the session. |
| `POST /api/auth/reset-password` | `{token, newPassword}`. |
| `POST /api/auth/vault/bootstrap` | `{token}` claims the first admin using `SETUP_TOKEN`. |
| `GET /api/auth/vault/providers` | `{data: {providers: [...]}}`. |
| `POST /api/auth/sign-in/social` and `GET /api/auth/callback/:provider` | Same flow, error codes as today's `oauthErrorMessages`. |

Security requirements, matching or exceeding today's:

- **Passwords:** Argon2id for new hashes. Accept legacy scrypt `salt:key` hashes (parameters
  above) and rehash on success. Use constant-time comparison, and enforce 8–128 characters.
- **Sessions:** 32 random bytes in the cookie `vault_session` (`HttpOnly`, `Secure` on HTTPS,
  `SameSite=Lax`, `Path=/`). Store only the SHA-256 of the token. Sessions last 7 days and
  slide forward once a day, as today.
- **CSRF:** every state-changing `/api/auth/*` request must carry an `Origin` that equals
  `PUBLIC_BASE_URL`, as Better Auth's trusted-origins check does today.
- **Rate limits** (in memory, per client IP): sign-in 10/min, sign-up 5/5 min,
  reset 3/5 min, and 120 requests/min per API key. Read the client IP from
  `X-Forwarded-For` only when `TRUST_PROXY_HEADERS=true`, which the RivetPanel template sets.
- **API keys:** generate `cv_live_` plus 64 random alphanumeric characters, store the
  SHA-256/base64url hash, keep `start` as the visible prefix, and check expiry, `enabled` and
  permissions on every request.
- **Social sign-in** (Google, GitHub, Discord): port `auth/src/providers.ts` with the `oauth2`
  crate. Keep the current linking rules: only verified emails, no linking across different
  emails, and no automatic linking without a verified email. Production has no providers
  configured, so this can land as the last step of the phase, but 3.0 should not ship
  without it.

**Done when** the old pytest contracts for auth behavior have Rust equivalents.
`static/app.js` changes only the cookie-related code (if any). The `auth/` directory is
deleted.

### Phase 3 — Import tool (≈ 2–3 days)

Add a subcommand to the same binary:

```bash
clipboard-vault import-postgres --from postgresql://vault:…@db:5432/clipboard_vault \
  --data-dir /data [--uploads-from /old/uploads] [--dry-run]
```

- It refuses to run if `/data/vault.db` already has users.
- It imports in one transaction, in this order: settings, users (password hashes as-is),
  OAuth accounts, workspaces (from `organization`), memberships (from `member`), API keys
  (`key` hash, `start`, permissions, expiry, `metadata.createdByUserId`), then
  blobs → items → invitations → activity → workspace settings → server settings.
- It keeps every ID, so item URLs and key IDs stay valid.
- It skips sessions, verification rows, Better Auth `invitation` rows and Tus sessions.
- If given `--uploads-from`, it copies files keeping each `storage_key` path, then checks
  that every blob row has its file and that the sizes match.
- It finishes with a count report per table and exits non-zero on any mismatch.
- Test it in CI against a Postgres service seeded with the 2.x schema plus fixture rows,
  including a user with a real Better Auth scrypt hash and a real API key, and assert that both
  still work after the import.

### Phase 4 — Image and Compose (≈ 1–2 days)

- **`Dockerfile`:** build with `cargo-chef` for target `x86_64-unknown-linux-musl` (plus
  `aarch64-unknown-linux-musl` for ARM hosts), use `FROM gcr.io/distroless/static:nonroot` or
  `alpine:3.23`, and copy only the binary, `templates/`, `static/` and migrations if they are
  not embedded. Prefer embedding `static/` and `templates/` with `include_dir` and
  `sqlx::migrate!`, which leaves a single file in the image. Run as `USER 10001:10001`.
  Health check: `/clipboard-vault healthcheck` (a subcommand), because distroless has no curl.
- **`docker-compose.yml`:** one `vault` service with the `vault-data:/data` volume, plus the
  optional `nginx` and certbot services for people who are not behind another proxy. The
  `postgres` and `auth` services are removed.
- **`.env.example`:** `PUBLIC_BASE_URL`, `VAULT_SECRET` (used for the setup flow and signed
  tokens such as reset links), `SETUP_TOKEN`, `TRUST_PROXY_HEADERS` and the upload limits.
- **`scripts/upgrade.sh`:** keep it for 3.x-to-3.x upgrades. The 2.x-to-3.0 move uses the
  runbook below instead.

### Phase 5 — Publish so any RivetPanel can install it (≈ 0.5 day plus setup)

A one-click template must name an image that every RivetPanel host can pull from an allowed
registry (by default `docker.io`, `ghcr.io` or `quay.io`). Today's images exist only on the
8gb VPS, which is why the current template works nowhere else.

- Publish `ghcr.io/xenycx/clipboard-vault:3.0.0` (and `:3.0`) as a **public** GHCR package for
  `linux/amd64` and `linux/arm64`. This pushes only the image; the source repository can stay
  private and unpushed:

  ```bash
  echo "$GHCR_TOKEN" | docker login ghcr.io -u xenycx --password-stdin   # PAT with write:packages
  docker buildx build --platform linux/amd64,linux/arm64 \
    -t ghcr.io/xenycx/clipboard-vault:3.0.0 -t ghcr.io/xenycx/clipboard-vault:3.0 --push .
  ```

  Then set the package to **Public** under GitHub → Packages → clipboard-vault → Settings.
  Docker Hub (`docker.io/xenycx/clipboard-vault`) works the same way if you prefer it.
- Never move a version tag. Each release gets a new tag, and the template pins it (the
  RivetPanel template tests reject `:latest`).

### Phase 6 — RivetPanel template 3.0 (≈ 0.5 day)

Replace `BotPanel/internal/apptemplates/data/clipboard-vault/compose.yaml` with one service:

```yaml
services:
  app:
    image: ghcr.io/xenycx/clipboard-vault:3.0.0
    user: "10001:10001"
    ports: ["${APP_PORT}:8080"]
    environment:
      PUBLIC_BASE_URL: ${APP_ORIGIN}
      VAULT_SECRET: ${VAULT_SECRET}
      SETUP_TOKEN: ${SETUP_TOKEN}
      DATA_DIR: /data
      TRUST_PROXY_HEADERS: "true"
      SERVER_MAX_UPLOAD_BYTES: "10737418240"
      UPLOAD_DISK_RESERVE_BYTES: "2147483648"
    volumes: ["data:/data"]
    mem_limit: 192m
volumes:
  data: {}
```

In `template.yaml`, keep `web: {service: app, port: 8080, health: /health/live}` and change the
vars to `SETUP_TOKEN` (credential, base64url 48) and `VAULT_SECRET` (advanced, hex 64). Keep the
notes. Add a changelog entry. Run `go test ./internal/apptemplates/` and `make version-check`.
The catalog card will then show "1 container · 192 MiB".

Ship the panel through the normal path (push to `xenycx/rivetpanel` main or tag a release).
New installs get 3.0. Existing 2.x stacks keep running until moved with the runbook.

### Phase 7 — Docs and contracts (≈ 1 day)

- `AGENTS.md`, `src/AGENTS.md`, `README.md`, `DEPLOYMENT.md` and `SECURITY.md`: describe one
  service, SQLite, and native auth. Remove `auth/AGENTS.md` and the bridge invariants; add
  invariants for session hashing, the Origin check and legacy-hash rehashing.
- `tests/test_security_contract.py`: drop the bridge, Nginx-to-auth and Postgres-cast tests;
  add contracts for single-writer transactions, the FTS query escaping, `Origin` enforcement
  and the `TRUST_PROXY_HEADERS` default (off).

## Runbook: moving vault.xenyc.ge from 2.x to 3.0

The 2.x stack is RivetPanel stack `e74fe2ba` (services app `c5abb40e`, auth `b4c11e7a`,
db `5de4da19`) on the 8gb host.

1. **Rehearse.** Install 3.0 from the catalog as a second app (for example on
   `vault3.snappages.click`). Run the import against a copy of production (step 4), check
   sign-in, a few items, a download and the API keys. Then delete the rehearsal app.
2. **Install** 3.0 from the catalog on a new site. Leave the domain on 2.x.
3. **Maintenance window:** stop the 2.x `app` and `auth` services in RivetPanel, leaving `db`
   running.
4. **Import** with a one-off container on the host that joins the 2.x stack's private network
   and mounts the 3.0 data volume:

   ```bash
   docker network ls | grep e74fe2ba        # the 2.x stack network
   sudo docker run --rm --network <that network> --user 10001:10001 \
     -v /var/lib/rivetpanel/workspaces/<3.0 app id>/volumes/data:/data \
     -v /var/lib/rivetpanel/workspaces/c5abb40e-edbb-4549-ab60-2c080c7726f3/volumes/uploads:/old-uploads:ro \
     ghcr.io/xenycx/clipboard-vault:3.0.0 import-postgres \
       --from "postgresql://vault:<DB_PASSWORD>@db:5432/clipboard_vault" \
       --data-dir /data --uploads-from /old-uploads
   ```

   The 2.x `DB_PASSWORD` is visible under the db service's variables in RivetPanel. Stop if
   the count report shows any mismatch.
5. **Switch the domain:** remove `vault.xenyc.ge` from the 2.x site, add and verify it on the
   3.0 site (the TXT record already exists), and set `PUBLIC_BASE_URL=https://vault.xenyc.ge`
   on the 3.0 app. Traefik needs no change; it already sends the host to RivetPanel's sites.
6. **Verify:** `/health/ready`, sign in (expected once), open an image and a long text item,
   upload through the browser and through `vault.py`, and list with an existing API key.
7. **Keep 2.x stopped, not deleted,** for at least a week. To roll back, move the domain back
   and start 2.x `app` and `auth`. Writes made on 3.0 in between are lost.
8. Once you're satisfied, delete the 2.x stack, the old Compose install in `~/ClipboardVault`,
   and the local-only `2.0.0` images.

## Risks and how the plan handles them

| Risk | Mitigation |
| --- | --- |
| SQLite write contention under heavy concurrent uploads | Finalization writes are short; one write connection plus `busy_timeout` serializes them. This scale (a few users) is far below SQLite's limits. Document Postgres-backed 2.x for very large teams if that ever matters. |
| A security regression in auth that Better Auth used to cover | Port Better Auth's behaviors explicitly (the tables above), and get an independent review of `src/identity/` (`/code-review` or `/security-review`) before release. |
| Corrupt backups from copying a live SQLite file | Daily `VACUUM INTO` snapshots in `/data/backups`; the docs say to restore from those, not from the raw `vault.db`. |
| Import mismatches | Dry run, a count report, a file and size check, and the rehearsal on a production copy. |
| Search results change (FTS5 vs `tsvector`) | Keep today's search tests (prefix, multi-word, punctuation, Georgian) and add golden queries from production before cutover. |

## Order and rough total

Phases 1 → 2 → 3 can each merge separately behind the 3.0 branch. 4 → 5 → 6 → 7 follow, then
the runbook. Expect roughly **15–22 working days** for one person. The biggest piece is native
auth (Phase 2); keep it reviewable by landing the bridge replacement first and social sign-in
last.

## Implementation notes

What was built, and where it differs from the text above:

- **Social sign-in** uses `reqwest` (already a dependency) instead of the `oauth2` crate: the
  authorization-code flow with PKCE (S256) for all three providers, and the state, verifier
  and return paths in a 10-minute cookie signed with `VAULT_SECRET` (HMAC-SHA256) and scoped to
  `/api/auth/callback`. The callback URLs are unchanged, so existing provider consoles keep
  working.
- **Static assets are not embedded.** Templates and migrations are compiled into the binary;
  `static/` is copied into the image at `/app/static` and fingerprinted at startup as before.
  The runtime image is `distroless/static`, about 25 MB.
- **Sessions have no `active_workspace` column.** The active workspace is still chosen by the
  `vault_workspace` cookie, validated against memberships, exactly as in 2.x.
- **New subcommand `clipboard-vault backup <file>`** (a `VACUUM INTO` snapshot), used by
  `scripts/upgrade.sh` before every 3.x upgrade.
- **New optional settings:** `BACKUP_KEEP` (default 7, 0 disables snapshots) and
  `DATABASE_READ_CONNECTIONS` (default 8). Retired 2.x variables are ignored with a warning.
- **Rollback within 3.x:** the migrator runs with `set_ignore_missing(true)`, so an older 3.x
  binary starts on a database a newer one migrated. Migrations must stay additive.
- **Write path:** Tus chunk writes and multipart streaming happen outside database
  transactions, serialized by in-process locks (one admission lock, one lock per upload);
  offsets are committed with a conditional `UPDATE … WHERE acknowledged_bytes = ?`.
- **Full-text search** uses an external-content FTS5 table keyed by `vault_items.seq`, an
  `INTEGER PRIMARY KEY`, so rowids survive `VACUUM` and restored snapshots stay searchable.
- **Measured:** idle memory 2.75 MiB in a 192 MiB container (read-only root, uid 10001).
- **Tests:** `tests/app.rs` drives the router end to end on a temporary database;
  `tests/import_postgres.rs` seeds the archived 2.x schema (`migrations-postgres/`) with a
  real Better Auth scrypt hash and API key, imports, and signs in with both (CI runs it with a
  PostgreSQL service).
