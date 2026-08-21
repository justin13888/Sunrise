import { eq } from "drizzle-orm";
import { db } from "../db";
import { oauthTokens, users } from "../db/schema";

export interface UserTokens {
    userId: string;
    accessToken: string;
    refreshToken: string;
    expiresAt: Date;
    email: string;
    name?: string;
    picture?: string;
}

/**
 * Database-backed token storage
 */
export class TokenStore {
    async storeTokens(userId: string, tokens: UserTokens): Promise<void> {
        // 1. Upsert User
        await db
            .insert(users)
            .values({
                id: userId,
                email: tokens.email,
                name: tokens.name,
                picture: tokens.picture,
            })
            .onConflictDoUpdate({
                target: users.id,
                set: {
                    email: tokens.email,
                    name: tokens.name,
                    picture: tokens.picture,
                },
            });

        // 2. Upsert Tokens
        await db
            .insert(oauthTokens)
            .values({
                id: userId, // Simple 1:1 mapping for now
                userId: userId,
                accessToken: tokens.accessToken,
                refreshToken: tokens.refreshToken,
                expiresAt: tokens.expiresAt,
            })
            .onConflictDoUpdate({
                target: oauthTokens.id,
                set: {
                    accessToken: tokens.accessToken,
                    refreshToken: tokens.refreshToken,
                    expiresAt: tokens.expiresAt,
                },
            });
    }

    /**
     * Returns the stored tokens for a user, or null if none exist.
     *
     * Tokens are returned even if the Google access token is expired: the
     * refresh token is the durable credential, and callers refresh access
     * tokens via the Google client as needed.
     */
    async getTokens(userId: string): Promise<UserTokens | null> {
        const result = await db
            .select({
                tokens: oauthTokens,
                user: users,
            })
            .from(oauthTokens)
            .innerJoin(users, eq(oauthTokens.userId, users.id))
            .where(eq(oauthTokens.userId, userId))
            .get();

        if (!result) return null;

        return {
            userId: result.user.id,
            accessToken: result.tokens.accessToken,
            refreshToken: result.tokens.refreshToken,
            expiresAt: result.tokens.expiresAt,
            email: result.user.email,
            name: result.user.name || undefined,
            picture: result.user.picture || undefined,
        };
    }

    async removeTokens(userId: string): Promise<void> {
        await db.delete(oauthTokens).where(eq(oauthTokens.userId, userId));
    }

    /**
     * Returns the IDs of users that currently have stored OAuth tokens
     * (i.e. logged-out users are excluded).
     */
    async getAllUserIds(): Promise<string[]> {
        const results = await db
            .selectDistinct({ userId: oauthTokens.userId })
            .from(oauthTokens)
            .all();
        return results.map((r) => r.userId);
    }
}

// Global token store instance
export const tokenStore = new TokenStore();
