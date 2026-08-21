# Clipboard Vault agent guide

Clipboard Vault is a self-hosted workspace for explicitly submitted text, code, links, and files. It is intentionally split into a primary Rust application, a small authentication service, a server-rendered frontend, and deployment infrastructure.

## Live deployment and upgrade policy

This application is deployed on a VPS. Treat every change as an upgrade to a live production installation with existing users, PostgreSQL data, authentication records, API keys, environment configuration, and files on a persistent volume. Do not reason about this repository as a disposable or greenfield deployment.

For every requested feature, bug fix, dependency upgrade, or architectural change, provide an explicit compatibility impact before implementation and repeat the important result in the final handoff. Use one of these classifications:

- **Backward-compatible:** can be deployed over the current VPS version without changing existing data, configuration, clients, or rollback behavior.
- **Migration required:** preserves existing data but requires an ordered database migration, environment/configuration update, service restart, client update, or deployment step.
- **Breaking or not safely upgradeable:** invalidates existing data/configuration/API behavior, requires downtime or destructive conversion, prevents old containers from reading new state, or cannot be rolled back safely.

The compatibility impact must identify every affected surface, not merely say that a change is breaking. Check PostgreSQL schemas, Better Auth schemas, API and form contracts, stored file layout, Tus upload sessions, persistent volumes, environment variables, Docker Compose, Nginx, browser/API clients, backups, deployment order, and rollback behavior as applicable.

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
| Rust application and API | `src/AGENTS.md` | Axum routes, authorization, PostgreSQL metadata, file storage, previews, and resumable uploads |
| Authentication service | `auth/AGENTS.md` | Hono, Better Auth, sessions, approvals, organizations, roles, and API keys |
| Server-rendered UI | `templates/AGENTS.md` | Askama templates, page forms, semantic markup, and backend/template contracts |
| Browser assets | `static/AGENTS.md` | Shared design system, themes, client behavior, uploads, previews, and DOM hooks |

A frontend change commonly touches both `templates/` and `static/`; read both frontend guides. Cross-cutting behavior may also require the relevant backend guide. Root-level deployment, migration, test, or documentation work follows this file.

## Architecture

- Nginx is the only public entry point in production.
- `/api/auth/*` is proxied directly to the Hono/Better Auth service on port 3001.
- Page routes, `/api/v1/*`, health checks, and `/static/*` are served by the Rust/Axum application on port 8080.
- The Rust service renders Askama templates and serves the browser assets without a frontend build step.
- Rust calls the auth service over private `/internal/*` endpoints protected by `AUTH_BRIDGE_SECRET` to resolve sessions, verify API keys, and manage accounts or workspaces.
- Both services use PostgreSQL. Better Auth owns identity, sessions, organizations, memberships, and API-key records. Rust owns vault items, blob metadata, upload sessions, and activity data.
- File bytes are streamed to a persistent volume. PostgreSQL stores their metadata and references; large payloads must not be buffered in memory.

## Request and data flow

### Browser page request

1. Nginx forwards the request to Rust.
2. Rust sends the session cookie to the private auth bridge.
3. The auth service returns the user, approval status, active organization, role, and memberships.
4. Rust authorizes the workspace, queries vault data, constructs an Askama template model, and renders HTML.
5. `templates/base.html` loads `/static/theme.js` (blocking, so the stored theme is applied before first paint), `/static/app.css`, `/static/app.js`, and vendored preview libraries.

### Browser authentication request

1. `static/app.js` posts to `/api/auth/*`.
2. Nginx forwards the request directly to Better Auth.
3. Better Auth updates PostgreSQL and returns or clears the secure session cookie.
4. The browser navigates back to a Rust-rendered page, where Rust validates the session through the private bridge.

### API or upload request

1. Rust accepts a session cookie, `Authorization: Bearer`, or `X-API-Key` where supported.
2. API keys are verified by the auth bridge; the verified key determines the organization and permissions. Clients may not choose another organization ID.
3. Rust validates role or key permissions, updates metadata transactionally, and streams file bytes to or from the volume.
4. Multipart uploads are the compatibility path; large uploads use the Tus endpoints in `src/uploads.rs` and the resumable client in `static/app.js`.

## Repository map

- `src/lib.rs`: application state, router, shared middleware, security headers.
- `src/pages.rs`: server-rendered page handlers and Askama view models.
- `src/api.rs`: JSON APIs, item operations, storage status, content, and previews.
- `src/uploads.rs`: Tus creation, offsets, chunk streaming, finalization, and cleanup.
- `src/auth.rs`: Rust client for the private auth bridge and request authentication.
- `src/storage.rs`: safe filenames, streaming, hashing, deduplication, and blob persistence.
- `auth/src/auth.ts`: Better Auth configuration and access-control model.
- `auth/src/providers.ts`: typed social-provider credentials, scopes, and public metadata.
- `auth/src/index.ts`: public auth mount and private bridge endpoints.
- `templates/`: Askama HTML templates.
- `static/app.css`: shared two-theme visual system and the application shell.
- `static/app.js`: browser behavior, navigation, command palette, auth forms, filtering, uploads, previews, and copy actions.
- `static/theme.js`: pre-paint theme selection; separate from `app.js` because the CSP forbids inline scripts.
- `migrations/`: Rust-owned vault schema migrations.
- `auth/migrations/`: auth-service schema material.
- `nginx/`: public routing, request limits, and HTTPS configuration.
- `tests/test_security_contract.py`: cross-file security and architecture contracts.

## System invariants

- Never read or monitor the user's clipboard. Only explicit `navigator.clipboard.writeText` copy actions are allowed.
- Never trust identity headers supplied by the public request. Rust removes legacy identity headers and authenticates through the bridge.
- Never accept a caller-provided organization as authority. Resolve it from the session or verified API key and enforce role or permission checks server-side.
- Keep `/internal/*` private. Do not expose it through Nginx, and require the bridge secret on every internal auth endpoint.
- Stream uploads and downloads. Preserve size ceilings, free-space reserve checks, safe paths, authoritative Tus offsets, and partial-file cleanup.
- Sanitize rendered Markdown and HTML. Preserve sandboxing, restrictive security headers, forced downloads, and preview size limits.
- Never commit `.env`, secrets, complete API keys, setup tokens, OAuth credentials, or production database URLs.
- Never write inline `<script>` blocks or `on*=` handlers in templates. The page CSP is
  `script-src 'self'` with no nonce, so inline JavaScript is silently dead in production.
- Never reference `/static/...` from a template without the `|asset` filter. The fingerprint it
  appends is what stops a browser from pairing new markup with a cached old stylesheet.

## Cross-layer change checklist

When changing a user-facing capability, check all contracts it crosses:

1. Route registration in `src/lib.rs` or `auth/src/index.ts`.
2. Authorization and organization scoping.
3. Request/response model or Askama view model.
4. Template markup and `data-*` hooks.
5. Browser behavior and error handling.
6. PostgreSQL migration or file-storage lifecycle, if state changes.
7. Nginx body/streaming rules for new upload endpoints.
8. Security-contract and service tests.
9. README, deployment, or API documentation when public behavior changes.
10. VPS upgrade classification, ordered deployment steps, backup needs, and rollback compatibility.

## Validation

Run the checks relevant to the changed scope:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cd auth && pnpm install --frozen-lockfile && pnpm test && pnpm build
python -m pytest -q tests
docker compose config --quiet
```

The GitHub Actions workflow builds each container image once, loads those exact images into the integration stack, and exercises all enabled social-provider redirects. On Windows, Rust compilation needs a compatible linker; use CI, Docker, WSL, or Linux when it is unavailable locally.
