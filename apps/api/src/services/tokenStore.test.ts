import { beforeEach, describe, expect, it, vi } from "vitest";
import type { UserTokens } from "./tokenStore";

// Mock the db module with a real in-memory SQLite database whose schema is
// built from the committed drizzle migration SQL files.
// vi.mock is hoisted above imports, so the factory runs before TokenStore is imported.
vi.mock("../db", async () => {
    const { createTestDb } = await import("../db/testUtils");
    const { sqlite, db } = createTestDb();

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

function makeTokens(
    userId: string,
    overrides: Partial<UserTokens> = {},
): UserTokens {
    return {
        userId,
        accessToken: `access-${userId}`,
        refreshToken: `refresh-${userId}`,
        expiresAt: new Date(Date.now() + 3600 * 1000),
        email: `${userId}@example.com`,
        ...overrides,
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
            const userTokens = makeTokens("user1", {
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                email: "user1@example.com",
                name: "User One",
            });

            await tokenStore.storeTokens("user1", userTokens);

            const result = await tokenStore.getTokens("user1");
            expect(result).not.toBeNull();
            expect(result?.userId).toBe("user1");
            expect(result?.accessToken).toBe("access-token-1");
            expect(result?.email).toBe("user1@example.com");
            expect(result?.name).toBe("User One");
        });

        it("should update tokens for existing user", async () => {
            const initialTokens = makeTokens("user1", {
                accessToken: "old-access-token",
                refreshToken: "old-refresh-token",
            });

            const updatedTokens = makeTokens("user1", {
                accessToken: "new-access-token",
                refreshToken: "new-refresh-token",
            });

            await tokenStore.storeTokens("user1", initialTokens);
            await tokenStore.storeTokens("user1", updatedTokens);

            const result = await tokenStore.getTokens("user1");
            expect(result?.accessToken).toBe("new-access-token");
            expect(result?.refreshToken).toBe("new-refresh-token");
        });

        it("should not affect other users when updating one user", async () => {
            await tokenStore.storeTokens("user1", makeTokens("user1"));
            await tokenStore.storeTokens("user2", makeTokens("user2"));

            const result1 = await tokenStore.getTokens("user1");
            const result2 = await tokenStore.getTokens("user2");
            expect(result1?.accessToken).toBe("access-user1");
            expect(result2?.accessToken).toBe("access-user2");
        });
    });

    describe("getTokens", () => {
        it("should return null for non-existent user", async () => {
            const tokens = await tokenStore.getTokens("non-existent-user");
            expect(tokens).toBeNull();
        });

        it("should return tokens for existing user", async () => {
            await tokenStore.storeTokens("user1", makeTokens("user1"));

            const result = await tokenStore.getTokens("user1");
            expect(result).not.toBeNull();
            expect(result?.userId).toBe("user1");
            expect(result?.accessToken).toBe("access-user1");
        });

        it("should return tokens even when the access token is expired", async () => {
            // Second-aligned: the timestamp column stores whole seconds
            const expiresAt = new Date(
                Math.floor((Date.now() - 1000) / 1000) * 1000,
            );
            const expiredTokens = makeTokens("user1", { expiresAt });

            await tokenStore.storeTokens("user1", expiredTokens);

            const result = await tokenStore.getTokens("user1");
            expect(result).not.toBeNull();
            expect(result?.refreshToken).toBe("refresh-user1");
            expect(result?.expiresAt.getTime()).toBe(expiresAt.getTime());
        });

        it("should return tokens expiring in the near future", async () => {
            const soonToExpireTokens = makeTokens("user1", {
                expiresAt: new Date(Date.now() + 2 * 60 * 1000), // 2 minutes
            });

            await tokenStore.storeTokens("user1", soonToExpireTokens);

            const result = await tokenStore.getTokens("user1");
            expect(result).not.toBeNull();
            expect(result?.accessToken).toBe("access-user1");
        });

        it("should handle tokens with all optional fields", async () => {
            const fullTokens = makeTokens("user1", {
                email: "user1@example.com",
                name: "User One",
                picture: "https://example.com/picture.jpg",
            });

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
            await tokenStore.storeTokens("user1", makeTokens("user1"));
            await tokenStore.removeTokens("user1");

            const result = await tokenStore.getTokens("user1");
            expect(result).toBeNull();
        });

        it("should not affect other users when removing one", async () => {
            await tokenStore.storeTokens("user1", makeTokens("user1"));
            await tokenStore.storeTokens("user2", makeTokens("user2"));
            await tokenStore.removeTokens("user1");

            const result = await tokenStore.getTokens("user2");
            expect(result).not.toBeNull();
            expect(result?.userId).toBe("user2");
            expect(result?.accessToken).toBe("access-user2");
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

        it("should return all user IDs with stored tokens", async () => {
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
            const expiredTokens = makeTokens("expired-user", {
                expiresAt: new Date(Date.now() - 1000),
            });

            await tokenStore.storeTokens("expired-user", expiredTokens);

            const userIds = await tokenStore.getAllUserIds();
            expect(userIds).toContain("expired-user");
        });

        it("should exclude users whose tokens were removed (logged out)", async () => {
            await tokenStore.storeTokens("user1", makeTokens("user1"));
            await tokenStore.storeTokens("user2", makeTokens("user2"));
            await tokenStore.removeTokens("user1");

            const userIds = await tokenStore.getAllUserIds();
            expect(userIds).toEqual(["user2"]);
        });

        it("should not duplicate user IDs", async () => {
            await tokenStore.storeTokens("user1", makeTokens("user1"));
            await tokenStore.storeTokens("user1", makeTokens("user1"));

            const userIds = await tokenStore.getAllUserIds();
            expect(userIds).toEqual(["user1"]);
        });
    });
});
