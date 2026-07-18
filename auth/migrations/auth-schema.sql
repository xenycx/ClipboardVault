-- Better Auth 1.6.23 schema for the plugins configured in auth/src/auth.ts.
-- This migration is deterministic and is applied before the Rust vault schema.

CREATE TABLE IF NOT EXISTS "user" (
    id text PRIMARY KEY,
    name text NOT NULL,
    email text NOT NULL UNIQUE,
    "emailVerified" boolean NOT NULL DEFAULT false,
    image text,
    "createdAt" timestamptz NOT NULL DEFAULT now(),
    "updatedAt" timestamptz NOT NULL DEFAULT now(),
    role text,
    banned boolean NOT NULL DEFAULT false,
    "banReason" text,
    "banExpires" timestamptz,
    "approvalStatus" text NOT NULL DEFAULT 'pending'
        CHECK ("approvalStatus" IN ('pending', 'approved', 'rejected')),
    "approvedAt" timestamptz,
    "approvedBy" text REFERENCES "user"(id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS session (
    id text PRIMARY KEY,
    "expiresAt" timestamptz NOT NULL,
    token text NOT NULL UNIQUE,
    "createdAt" timestamptz NOT NULL DEFAULT now(),
    "updatedAt" timestamptz NOT NULL DEFAULT now(),
    "ipAddress" text,
    "userAgent" text,
    "userId" text NOT NULL REFERENCES "user"(id) ON DELETE CASCADE,
    "impersonatedBy" text,
    "activeOrganizationId" text
);
CREATE INDEX IF NOT EXISTS session_user_id_idx ON session ("userId");

CREATE TABLE IF NOT EXISTS account (
    id text PRIMARY KEY,
    "accountId" text NOT NULL,
    "providerId" text NOT NULL,
    "userId" text NOT NULL REFERENCES "user"(id) ON DELETE CASCADE,
    "accessToken" text,
    "refreshToken" text,
    "idToken" text,
    "accessTokenExpiresAt" timestamptz,
    "refreshTokenExpiresAt" timestamptz,
    scope text,
    password text,
    "createdAt" timestamptz NOT NULL DEFAULT now(),
    "updatedAt" timestamptz NOT NULL DEFAULT now(),
    UNIQUE ("providerId", "accountId")
);
CREATE INDEX IF NOT EXISTS account_user_id_idx ON account ("userId");

CREATE TABLE IF NOT EXISTS verification (
    id text PRIMARY KEY,
    identifier text NOT NULL,
    value text NOT NULL,
    "expiresAt" timestamptz NOT NULL,
    "createdAt" timestamptz NOT NULL DEFAULT now(),
    "updatedAt" timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS verification_identifier_idx ON verification (identifier);

CREATE TABLE IF NOT EXISTS organization (
    id text PRIMARY KEY,
    name text NOT NULL,
    slug text NOT NULL UNIQUE,
    logo text,
    "createdAt" timestamptz NOT NULL,
    metadata text
);
CREATE INDEX IF NOT EXISTS organization_slug_idx ON organization (slug);

CREATE TABLE IF NOT EXISTS member (
    id text PRIMARY KEY,
    "organizationId" text NOT NULL REFERENCES organization(id) ON DELETE CASCADE,
    "userId" text NOT NULL REFERENCES "user"(id) ON DELETE CASCADE,
    role text NOT NULL DEFAULT 'member',
    "createdAt" timestamptz NOT NULL,
    UNIQUE ("organizationId", "userId")
);
CREATE INDEX IF NOT EXISTS member_organization_id_idx ON member ("organizationId");
CREATE INDEX IF NOT EXISTS member_user_id_idx ON member ("userId");

CREATE TABLE IF NOT EXISTS invitation (
    id text PRIMARY KEY,
    "organizationId" text NOT NULL REFERENCES organization(id) ON DELETE CASCADE,
    email text NOT NULL,
    role text,
    status text NOT NULL DEFAULT 'pending',
    "expiresAt" timestamptz NOT NULL,
    "createdAt" timestamptz NOT NULL DEFAULT now(),
    "inviterId" text NOT NULL REFERENCES "user"(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS invitation_organization_id_idx ON invitation ("organizationId");
CREATE INDEX IF NOT EXISTS invitation_email_idx ON invitation (email);

CREATE TABLE IF NOT EXISTS apikey (
    id text PRIMARY KEY,
    "configId" text NOT NULL DEFAULT 'default',
    name text,
    start text,
    "referenceId" text NOT NULL REFERENCES organization(id) ON DELETE CASCADE,
    prefix text,
    key text NOT NULL,
    "refillInterval" integer,
    "refillAmount" integer,
    "lastRefillAt" timestamptz,
    enabled boolean NOT NULL DEFAULT true,
    "rateLimitEnabled" boolean NOT NULL DEFAULT true,
    "rateLimitTimeWindow" integer NOT NULL DEFAULT 60000,
    "rateLimitMax" integer NOT NULL DEFAULT 120,
    "requestCount" integer NOT NULL DEFAULT 0,
    remaining integer,
    "lastRequest" timestamptz,
    "expiresAt" timestamptz,
    "createdAt" timestamptz NOT NULL,
    "updatedAt" timestamptz NOT NULL,
    permissions text,
    metadata text
);
CREATE INDEX IF NOT EXISTS apikey_config_id_idx ON apikey ("configId");
CREATE INDEX IF NOT EXISTS apikey_reference_id_idx ON apikey ("referenceId");
CREATE UNIQUE INDEX IF NOT EXISTS apikey_key_idx ON apikey (key);
