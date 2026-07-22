# Rust backend guide

This directory is the primary application: Rust 2024, Axum, Askama, SQLx/PostgreSQL, and streamed filesystem storage. Follow the root `AGENTS.md` for system-wide architecture and invariants.

## Module ownership

- `lib.rs`: router composition, shared state, request IDs, compression, tracing, panic handling, CSP, and other security headers.
- `main.rs`: configuration, database startup, migrations, server lifecycle.
- `config.rs`: validated environment configuration and upload limits.
- `auth.rs`: session/API-key extraction and private calls to the auth service.
- `models.rs`: database rows and HTTP payload types.
- `pages.rs`: HTML handlers, page authorization, form actions, and Askama template models.
- `api.rs`: JSON item APIs, downloads, previews, exports, tags, storage reporting, and purge operations.
- `uploads.rs`: Tus protocol, reservations, offsets, chunk writes, finalization, expiry, and cancellation.
- `storage.rs`: filename/path safety, hashing, streaming, MIME classification, deduplication, and physical blob lifecycle.
- `error.rs`: stable application errors and public JSON error envelopes.

## Backend boundaries

- Rust owns vault-domain data and file bytes; the auth service owns users, sessions, memberships, roles, and API keys.
- Use `AuthBridge` for identity and access data. Do not query Better Auth tables from Rust as a shortcut.
- A session or verified API key must establish the organization before any vault query runs.
- Page handlers render templates; JSON handlers return the documented error envelope. Do not mix the two response conventions accidentally.

## Authorization rules

- Treat all public headers, URL parameters, JSON fields, and form values as untrusted.
- Resolve browser identity from the session bridge and API identity from verified key data.
- Scope every item/blob/upload query by organization, even when an item UUID is globally unique.
- Enforce owner/admin/member roles and `items:read`, `items:write`, or `items:delete` permissions in the handler or shared extractor.
- Keep approval and active-workspace behavior consistent between page and API paths.

## Database and storage rules

- Use SQLx binds for values; never interpolate untrusted SQL.
- Keep database metadata and file operations failure-safe. Roll back metadata on failure and clean temporary/partial files.
- Store byte sizes in 64-bit-safe types and keep explicit PostgreSQL `BIGINT` casts for aggregates decoded as `i64`.
- Validate virtual paths; they are labels, not filesystem paths. Never allow `..` to escape storage roots.
- Preserve per-organization deduplication and only remove physical blobs after their final reference is purged.
- Check workspace limits, server limits, current reservations, and the protected disk reserve before accepting uploads.
- Assume PostgreSQL and the upload volume already contain production data on the VPS. Prefer additive migrations and storage formats that both the deployment process and rollback version can handle.
- Before changing a table, enum, identifier, storage key, hash/deduplication rule, or upload-session format, document whether old and new containers can safely run against the same state and whether rollback remains possible.
- Never require destructive schema or file conversion without first flagging the feature as breaking under the root upgrade policy and providing backup and migration steps.

## Upload and preview rules

- Never buffer arbitrary uploads in memory. Stream chunks, hash incrementally, and enforce limits while reading.
- Tus offsets are server-authoritative. Reject mismatches and preserve resumability, expiry, termination, and reservation cleanup.
- Keep Nginx limits and browser chunk behavior aligned when changing upload routes or limits.
- Downloads and PDFs must retain Range support. Unsafe/unknown files must remain downloadable even when not previewable.
- Preview classification must remain conservative. Keep size ceilings, `nosniff`, sandboxing/CSP behavior, and private caching headers.

## Adding or changing an endpoint

1. Register it in `build_router`.
2. Add or reuse a typed request model.
3. Authenticate and authorize before data access.
4. Scope database operations to the resolved organization.
5. Return `AppError` rather than inventing a one-off error shape.
6. Update templates or browser hooks if the endpoint is user-facing.
7. Update route documentation and contract/integration tests.
8. State whether existing browser/API clients and the currently deployed VPS version remain compatible.

## Validation

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
python -m pytest -q tests
```
