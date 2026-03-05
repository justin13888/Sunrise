import { eq } from "drizzle-orm";
import { db } from "../db";
import { oauthTokens, users } from "../db/schema";

export interface UserTokens {
    userId: string;
    accessToken: string;
    refreshToken: string;
    expiresAt: Date;
    email?: string;
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
                email: tokens.email || "unknown", // Constraint requires email
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

    async getTokens(userId: string): Promise<UserTokens | null> {
        const result = await db
            .select({
                tokens: oauthTokens,
                user: users,
            })
            .from(oauthTokens)
            .leftJoin(users, eq(oauthTokens.userId, users.id))
            .where(eq(oauthTokens.userId, userId))
            .get();

        if (!result) return null;

        return {
            userId: result.user?.id || userId,
            accessToken: result.tokens.accessToken,
            refreshToken: result.tokens.refreshToken,
            expiresAt: result.tokens.expiresAt,
            email: result.user?.email || undefined,
            name: result.user?.name || undefined,
            picture: result.user?.picture || undefined,
        };
    }

    async removeTokens(userId: string): Promise<void> {
        await db.delete(oauthTokens).where(eq(oauthTokens.userId, userId));
    }

    async getAllUserIds(): Promise<string[]> {
        const results = await db.select({ id: users.id }).from(users).all();
        return results.map((r) => r.id);
    }
}

// Global token store instance
export const tokenStore = new TokenStore();
