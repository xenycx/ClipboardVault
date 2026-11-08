CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE vault_blobs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    organization_id TEXT NOT NULL REFERENCES organization(id) ON DELETE CASCADE,
    sha256 TEXT NOT NULL,
    storage_key TEXT NOT NULL UNIQUE,
    size_bytes BIGINT NOT NULL CHECK (size_bytes > 0),
    mime_type TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (organization_id, sha256)
);

CREATE TABLE vault_items (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    organization_id TEXT NOT NULL REFERENCES organization(id) ON DELETE CASCADE,
    created_by_user_id TEXT REFERENCES "user"(id) ON DELETE SET NULL,
    created_by_key_id TEXT REFERENCES apikey(id) ON DELETE SET NULL,
    kind TEXT NOT NULL CHECK (kind IN ('text','html','code','url','image','file')),
    text_payload TEXT,
    blob_id UUID REFERENCES vault_blobs(id) ON DELETE RESTRICT,
    original_filename TEXT,
    virtual_path TEXT NOT NULL DEFAULT '/',
    source_url TEXT,
    tags JSONB NOT NULL DEFAULT '[]'::jsonb,
    content_hash TEXT NOT NULL,
    size_bytes BIGINT NOT NULL CHECK (size_bytes > 0),
    pinned BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ,
    CHECK ((text_payload IS NOT NULL) <> (blob_id IS NOT NULL))
);

CREATE INDEX vault_items_org_created_idx ON vault_items (organization_id, created_at DESC);
CREATE INDEX vault_items_org_kind_idx ON vault_items (organization_id, kind);
CREATE INDEX vault_items_org_deleted_idx ON vault_items (organization_id, deleted_at);
CREATE INDEX vault_items_org_path_idx ON vault_items (organization_id, virtual_path);
CREATE INDEX vault_items_tags_idx ON vault_items USING GIN (tags);
CREATE INDEX vault_items_search_idx ON vault_items USING GIN (
    to_tsvector('simple', coalesce(text_payload, '') || ' ' || coalesce(original_filename, '') || ' ' || virtual_path)
);

CREATE TABLE vault_workspace_settings (
    organization_id TEXT PRIMARY KEY REFERENCES organization(id) ON DELETE CASCADE,
    max_upload_bytes BIGINT NOT NULL DEFAULT 26214400 CHECK (max_upload_bytes > 0),
    retention_days INTEGER NOT NULL DEFAULT 30 CHECK (retention_days BETWEEN 1 AND 3650),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by_user_id TEXT REFERENCES "user"(id) ON DELETE SET NULL
);

CREATE TABLE vault_invitations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    organization_id TEXT NOT NULL REFERENCES organization(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    role TEXT NOT NULL CHECK (role IN ('admin','member')),
    created_by_user_id TEXT NOT NULL REFERENCES "user"(id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL,
    redeemed_at TIMESTAMPTZ,
    redeemed_by_user_id TEXT REFERENCES "user"(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX vault_invitations_org_idx ON vault_invitations (organization_id, created_at DESC);

CREATE TABLE vault_activity (
    id BIGSERIAL PRIMARY KEY,
    organization_id TEXT REFERENCES organization(id) ON DELETE CASCADE,
    actor_user_id TEXT REFERENCES "user"(id) ON DELETE SET NULL,
    actor_key_id TEXT REFERENCES apikey(id) ON DELETE SET NULL,
    action TEXT NOT NULL,
    item_id UUID REFERENCES vault_items(id) ON DELETE SET NULL,
    detail JSONB NOT NULL DEFAULT '{}'::jsonb,
    ip_hash TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX vault_activity_org_created_idx ON vault_activity (organization_id, created_at DESC);
