import { env } from "cloudflare:workers";

let schemaReady = false;

export async function ensureVaultSchema() {
  if (schemaReady) return;
  if (!env.DB) throw new Error("D1 binding `DB` is unavailable");

  const db = env.DB;
  await db.batch([
    db.prepare(`CREATE TABLE IF NOT EXISTS clipboard_items (
      id TEXT PRIMARY KEY,
      payload TEXT NOT NULL,
      type TEXT NOT NULL DEFAULT 'text' CHECK (type IN ('text','html','code','url','image')),
      source_url TEXT,
      content_hash TEXT NOT NULL,
      size_bytes INTEGER NOT NULL CHECK (size_bytes > 0),
      created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
      deleted_at TEXT,
      tags TEXT NOT NULL DEFAULT '[]',
      pinned INTEGER NOT NULL DEFAULT 0
    )`),
    db.prepare(`CREATE TABLE IF NOT EXISTS activity_log (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      action TEXT NOT NULL,
      item_id TEXT,
      detail TEXT,
      ip TEXT,
      created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
    )`),
    db.prepare(`CREATE TABLE IF NOT EXISTS vault_settings (
      key TEXT PRIMARY KEY,
      value TEXT NOT NULL,
      updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
    )`),
    db.prepare("CREATE UNIQUE INDEX IF NOT EXISTS idx_items_hash ON clipboard_items(content_hash)"),
    db.prepare("CREATE INDEX IF NOT EXISTS idx_items_created ON clipboard_items(created_at DESC)"),
    db.prepare("CREATE INDEX IF NOT EXISTS idx_items_type ON clipboard_items(type)"),
    db.prepare("CREATE INDEX IF NOT EXISTS idx_items_deleted ON clipboard_items(deleted_at)"),
    db.prepare("CREATE INDEX IF NOT EXISTS idx_activity_created ON activity_log(created_at DESC)"),
  ]);
  schemaReady = true;
}
