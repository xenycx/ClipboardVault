-- Server-wide administration settings and the 10 GiB default upload ceiling.
--
-- Upgrade safety:
-- * Additive only. Existing users, sessions, API keys, items, and blobs are untouched.
-- * The previous release never reads vault_server_settings, so rolling the containers
--   back after this migration keeps working (it simply ignores the new table).
-- * The auth service reads this table defensively and treats a missing table as
--   "registration open, approval required", which is the previous behavior.

CREATE TABLE IF NOT EXISTS vault_server_settings (
    id BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    registration_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    require_approval BOOLEAN NOT NULL DEFAULT TRUE,
    default_max_upload_bytes BIGINT NOT NULL DEFAULT 10737418240
        CHECK (default_max_upload_bytes > 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by_user_id TEXT REFERENCES "user"(id) ON DELETE SET NULL
);

INSERT INTO vault_server_settings (id) VALUES (TRUE) ON CONFLICT (id) DO NOTHING;

ALTER TABLE vault_workspace_settings
    ALTER COLUMN max_upload_bytes SET DEFAULT 10737418240;

-- Workspaces still on the previous 100 MiB product default move to 10 GiB.
-- Any other explicitly chosen value is preserved. The effective limit is always
-- capped by SERVER_MAX_UPLOAD_BYTES at runtime.
UPDATE vault_workspace_settings
SET max_upload_bytes = 10737418240,
    updated_at = now()
WHERE max_upload_bytes = 104857600;
