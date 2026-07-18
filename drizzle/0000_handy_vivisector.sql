CREATE TABLE `activity_log` (
	`id` integer PRIMARY KEY AUTOINCREMENT NOT NULL,
	`action` text NOT NULL,
	`item_id` text,
	`detail` text,
	`ip` text,
	`created_at` text DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')) NOT NULL
);
--> statement-breakpoint
CREATE INDEX `idx_activity_created` ON `activity_log` (`created_at`);--> statement-breakpoint
CREATE TABLE `clipboard_items` (
	`id` text PRIMARY KEY NOT NULL,
	`payload` text NOT NULL,
	`type` text DEFAULT 'text' NOT NULL,
	`source_url` text,
	`content_hash` text NOT NULL,
	`size_bytes` integer NOT NULL,
	`created_at` text DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')) NOT NULL,
	`deleted_at` text,
	`tags` text DEFAULT '[]' NOT NULL,
	`pinned` integer DEFAULT false NOT NULL
);
--> statement-breakpoint
CREATE UNIQUE INDEX `idx_items_hash` ON `clipboard_items` (`content_hash`);--> statement-breakpoint
CREATE INDEX `idx_items_created` ON `clipboard_items` (`created_at`);--> statement-breakpoint
CREATE INDEX `idx_items_type` ON `clipboard_items` (`type`);--> statement-breakpoint
CREATE INDEX `idx_items_deleted` ON `clipboard_items` (`deleted_at`);--> statement-breakpoint
CREATE TABLE `vault_settings` (
	`key` text PRIMARY KEY NOT NULL,
	`value` text NOT NULL,
	`updated_at` text DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')) NOT NULL
);
