# Rust backend guide

This directory is the whole application: Rust 2024, Axum, Askama, SQLx/SQLite, native identity, and streamed filesystem storage. Follow the root `AGENTS.md` for system-wide architecture and invariants.

## Module ownership

- `lib.rs`: router composition (`build_router`, `auth_routes`), shared state, request IDs, compression, tracing, panic handling, CSP, and other security headers.
- `main.rs`: subcommands (`serve`, `healthcheck`, `backup`, `import-postgres`), startup, and shutdown.
- `config.rs`: validated environment configuration, upload limits, and the list of retired 2.x variables.
- `db.rs`: the read and write pools, pragmas, `Db::begin` (`BEGIN IMMEDIATE`), the stored timestamp format (`ts`, `now`), and `VACUUM INTO` snapshots.
- `identity/mod.rs`: session cookies, session resolution, approval, personal workspaces, sliding expiry.
- `identity/routes.rs`: public `/api/auth/*` handlers with Origin checks and rate limits.
- `identity/password.rs`: Argon2id hashing and legacy Better Auth scrypt verification.
- `identity/keys.rs`: API key creation, listing, revocation, and verification.
- `identity/workspaces.rs`: membership rules, ownership transfer, workspace deletion.
- `identity/admin.rs`: global administration with its safety rules.
- `identity/oauth.rs`: Google, GitHub, and Discord sign-in (PKCE, signed state cookie).
- `identity/rate_limit.rs`: in-memory fixed-window limits and client-address resolution.
- `auth.rs`: request authentication for vault routes (API key first, then session).
- `models.rs`: database rows and HTTP payload types.
- `pages.rs`: HTML handlers, page authorization, form actions, and Askama template models.
- `api.rs`: JSON item APIs, search, downloads, previews, exports, tags, storage reporting, and purge operations.
- `uploads.rs`: Tus protocol, reservations, offsets, chunk writes, finalization, expiry, and cancellation.
- `storage.rs`: filename/path safety, hashing, streaming, MIME classification, deduplication, and physical blob lifecycle.
- `backup.rs`: daily snapshots, retention, and the admin download.
- `import.rs`: the read-only 2.x PostgreSQL import.
- `error.rs`: stable application errors and public JSON error envelopes.

## Backend boundaries

- Identity functions live in `identity/`. Page and API handlers call them; they do not reimplement membership or role checks with ad hoc SQL.
- A session or verified API key must establish the workspace before any vault query runs.
- Page handlers render templates; JSON handlers return the documented error envelope; `/api/auth/*` returns `{code, message}` as `static/app.js` expects. Do not mix the conventions accidentally.

## Authorization rules

- Treat all public headers, URL parameters, JSON fields, cookies, and form values as untrusted.
- Resolve browser identity from the hashed session cookie and API identity from the verified key row.
- The `vault_workspace` cookie only selects among the user's own memberships; it never grants access.
- Scope every item/blob/upload query by organization, even when an item UUID is globally unique.
- Enforce owner/admin/member roles and `items:read`, `items:write`, or `items:delete` permissions in the handler or shared extractor.
- Keep approval and active-workspace behavior consistent between page and API paths.
- Keep the administrator safety rules: nobody disables, demotes, or deletes themselves; administrators are demoted before being disabled, rejected, or deleted; deletion needs the exact email; only the owner transfers or deletes a workspace, and deletion needs its exact name.

## Database and storage rules

- Use SQLx binds for values; never interpolate untrusted SQL. Expand ID lists with `json_each(?)` and one JSON-array bind.
- Read with `db.read`; write with `db.write`; group related writes in `db.begin()`. Keep write transactions short: no hashing, streaming, copying, or network calls inside them. Use the in-process locks (`upload_admission`, per-upload locks) for serialization that spans file work.
- Store timestamps only through `db::ts`/`db::now`; bind IDs as hyphenated text (`Uuid::hyphenated()`, `db::Id`).
- Validate virtual paths; they are labels, not filesystem paths. Never allow `..` to escape storage roots.
- Preserve per-organization deduplication and only remove physical blobs after their final reference is purged and the transaction has committed.
- Check workspace limits, server limits, current reservations, and the protected disk reserve before accepting uploads.
- Assume the volume already contains production data. Migrations must be additive (new tables, new nullable or defaulted columns, new indexes): an older 3.x binary must still start on the migrated database.
- Before changing a table, identifier, storage key, hash/deduplication rule, cookie, or upload-session format, document whether old and new binaries can run against the same state and whether rollback remains possible.
- Never require destructive schema or file conversion without first flagging the feature as breaking under the root upgrade policy and providing backup and migration steps.
- `import.rs` is the only code that talks to PostgreSQL, and it only reads.

## Upload and preview rules

- Never buffer arbitrary uploads in memory. Stream chunks, hash incrementally, and enforce limits while reading.
- Tus offsets are server-authoritative. Reject mismatches and preserve resumability, expiry, termination, and reservation cleanup.
- Keep Nginx limits and browser chunk behavior aligned when changing upload routes or limits.
- Downloads and PDFs must retain Range support. Unsafe/unknown files must remain downloadable even when not previewable.
- Preview classification must remain conservative. Keep size ceilings, `nosniff`, sandboxing/CSP behavior, and private caching headers.

## Adding or changing an endpoint

1. Register it in `build_router` (or `auth_routes` for `/api/auth/*`).
2. Add or reuse a typed request model.
3. Authenticate and authorize before data access. Auth routes check `Origin` and the rate limit first.
4. Scope database operations to the resolved organization.
5. Return `AppError` (or `auth_error` under `/api/auth/*`) rather than inventing a one-off error shape.
6. Update templates or browser hooks if the endpoint is user-facing.
7. Add a case to `tests/app.rs` and update route documentation and contracts.
8. State whether existing browser/API clients and the currently deployed version remain compatible.

## Validation

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
python -m pytest -q tests
```
