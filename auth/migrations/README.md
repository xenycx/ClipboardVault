# Better Auth schema

`auth-schema.sql` is the reviewed, deterministic schema for Better Auth 1.6.23 and the
configured Admin, Organization, and API Key plugins. PostgreSQL applies it only when a fresh
data volume is initialized. The Rust service applies its own numbered SQLx migrations.

After changing the Better Auth configuration, run `pnpm auth:generate` against a disposable
PostgreSQL database, compare `generated.sql` with this committed migration, and write a new
forward-only migration. Never overwrite a production database schema blindly.

The Google, GitHub, and Discord registry and explicit account-linking policy do not add
tables or columns in Better Auth 1.6.23. All provider identities use the existing `account`
rows and the existing unique `(providerId, accountId)` key, so this release has no auth
schema migration. Keep provider IDs unchanged across upgrades.
