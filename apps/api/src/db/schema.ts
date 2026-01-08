import { sqliteTable, text, integer } from 'drizzle-orm/sqlite-core';

export const users = sqliteTable('users', {
    id: text('id').primaryKey(),
    email: text('email').notNull(),
    name: text('name'),
    picture: text('picture'),
});

export const oauthTokens = sqliteTable('oauth_tokens', {
    id: text('id').primaryKey(), // Can be same as userId or random UUID
    userId: text('user_id').references(() => users.id).notNull(),
    accessToken: text('access_token').notNull(),
    refreshToken: text('refresh_token').notNull(),
    expiresAt: integer('expires_at', { mode: 'timestamp' }).notNull(),
    scope: text('scope'),
});
