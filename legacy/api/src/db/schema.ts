import { integer, sqliteTable, text } from "drizzle-orm/sqlite-core";

export const users = sqliteTable("users", {
    id: text("id").primaryKey(),
    email: text("email").notNull(),
    name: text("name"),
    picture: text("picture"),
});

export const oauthTokens = sqliteTable("oauth_tokens", {
    id: text("id").primaryKey(), // Can be same as userId or random UUID
    userId: text("user_id")
        .references(() => users.id)
        .notNull(),
    accessToken: text("access_token").notNull(),
    refreshToken: text("refresh_token").notNull(),
    expiresAt: integer("expires_at", { mode: "timestamp" }).notNull(),
    scope: text("scope"),
});

export const routines = sqliteTable("routines", {
    id: text("id").primaryKey(),
    userId: text("user_id")
        .references(() => users.id)
        .notNull(),
    name: text("name").notNull(),
    description: text("description"),
    // Stored as JSON strings
    duration: text("duration").notNull(), // Duration JSON
    priority: text("priority").notNull(), // 'high' | 'medium' | 'low'
    flexibility: integer("flexibility").notNull(),
    energyLevelRequired: text("energy_level_required").notNull(),
    category: text("category").notNull(), // UUID string
    frequency: text("frequency").notNull(),
    timePreferences: text("time_preferences").notNull(), // JSON array
    availabilityWindows: text("availability_windows").notNull(), // JSON array
    dependencies: text("dependencies").notNull(), // JSON array
    minimumGapMinutes: integer("minimum_gap_minutes").notNull().default(0),
    bufferTimeMinutes: integer("buffer_time_minutes").notNull().default(5),
    conflictResolution: text("conflict_resolution").notNull(),
    canBeGrouped: integer("can_be_grouped", { mode: "boolean" })
        .notNull()
        .default(true),
    preferredBatchSize: integer("preferred_batch_size"),
    enabled: integer("enabled", { mode: "boolean" }).notNull().default(true),
    tags: text("tags").notNull().default("[]"), // JSON array
    createdAt: integer("created_at", { mode: "timestamp" }).notNull(),
    updatedAt: integer("updated_at", { mode: "timestamp" }).notNull(),
});
