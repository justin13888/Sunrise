import { existsSync } from "node:fs";
import * as fs from "node:fs/promises";
import * as path from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { TokenStore, type UserTokens } from "./tokenStore";

describe("TokenStore", () => {
    let tokenStore: TokenStore;
    const testDir = "./test-data";
    const testTokensPath = path.join(testDir, "tokens.json");

    beforeEach(async () => {
        // Create a new token store with a test directory
        tokenStore = new TokenStore(testDir);

        // Clean up any existing test data
        if (existsSync(testDir)) {
            await fs.rm(testDir, { recursive: true, force: true });
        }
    });

    afterEach(async () => {
        // Clean up test data after each test
        if (existsSync(testDir)) {
            await fs.rm(testDir, { recursive: true, force: true });
        }
    });

    describe("ensureDataDir", () => {
        it("should create data directory if it does not exist", async () => {
            await tokenStore.ensureDataDir();

            const dirExists = existsSync(testDir);
            expect(dirExists).toBe(true);
        });

        it("should not throw if directory already exists", async () => {
            await tokenStore.ensureDataDir();
            // Should not throw - just call it again
            await tokenStore.ensureDataDir();
            const dirExists = existsSync(testDir);
            expect(dirExists).toBe(true);
        });
    });

    describe("loadTokens", () => {
        it("should return empty object when file does not exist", async () => {
            const tokens = await tokenStore.loadTokens();
            expect(tokens).toEqual({});
        });

        it("should load tokens from existing file", async () => {
            const mockTokens = {
                user1: {
                    userId: "user1",
                    accessToken: "access-token-1",
                    refreshToken: "refresh-token-1",
                    expiresAt: "2026-01-01T00:00:00.000Z",
                    email: "user1@example.com",
                },
            };

            await tokenStore.ensureDataDir();
            await fs.writeFile(testTokensPath, JSON.stringify(mockTokens));

            const tokens = await tokenStore.loadTokens();
            expect(tokens).toEqual(mockTokens);
        });

        it("should handle corrupted JSON gracefully", async () => {
            await tokenStore.ensureDataDir();
            await fs.writeFile(testTokensPath, "invalid json");

            const tokens = await tokenStore.loadTokens();
            expect(tokens).toEqual({});
        });
    });

    describe("saveTokens", () => {
        it("should save tokens to file", async () => {
            const mockTokens = {
                user1: {
                    userId: "user1",
                    accessToken: "access-token-1",
                    refreshToken: "refresh-token-1",
                    expiresAt: new Date("2026-01-01"),
                    email: "user1@example.com",
                },
            };

            await tokenStore.saveTokens(mockTokens);

            const fileContent = await fs.readFile(testTokensPath, "utf8");
            const savedTokens = JSON.parse(fileContent);
            // Dates get serialized to strings, so we check the structure
            expect(savedTokens.user1.userId).toBe("user1");
            expect(savedTokens.user1.accessToken).toBe("access-token-1");
            expect(savedTokens.user1.email).toBe("user1@example.com");
        });

        it("should create directory if it does not exist", async () => {
            const mockTokens = {
                user1: {
                    userId: "user1",
                    accessToken: "access-token-1",
                    refreshToken: "refresh-token-1",
                    expiresAt: new Date("2026-01-01"),
                },
            };

            await tokenStore.saveTokens(mockTokens);

            const dirExists = existsSync(testDir);
            expect(dirExists).toBe(true);
        });

        it("should format JSON with proper indentation", async () => {
            const mockTokens = {
                user1: {
                    userId: "user1",
                    accessToken: "access-token-1",
                    refreshToken: "refresh-token-1",
                    expiresAt: new Date("2026-01-01"),
                },
            };

            await tokenStore.saveTokens(mockTokens);

            const fileContent = await fs.readFile(testTokensPath, "utf8");
            expect(fileContent).toContain("  "); // Should have 2-space indentation
            expect(fileContent.split("\n").length).toBeGreaterThan(1); // Should be multi-line
        });
    });

    describe("storeTokens", () => {
        it("should store tokens for a new user", async () => {
            const userTokens: UserTokens = {
                userId: "user1",
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                expiresAt: new Date("2026-01-01"),
                email: "user1@example.com",
                name: "User One",
            };

            await tokenStore.storeTokens("user1", userTokens);

            const allTokens = await tokenStore.loadTokens();
            expect(allTokens["user1"].userId).toBe("user1");
            expect(allTokens["user1"].accessToken).toBe("access-token-1");
            expect(allTokens["user1"].email).toBe("user1@example.com");
            expect(allTokens["user1"].name).toBe("User One");
        });

        it("should update tokens for existing user", async () => {
            const initialTokens: UserTokens = {
                userId: "user1",
                accessToken: "old-access-token",
                refreshToken: "old-refresh-token",
                expiresAt: new Date("2025-12-01"),
            };

            const updatedTokens: UserTokens = {
                userId: "user1",
                accessToken: "new-access-token",
                refreshToken: "new-refresh-token",
                expiresAt: new Date("2026-01-01"),
            };

            await tokenStore.storeTokens("user1", initialTokens);
            await tokenStore.storeTokens("user1", updatedTokens);

            const allTokens = await tokenStore.loadTokens();
            expect(allTokens["user1"].accessToken).toBe("new-access-token");
            expect(allTokens["user1"].refreshToken).toBe("new-refresh-token");
        });

        it("should not affect other users when updating one user", async () => {
            const user1Tokens: UserTokens = {
                userId: "user1",
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                expiresAt: new Date("2026-01-01"),
            };

            const user2Tokens: UserTokens = {
                userId: "user2",
                accessToken: "access-token-2",
                refreshToken: "refresh-token-2",
                expiresAt: new Date("2026-01-01"),
            };

            await tokenStore.storeTokens("user1", user1Tokens);
            await tokenStore.storeTokens("user2", user2Tokens);

            const allTokens = await tokenStore.loadTokens();
            expect(allTokens["user1"].accessToken).toBe("access-token-1");
            expect(allTokens["user2"].accessToken).toBe("access-token-2");
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
                expiresAt: new Date(Date.now() + 3600 * 1000), // 1 hour from now
            };

            await tokenStore.storeTokens("user1", userTokens);

            const tokens = await tokenStore.getTokens("user1");
            expect(tokens).not.toBeNull();
            expect(tokens?.userId).toBe("user1");
            expect(tokens?.accessToken).toBe("access-token-1");
        });

        it("should return null for expired tokens", async () => {
            const expiredTokens: UserTokens = {
                userId: "user1",
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                expiresAt: new Date(Date.now() - 1000), // 1 second ago
            };

            await tokenStore.storeTokens("user1", expiredTokens);

            const tokens = await tokenStore.getTokens("user1");
            expect(tokens).toBeNull();
        });

        it("should return null for tokens expiring within buffer time", async () => {
            const soonToExpireTokens: UserTokens = {
                userId: "user1",
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                expiresAt: new Date(Date.now() + 2 * 60 * 1000), // 2 minutes from now (within 5 min buffer)
            };

            await tokenStore.storeTokens("user1", soonToExpireTokens);

            const tokens = await tokenStore.getTokens("user1");
            expect(tokens).toBeNull();
        });

        it("should return tokens when expiry is beyond buffer time", async () => {
            const validTokens: UserTokens = {
                userId: "user1",
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                expiresAt: new Date(Date.now() + 10 * 60 * 1000), // 10 minutes from now (beyond 5 min buffer)
            };

            await tokenStore.storeTokens("user1", validTokens);

            const tokens = await tokenStore.getTokens("user1");
            expect(tokens).not.toBeNull();
            expect(tokens?.userId).toBe("user1");
            expect(tokens?.accessToken).toBe("access-token-1");
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

            const tokens = await tokenStore.getTokens("user1");
            expect(tokens).not.toBeNull();
            expect(tokens?.email).toBe("user1@example.com");
            expect(tokens?.name).toBe("User One");
            expect(tokens?.picture).toBe("https://example.com/picture.jpg");
        });
    });

    describe("removeTokens", () => {
        it("should remove tokens for a user", async () => {
            const userTokens: UserTokens = {
                userId: "user1",
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                expiresAt: new Date("2026-01-01"),
            };

            await tokenStore.storeTokens("user1", userTokens);
            await tokenStore.removeTokens("user1");

            const tokens = await tokenStore.getTokens("user1");
            expect(tokens).toBeNull();
        });

        it("should not affect other users when removing one", async () => {
            const user1Tokens: UserTokens = {
                userId: "user1",
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                expiresAt: new Date("2026-01-01"),
            };

            const user2Tokens: UserTokens = {
                userId: "user2",
                accessToken: "access-token-2",
                refreshToken: "refresh-token-2",
                expiresAt: new Date("2026-01-01"),
            };

            await tokenStore.storeTokens("user1", user1Tokens);
            await tokenStore.storeTokens("user2", user2Tokens);
            await tokenStore.removeTokens("user1");

            const tokens = await tokenStore.getTokens("user2");
            expect(tokens).not.toBeNull();
            expect(tokens?.userId).toBe("user2");
            expect(tokens?.accessToken).toBe("access-token-2");
        });

        it("should not throw when removing non-existent user", async () => {
            // Should not throw - just call it
            await tokenStore.removeTokens("non-existent");
            // If we got here, it didn't throw
            expect(true).toBe(true);
        });
    });

    describe("getAllUserIds", () => {
        it("should return empty array when no tokens stored", async () => {
            const userIds = await tokenStore.getAllUserIds();
            expect(userIds).toEqual([]);
        });

        it("should return all user IDs", async () => {
            const user1Tokens: UserTokens = {
                userId: "user1",
                accessToken: "access-token-1",
                refreshToken: "refresh-token-1",
                expiresAt: new Date("2026-01-01"),
            };

            const user2Tokens: UserTokens = {
                userId: "user2",
                accessToken: "access-token-2",
                refreshToken: "refresh-token-2",
                expiresAt: new Date("2026-01-01"),
            };

            const user3Tokens: UserTokens = {
                userId: "user3",
                accessToken: "access-token-3",
                refreshToken: "refresh-token-3",
                expiresAt: new Date("2026-01-01"),
            };

            await tokenStore.storeTokens("user1", user1Tokens);
            await tokenStore.storeTokens("user2", user2Tokens);
            await tokenStore.storeTokens("user3", user3Tokens);

            const userIds = await tokenStore.getAllUserIds();
            expect(userIds).toContain("user1");
            expect(userIds).toContain("user2");
            expect(userIds).toContain("user3");
            expect(userIds).toHaveLength(3);
        });

        it("should include expired users", async () => {
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
