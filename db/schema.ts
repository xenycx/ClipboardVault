import { sql } from "drizzle-orm";
import {
  index,
  integer,
  sqliteTable,
  text,
  uniqueIndex,
} from "drizzle-orm/sqlite-core";

export const clipboardItems = sqliteTable(
  "clipboard_items",
  {
    id: text("id")
      .primaryKey()
      .$defaultFn(() => crypto.randomUUID()),
    payload: text("payload").notNull(),
    type: text("type", {
      enum: ["text", "html", "code", "url", "image"],
    })
      .notNull()
      .default("text"),
    sourceUrl: text("source_url"),
    contentHash: text("content_hash").notNull(),
    sizeBytes: integer("size_bytes").notNull(),
    createdAt: text("created_at")
      .notNull()
      .default(sql`(strftime('%Y-%m-%dT%H:%M:%fZ','now'))`),
    deletedAt: text("deleted_at"),
    tags: text("tags").notNull().default("[]"),
    pinned: integer("pinned", { mode: "boolean" }).notNull().default(false),
  },
  (table) => [
    uniqueIndex("idx_items_hash").on(table.contentHash),
    index("idx_items_created").on(table.createdAt),
    index("idx_items_type").on(table.type),
    index("idx_items_deleted").on(table.deletedAt),
  ],
);

export const activityLog = sqliteTable(
  "activity_log",
  {
    id: integer("id").primaryKey({ autoIncrement: true }),
    action: text("action").notNull(),
    itemId: text("item_id"),
    detail: text("detail"),
    ip: text("ip"),
    createdAt: text("created_at")
      .notNull()
      .default(sql`(strftime('%Y-%m-%dT%H:%M:%fZ','now'))`),
  },
  (table) => [index("idx_activity_created").on(table.createdAt)],
);

export const vaultSettings = sqliteTable("vault_settings", {
  key: text("key").primaryKey(),
  value: text("value").notNull(),
  updatedAt: text("updated_at")
    .notNull()
    .default(sql`(strftime('%Y-%m-%dT%H:%M:%fZ','now'))`),
});

export type ClipboardItem = typeof clipboardItems.$inferSelect;
export type ActivityEntry = typeof activityLog.$inferSelect;
