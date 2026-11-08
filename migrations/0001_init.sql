-- Clipboard Vault 3.0 schema: identity and vault data in one SQLite database.
--
-- Rules every query relies on:
-- * Identifiers are TEXT. Item, blob, upload and invitation IDs are hyphenated UUIDs.
-- * Timestamps are TEXT in one fixed-width UTC format written by Rust
--   (`YYYY-MM-DDTHH:MM:SS.ffffffZ`, see `db::ts`). SQL never generates a timestamp,
--   because mixing formats would break ordering and range comparisons.
-- * JSON (tags, activity detail, API-key permissions) is stored as TEXT.
-- * Byte counts are INTEGER, which SQLite stores as 64-bit.

-- ---------------------------------------------------------------- identity

CREATE TABLE users (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    email TEXT NOT NULL UNIQUE COLLATE NOCASE,
    email_verified INTEGER NOT NULL DEFAULT 0,
    image TEXT,
    -- Argon2id PHC string, or a legacy Better Auth scrypt `salt:key` (rehashed on sign-in).
    -- NULL for accounts that only sign in through a social provider.
    password_hash TEXT,
    role TEXT NOT NULL DEFAULT 'user',
    approval_status TEXT NOT NULL DEFAULT 'pending'
        CHECK (approval_status IN ('pending', 'approved', 'rejected')),
    approved_at TEXT,
    approved_by TEXT REFERENCES users(id) ON DELETE SET NULL,
    banned INTEGER NOT NULL DEFAULT 0,
    ban_reason TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE oauth_accounts (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    provider_account_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE (provider, provider_account_id)
);
CREATE INDEX oauth_accounts_user_idx ON oauth_accounts (user_id);

-- Only the SHA-256 of a session token is stored; the token lives in the cookie.
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    token_hash TEXT NOT NULL UNIQUE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    ip_address TEXT,
    user_agent TEXT
);
CREATE INDEX sessions_user_idx ON sessions (user_id);
CREATE INDEX sessions_expires_idx ON sessions (expires_at);

CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    slug TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL
);

CREATE TABLE memberships (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role TEXT NOT NULL DEFAULT 'member',
    created_at TEXT NOT NULL,
    UNIQUE (workspace_id, user_id)
);
CREATE INDEX memberships_user_idx ON memberships (user_id);

-- `key_hash` is SHA-256 of the full key, base64url without padding: the same
-- value Better Auth stored, so keys issued by 2.x verify unchanged.
CREATE TABLE api_keys (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    created_by_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
    name TEXT,
    start TEXT,
    key_hash TEXT NOT NULL UNIQUE,
    permissions TEXT NOT NULL DEFAULT '{}',
    enabled INTEGER NOT NULL DEFAULT 1,
    expires_at TEXT,
    request_count INTEGER NOT NULL DEFAULT 0,
    last_request_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX api_keys_workspace_idx ON api_keys (workspace_id, created_at DESC);

CREATE TABLE password_resets (
    token_hash TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at TEXT NOT NULL,
    used_at TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX password_resets_user_idx ON password_resets (user_id);

CREATE TABLE server_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    registration_enabled INTEGER NOT NULL DEFAULT 1,
    require_approval INTEGER NOT NULL DEFAULT 1,
    default_max_upload_bytes INTEGER NOT NULL DEFAULT 10737418240
        CHECK (default_max_upload_bytes > 0),
    updated_at TEXT NOT NULL,
    updated_by_user_id TEXT REFERENCES users(id) ON DELETE SET NULL
);
INSERT INTO server_settings (id, updated_at) VALUES (1, '1970-01-01T00:00:00.000000Z');

-- ---------------------------------------------------------------- vault

CREATE TABLE vault_blobs (
    id TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    sha256 TEXT NOT NULL,
    storage_key TEXT NOT NULL UNIQUE,
    size_bytes INTEGER NOT NULL CHECK (size_bytes > 0),
    mime_type TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE (organization_id, sha256)
);

-- `seq` aliases the rowid so the full-text index keeps pointing at the right row
-- after VACUUM; a table without an INTEGER PRIMARY KEY may renumber its rowids.
CREATE TABLE vault_items (
    seq INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    organization_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    created_by_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
    created_by_key_id TEXT REFERENCES api_keys(id) ON DELETE SET NULL,
    kind TEXT NOT NULL CHECK (kind IN ('text', 'html', 'code', 'url', 'image', 'file')),
    text_payload TEXT,
    -- NO ACTION (checked at the end of the statement), so deleting a workspace can
    -- cascade to its items and blobs in either order.
    blob_id TEXT REFERENCES vault_blobs(id),
    original_filename TEXT,
    virtual_path TEXT NOT NULL DEFAULT '/',
    source_url TEXT,
    tags TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(tags)),
    content_hash TEXT NOT NULL,
    size_bytes INTEGER NOT NULL CHECK (size_bytes > 0),
    pinned INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    deleted_at TEXT,
    CHECK ((text_payload IS NOT NULL) <> (blob_id IS NOT NULL))
);
CREATE INDEX vault_items_org_created_idx ON vault_items (organization_id, created_at DESC);
CREATE INDEX vault_items_org_kind_idx ON vault_items (organization_id, kind);
CREATE INDEX vault_items_org_deleted_idx ON vault_items (organization_id, deleted_at);
CREATE INDEX vault_items_org_path_idx ON vault_items (organization_id, virtual_path);
CREATE INDEX vault_items_blob_idx ON vault_items (blob_id);

-- Full-text search over the same three fields the 2.x tsvector index covered.
-- unicode61 treats Georgian and other non-Latin letters as word characters.
CREATE VIRTUAL TABLE vault_items_fts USING fts5(
    text_payload,
    original_filename,
    virtual_path,
    content = 'vault_items',
    content_rowid = 'seq',
    tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TRIGGER vault_items_fts_insert AFTER INSERT ON vault_items BEGIN
    INSERT INTO vault_items_fts (rowid, text_payload, original_filename, virtual_path)
    VALUES (new.seq, new.text_payload, new.original_filename, new.virtual_path);
END;

CREATE TRIGGER vault_items_fts_delete AFTER DELETE ON vault_items BEGIN
    INSERT INTO vault_items_fts (vault_items_fts, rowid, text_payload, original_filename, virtual_path)
    VALUES ('delete', old.seq, old.text_payload, old.original_filename, old.virtual_path);
END;

CREATE TRIGGER vault_items_fts_update
AFTER UPDATE OF text_payload, original_filename, virtual_path ON vault_items BEGIN
    INSERT INTO vault_items_fts (vault_items_fts, rowid, text_payload, original_filename, virtual_path)
    VALUES ('delete', old.seq, old.text_payload, old.original_filename, old.virtual_path);
    INSERT INTO vault_items_fts (rowid, text_payload, original_filename, virtual_path)
    VALUES (new.seq, new.text_payload, new.original_filename, new.virtual_path);
END;

CREATE TABLE vault_workspace_settings (
    organization_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    max_upload_bytes INTEGER NOT NULL DEFAULT 10737418240 CHECK (max_upload_bytes > 0),
    retention_days INTEGER NOT NULL DEFAULT 30 CHECK (retention_days BETWEEN 1 AND 3650),
    updated_at TEXT NOT NULL,
    updated_by_user_id TEXT REFERENCES users(id) ON DELETE SET NULL
);

CREATE TABLE vault_invitations (
    id TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    role TEXT NOT NULL CHECK (role IN ('admin', 'member')),
    created_by_user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at TEXT NOT NULL,
    redeemed_at TEXT,
    redeemed_by_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX vault_invitations_org_idx ON vault_invitations (organization_id, created_at DESC);

CREATE TABLE vault_activity (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    organization_id TEXT REFERENCES workspaces(id) ON DELETE CASCADE,
    actor_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
    actor_key_id TEXT REFERENCES api_keys(id) ON DELETE SET NULL,
    action TEXT NOT NULL,
    item_id TEXT REFERENCES vault_items(id) ON DELETE SET NULL,
    detail TEXT NOT NULL DEFAULT '{}',
    ip_hash TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX vault_activity_org_created_idx ON vault_activity (organization_id, created_at DESC);

CREATE TABLE vault_upload_sessions (
    id TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    created_by_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
    created_by_key_id TEXT REFERENCES api_keys(id) ON DELETE SET NULL,
    state TEXT NOT NULL DEFAULT 'uploading'
        CHECK (state IN ('uploading', 'finalizing', 'completed', 'failed', 'canceled', 'expired')),
    temp_storage_key TEXT NOT NULL UNIQUE,
    original_filename TEXT NOT NULL,
    virtual_path TEXT NOT NULL DEFAULT '/',
    source_url TEXT,
    tags TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(tags)),
    declared_mime TEXT,
    expected_bytes INTEGER NOT NULL CHECK (expected_bytes > 0),
    acknowledged_bytes INTEGER NOT NULL DEFAULT 0
        CHECK (acknowledged_bytes >= 0 AND acknowledged_bytes <= expected_bytes),
    item_id TEXT REFERENCES vault_items(id) ON DELETE SET NULL,
    error_code TEXT,
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX vault_upload_sessions_org_state_idx
    ON vault_upload_sessions (organization_id, state, updated_at DESC);
CREATE INDEX vault_upload_sessions_maintenance_idx
    ON vault_upload_sessions (state, expires_at);
