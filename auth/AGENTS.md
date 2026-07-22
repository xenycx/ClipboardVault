# Authentication service guide

This directory is a private Node.js 22/TypeScript service built with Hono and Better Auth. It owns identity and access-control data. Follow the root `AGENTS.md` for the complete request flow.

## Files and responsibilities

- `src/auth.ts`: Better Auth configuration, PostgreSQL pool, cookies, account linking, rate limits, approval fields, organizations, workspace roles, and API-key plugin.
- `src/providers.ts`: the typed Google/GitHub/Discord registry, credential-pair validation, identity scopes, and secret-free UI metadata.
- `src/providers.test.ts`: provider enablement, ordering, scopes, validation, and metadata tests.
- `src/index.ts`: Hono application, public Better Auth mount, and private bridge endpoints consumed by Rust.
- `migrations/`: auth schema documentation or generated SQL. Keep it separate from Rust-owned vault migrations.

## Public and private surfaces

- Public `/api/auth/*` traffic reaches this service through Nginx and is handled by Better Auth.
- `/internal/*` is service-to-service only. Nginx must never proxy it publicly.
- Every internal endpoint must verify `AUTH_BRIDGE_SECRET` before reading or mutating data.
- Keep internal responses minimal, typed, and compatible with the Rust structures in `src/models.rs` and calls in `src/auth.rs`/`src/pages.rs`.

## Identity and access rules

- Better Auth is the source of truth for users, sessions, global roles, organizations, memberships, active organization, invitations, and API keys.
- New users remain pending until a global administrator approves them. Approval creates their personal workspace.
- Preserve the distinction between global admin roles and workspace owner/admin/member roles.
- API keys reference an organization. Never let request JSON override the organization established by the key record.
- Keep permission strings aligned with Rust enforcement: `items:read`, `items:write`, and `items:delete`.
- Full API keys and reset/setup secrets are one-time values. Do not log or persist them outside their intended Better Auth storage.

## Security rules

- Keep secure, HTTP-only, SameSite cookies and trusted-origin checks aligned with `BETTER_AUTH_URL`.
- Preserve sign-in, sign-up, and password-reset rate limits.
- Validate requester authorization on every admin, membership, ownership, invitation, and API-key bridge action.
- Use parameterized PostgreSQL queries. Do not expose internal database errors or secrets to callers.
- Do not add a second authentication path in Rust or trust proxy-injected user headers.
- Keep provider IDs stable. Enable providers only from complete credential pairs, request identity/email scopes only, and never synthesize an email address.
- Preserve verified-email implicit linking with `allowDifferentEmails`, `allowUnlinkingAll`, and `updateUserInfoOnLink` disabled. Do not add `trustedProviders` without an explicit security review and migration classification.

## Production upgrade rules

- The auth database is live on the VPS and contains users, sessions, organizations, memberships, invitations, approval state, and API keys.
- Treat Better Auth/plugin upgrades, role changes, cookie changes, key formats, and generated schema changes as compatibility-sensitive production migrations.
- Before changing auth configuration or schema, state whether existing sessions, passwords, organizations, API keys, and Rust bridge models remain valid.
- Prefer additive, ordered migrations. If old and new auth containers cannot share the schema or rollback would invalidate credentials, classify the change as breaking and document the maintenance, backup, migration, and rollback limits before implementation.

## Cross-service changes

When an internal response or command changes:

1. Update the TypeScript endpoint and its validation.
2. Update the matching Rust bridge call and Serde model.
3. Confirm Nginx still exposes only `/api/auth/*`, not `/internal/*`.
4. Update contract or integration tests.
5. Generate and review schema changes when Better Auth configuration changes the database shape.

## Validation

```bash
pnpm install --frozen-lockfile
pnpm test
pnpm build
```

From the repository root, also run `python -m pytest -q tests` for the private-bridge and workspace-binding contracts.
