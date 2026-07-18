# Security policy

## Supported version

Security fixes are applied to the latest release on the default branch.

## Design

- Nginx is the only publicly exposed container.
- Authentication and database traffic stay on private Docker networking.
- Browser sessions use HttpOnly, Secure, SameSite=Lax cookies in production.
- Every protected data route authenticates through Better Auth or a scoped API key.
- Every item query includes the active organization ID.
- API keys are bound to an organization, shown once, rate-limited, and revocable.
- New accounts cannot access workspace data before global approval.
- First-admin setup requires a private token and closes permanently after use.
- Invitation tokens are random, stored only as SHA-256 hashes, expire, and work once.
- User filenames and virtual paths never become operating-system storage paths.
- File responses use nosniff and potentially active formats download as attachments.
- Uploaded content is never executed by Clipboard Vault.
- Secrets are read from `.env` and excluded from Git.

Password accounts do not prove ownership of their email address because this deployment does
not send email. Global approval and private invitation links are the compensating controls.
For public, high-risk deployments, enable an email provider and require verified addresses.

## Reporting a vulnerability

Do not open a public issue containing secrets, user data, or exploit details. Contact the
repository owner privately with:

- A clear description and impact.
- A minimal reproduction.
- Affected version or commit.
- Any suggested mitigation.

Rotate exposed API keys and deployment secrets immediately. If PostgreSQL or file volumes may
have been accessed, preserve logs, take a backup, and treat stored vault content as compromised.
