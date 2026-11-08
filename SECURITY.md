# Security policy

## Supported version

Security fixes are applied to the latest release on the default branch.

## Design

- One process serves everything. In the Compose install, Nginx is the only publicly exposed
  container and the application network is private; behind another proxy, only that proxy
  reaches the application port.
- There is no internal authentication service and no shared bridge secret: identity checks
  are in-process function calls.
- Browser sessions are 32 random bytes in an HttpOnly, SameSite=Lax cookie (`Secure` when
  `PUBLIC_BASE_URL` is HTTPS). Only the SHA-256 of a session token is stored. Sessions last
  seven days, slide forward at most once a day, and are revoked on ban, password reset, and
  administrator request.
- Passwords are hashed with Argon2id. Hashes imported from 2.x (Better Auth scrypt) verify
  with a constant-time comparison and are replaced with Argon2id on the next sign-in.
- Every state-changing `/api/auth/*` request must carry an `Origin` (or `Referer`) equal to
  `PUBLIC_BASE_URL`. Sign-in, sign-up, password reset and setup are rate limited per client
  address; API keys are limited to 120 requests a minute.
- The client address comes from the TCP peer. `X-Forwarded-For` is read only when
  `TRUST_PROXY_HEADERS=true`, and then only its right-most entry, which the nearest proxy
  appended.
- Every protected data route authenticates with a session or a scoped API key, and every item
  query includes the workspace that session or key resolves to. Clients never choose it.
- API keys are bound to a workspace, shown once, stored only as SHA-256, rate-limited,
  permission-scoped (`items:read`, `items:write`, `items:delete`), and revocable.
- New accounts cannot access workspace data before global approval, unless an administrator
  turns approval off.
- Social sign-in uses the authorization-code flow with PKCE and a state value bound to a
  short-lived cookie signed with `VAULT_SECRET`. Identities link automatically only when the
  provider verifies an email matching the existing account; linking across different emails
  is impossible. Incomplete provider credentials stop the server at startup.
- First-admin setup requires `SETUP_TOKEN` and closes permanently once an administrator exists.
- Invitation and password-reset tokens are random, stored only as SHA-256 hashes, expire, and
  work once.
- User filenames and virtual paths never become operating-system storage paths.
- File responses use `nosniff`; download disposition is available for every format.
- Preview type is verified from content. SVG is inert source, HTML and Markdown are sanitized,
  and uploaded content is never executed by Clipboard Vault.
- Preview, content, and Tus routes reauthorize every request and remain workspace scoped.
- Tus chunks are sequential and offset checked; sessions expire and temporary paths are
  server generated. A completed file is hashed and classified before becoming a vault item.
- New and resumed uploads reserve their outstanding bytes and stop before the configured disk
  reserve. Low disk never triggers automatic deletion.
- The container runs as uid 10001 on a read-only root filesystem and writes only to `/data`
  and `/tmp`.
- Secrets are read from the environment (`.env` in the Compose install) and excluded from Git.

Password accounts do not prove ownership of their email address because this deployment does
not send email. Global approval and private invitation links are the compensating controls.

## Data at rest

`/data/vault.db` holds password hashes, session and API-key hashes, and all text items.
`/data/backups/` holds daily snapshots of that database, and `/data/uploads/` holds file
contents. Protect and back up the whole volume, and treat a leaked copy as a leak of the vault.

## Reporting a vulnerability

Do not open a public issue containing secrets, user data, or exploit details. Contact the
repository owner privately with:

- A clear description and impact.
- A minimal reproduction.
- Affected version or commit.
- Any suggested mitigation.

Rotate exposed API keys and deployment secrets immediately. If the data volume may have been
accessed, preserve logs and treat stored vault content as compromised.
