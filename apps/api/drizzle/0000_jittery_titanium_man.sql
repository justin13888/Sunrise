CREATE TABLE `oauth_tokens` (
	`id` text PRIMARY KEY NOT NULL,
	`user_id` text NOT NULL,
	`access_token` text NOT NULL,
	`refresh_token` text NOT NULL,
	`expires_at` integer NOT NULL,
	`scope` text,
	FOREIGN KEY (`user_id`) REFERENCES `users`(`id`) ON UPDATE no action ON DELETE no action
);
--> statement-breakpoint
CREATE TABLE `routines` (
	`id` text PRIMARY KEY NOT NULL,
	`user_id` text NOT NULL,
	`name` text NOT NULL,
	`description` text,
	`duration` text NOT NULL,
	`priority` text NOT NULL,
	`flexibility` integer NOT NULL,
	`energy_level_required` text NOT NULL,
	`category` text NOT NULL,
	`frequency` text NOT NULL,
	`time_preferences` text NOT NULL,
	`availability_windows` text NOT NULL,
	`dependencies` text NOT NULL,
	`minimum_gap_minutes` integer DEFAULT 0 NOT NULL,
	`buffer_time_minutes` integer DEFAULT 5 NOT NULL,
	`conflict_resolution` text NOT NULL,
	`can_be_grouped` integer DEFAULT true NOT NULL,
	`preferred_batch_size` integer,
	`enabled` integer DEFAULT true NOT NULL,
	`tags` text DEFAULT '[]' NOT NULL,
	`created_at` integer NOT NULL,
	`updated_at` integer NOT NULL,
	FOREIGN KEY (`user_id`) REFERENCES `users`(`id`) ON UPDATE no action ON DELETE no action
);
--> statement-breakpoint
CREATE TABLE `users` (
	`id` text PRIMARY KEY NOT NULL,
	`email` text NOT NULL,
	`name` text,
	`picture` text
);
