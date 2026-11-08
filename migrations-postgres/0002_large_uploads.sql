ALTER TABLE vault_workspace_settings
    ALTER COLUMN max_upload_bytes SET DEFAULT 104857600;

-- Preserve custom limits while moving workspaces that still use the old
-- product default to the new 100 MiB default.
UPDATE vault_workspace_settings
SET max_upload_bytes = 104857600,
    updated_at = now()
WHERE max_upload_bytes = 26214400;

CREATE TABLE vault_upload_sessions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    organization_id TEXT NOT NULL REFERENCES organization(id) ON DELETE CASCADE,
    created_by_user_id TEXT REFERENCES "user"(id) ON DELETE SET NULL,
    created_by_key_id TEXT REFERENCES apikey(id) ON DELETE SET NULL,
    state TEXT NOT NULL DEFAULT 'uploading'
        CHECK (state IN ('uploading','finalizing','completed','failed','canceled','expired')),
    temp_storage_key TEXT NOT NULL UNIQUE,
    original_filename TEXT NOT NULL,
    virtual_path TEXT NOT NULL DEFAULT '/',
    source_url TEXT,
    tags JSONB NOT NULL DEFAULT '[]'::jsonb,
    declared_mime TEXT,
    expected_bytes BIGINT NOT NULL CHECK (expected_bytes > 0),
    acknowledged_bytes BIGINT NOT NULL DEFAULT 0
        CHECK (acknowledged_bytes >= 0 AND acknowledged_bytes <= expected_bytes),
    item_id UUID REFERENCES vault_items(id) ON DELETE SET NULL,
    error_code TEXT,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX vault_upload_sessions_org_state_idx
    ON vault_upload_sessions (organization_id, state, updated_at DESC);
CREATE INDEX vault_upload_sessions_maintenance_idx
    ON vault_upload_sessions (state, expires_at);
