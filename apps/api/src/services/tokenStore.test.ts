import { beforeEach, describe, expect, it, vi } from "vitest";
import type { UserTokens } from "./tokenStore";

// Mock the db module with a real in-memory SQLite database.
// vi.mock is hoisted above imports, so the factory runs before TokenStore is imported.
vi.mock("../db", async () => {
    const { default: Database } = await import("better-sqlite3");
    const { drizzle } = await import("drizzle-orm/better-sqlite3");
    const schema = await import("../db/schema");

    const sqlite = new Database(":memory:");
    sqlite.exec(`
        CREATE TABLE IF NOT EXISTS users (
            id TEXT PRIMARY KEY,
            email TEXT NOT NULL,
            name TEXT,
            picture TEXT
        );
        CREATE TABLE IF NOT EXISTS oauth_tokens (
            id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL REFERENCES users(id),
            access_token TEXT NOT NULL,
            refresh_token TEXT NOT NULL,
            expires_at INTEGER NOT NULL,
            scope TEXT
        );
    `);

    const db = drizzle(sqlite, { schema });

    // Expose sqlite for cleanup between tests
    (globalThis as Record<string, unknown>).__testSqlite = sqlite;

    return { db };
});

import { TokenStore } from "./tokenStore";

function getSqlite() {
    return (globalThis as Record<string, unknown>).__testSqlite as {
        exec: (sql: string) => void;
    };
}

describe("TokenStore", () => {
    let tokenStore: TokenStore;

    beforeEach(() => {
        tokenStore = new TokenStore();
        // Clean all data between tests
        const sqlite = getSqlite();
        sqlite.exec("DELETE FROM oauth_tokens; DELETE FROM users;");
    });

    describe("storeTokens", () => {
        it("should store tokens for a new user", async () => {
            const userTokens: UserTokens = {
                userId: "user1",
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                expiresAt: new Date(Date.now() + 3600 * 1000),
                email: "user1@example.com",
                name: "User One",
            };

            await tokenStore.storeTokens("user1", userTokens);

            const result = await tokenStore.getTokens("user1");
            expect(result).not.toBeNull();
            expect(result?.userId).toBe("user1");
            expect(result?.accessToken).toBe("access-token-1");
            expect(result?.email).toBe("user1@example.com");
            expect(result?.name).toBe("User One");
        });

        it("should update tokens for existing user", async () => {
            const initialTokens: UserTokens = {
                userId: "user1",
                accessToken: "old-access-token",
                refreshToken: "old-refresh-token",
                expiresAt: new Date(Date.now() + 3600 * 1000),
            };

            const updatedTokens: UserTokens = {
                userId: "user1",
                accessToken: "new-access-token",
                refreshToken: "new-refresh-token",
                expiresAt: new Date(Date.now() + 3600 * 1000),
            };

            await tokenStore.storeTokens("user1", initialTokens);
            await tokenStore.storeTokens("user1", updatedTokens);

            const result = await tokenStore.getTokens("user1");
            expect(result?.accessToken).toBe("new-access-token");
            expect(result?.refreshToken).toBe("new-refresh-token");
        });

        it("should not affect other users when updating one user", async () => {
            const user1Tokens: UserTokens = {
                userId: "user1",
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                expiresAt: new Date(Date.now() + 3600 * 1000),
            };

            const user2Tokens: UserTokens = {
                userId: "user2",
                accessToken: "access-token-2",
                refreshToken: "refresh-token-2",
                expiresAt: new Date(Date.now() + 3600 * 1000),
            };

            await tokenStore.storeTokens("user1", user1Tokens);
            await tokenStore.storeTokens("user2", user2Tokens);

            const result1 = await tokenStore.getTokens("user1");
            const result2 = await tokenStore.getTokens("user2");
            expect(result1?.accessToken).toBe("access-token-1");
            expect(result2?.accessToken).toBe("access-token-2");
        });
    });

    describe("getTokens", () => {
        it("should return null for non-existent user", async () => {
            const tokens = await tokenStore.getTokens("non-existent-user");
            expect(tokens).toBeNull();
        });

        it("should return tokens for existing user", async () => {
            const userTokens: UserTokens = {
                userId: "user1",
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                expiresAt: new Date(Date.now() + 3600 * 1000),
            };

            await tokenStore.storeTokens("user1", userTokens);

            const result = await tokenStore.getTokens("user1");
            expect(result).not.toBeNull();
            expect(result?.userId).toBe("user1");
            expect(result?.accessToken).toBe("access-token-1");
        });

        it("should return null for expired tokens", async () => {
            const expiredTokens: UserTokens = {
                userId: "user1",
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                expiresAt: new Date(Date.now() - 1000), // 1 second ago
            };

            await tokenStore.storeTokens("user1", expiredTokens);

            const result = await tokenStore.getTokens("user1");
            expect(result).toBeNull();
        });

        it("should return null for tokens expiring within buffer time", async () => {
            const soonToExpireTokens: UserTokens = {
                userId: "user1",
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                expiresAt: new Date(Date.now() + 2 * 60 * 1000), // 2 minutes (within 5 min buffer)
            };

            await tokenStore.storeTokens("user1", soonToExpireTokens);

            const result = await tokenStore.getTokens("user1");
            expect(result).toBeNull();
        });

        it("should return tokens when expiry is beyond buffer time", async () => {
            const validTokens: UserTokens = {
                userId: "user1",
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                expiresAt: new Date(Date.now() + 10 * 60 * 1000), // 10 minutes (beyond 5 min buffer)
            };

            await tokenStore.storeTokens("user1", validTokens);

            const result = await tokenStore.getTokens("user1");
            expect(result).not.toBeNull();
            expect(result?.userId).toBe("user1");
            expect(result?.accessToken).toBe("access-token-1");
        });

        it("should handle tokens with all optional fields", async () => {
            const fullTokens: UserTokens = {
                userId: "user1",
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                expiresAt: new Date(Date.now() + 3600 * 1000),
                email: "user1@example.com",
                name: "User One",
                picture: "https://example.com/picture.jpg",
            };

            await tokenStore.storeTokens("user1", fullTokens);

            const result = await tokenStore.getTokens("user1");
            expect(result).not.toBeNull();
            expect(result?.email).toBe("user1@example.com");
            expect(result?.name).toBe("User One");
            expect(result?.picture).toBe("https://example.com/picture.jpg");
        });
    });

    describe("removeTokens", () => {
        it("should remove tokens for a user", async () => {
            const userTokens: UserTokens = {
                userId: "user1",
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                expiresAt: new Date(Date.now() + 3600 * 1000),
            };

            await tokenStore.storeTokens("user1", userTokens);
            await tokenStore.removeTokens("user1");

            const result = await tokenStore.getTokens("user1");
            expect(result).toBeNull();
        });

        it("should not affect other users when removing one", async () => {
            const user1Tokens: UserTokens = {
                userId: "user1",
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                expiresAt: new Date(Date.now() + 3600 * 1000),
            };

            const user2Tokens: UserTokens = {
                userId: "user2",
                accessToken: "access-token-2",
                refreshToken: "refresh-token-2",
                expiresAt: new Date(Date.now() + 3600 * 1000),
            };

            await tokenStore.storeTokens("user1", user1Tokens);
            await tokenStore.storeTokens("user2", user2Tokens);
            await tokenStore.removeTokens("user1");

            const result = await tokenStore.getTokens("user2");
            expect(result).not.toBeNull();
            expect(result?.userId).toBe("user2");
            expect(result?.accessToken).toBe("access-token-2");
        });

        it("should not throw when removing non-existent user", async () => {
            await expect(
                tokenStore.removeTokens("non-existent"),
            ).resolves.not.toThrow();
        });
    });

    describe("getAllUserIds", () => {
        it("should return empty array when no tokens stored", async () => {
            const userIds = await tokenStore.getAllUserIds();
            expect(userIds).toEqual([]);
        });

        it("should return all user IDs", async () => {
            const makeTokens = (userId: string): UserTokens => ({
                userId,
                accessToken: `access-${userId}`,
                refreshToken: `refresh-${userId}`,
                expiresAt: new Date(Date.now() + 3600 * 1000),
            });

            await tokenStore.storeTokens("user1", makeTokens("user1"));
            await tokenStore.storeTokens("user2", makeTokens("user2"));
            await tokenStore.storeTokens("user3", makeTokens("user3"));

            const userIds = await tokenStore.getAllUserIds();
            expect(userIds).toContain("user1");
            expect(userIds).toContain("user2");
            expect(userIds).toContain("user3");
            expect(userIds).toHaveLength(3);
        });

        it("should include users with expired tokens", async () => {
            const expiredTokens: UserTokens = {
                userId: "expired-user",
                accessToken: "access-token",
                refreshToken: "refresh-token",
                expiresAt: new Date(Date.now() - 1000),
            };

            await tokenStore.storeTokens("expired-user", expiredTokens);

            const userIds = await tokenStore.getAllUserIds();
            expect(userIds).toContain("expired-user");
        });
    });
});
