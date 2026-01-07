import * as fs from "node:fs/promises";
import * as path from "node:path";

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
 * Simple file-based token storage
 * TODO: Replace with proper database storage in production
 */
export class TokenStore {
    private tokensPath: string;

    constructor(tokensDir: string = "./data") {
        this.tokensPath = path.join(tokensDir, "tokens.json");
    }

    async ensureDataDir() {
        const dir = path.dirname(this.tokensPath);
        try {
            await fs.access(dir);
        } catch {
            await fs.mkdir(dir, { recursive: true });
        }
    }

    async loadTokens(): Promise<Record<string, UserTokens>> {
        try {
            const content = await fs.readFile(this.tokensPath, "utf8");
            return JSON.parse(content);
        } catch {
            return {};
        }
    }

    async saveTokens(tokens: Record<string, UserTokens>): Promise<void> {
        await this.ensureDataDir();
        await fs.writeFile(this.tokensPath, JSON.stringify(tokens, null, 2));
    }

    async storeTokens(userId: string, tokens: UserTokens): Promise<void> {
        const allTokens = await this.loadTokens();
        allTokens[userId] = tokens;
        await this.saveTokens(allTokens);
    }

    async getTokens(userId: string): Promise<UserTokens | null> {
        const allTokens = await this.loadTokens();
        const tokens = allTokens[userId];

        if (!tokens) return null;

        // Check if token is expired (with some buffer)
        const expiresAt = new Date(tokens.expiresAt);
        const now = new Date();
        const bufferTime = 5 * 60 * 1000; // 5 minutes buffer

        if (expiresAt.getTime() - bufferTime < now.getTime()) {
            // Token is expired or about to expire
            return null;
        }

        return tokens;
    }

    async removeTokens(userId: string): Promise<void> {
        const allTokens = await this.loadTokens();
        delete allTokens[userId];
        await this.saveTokens(allTokens);
    }

    async getAllUserIds(): Promise<string[]> {
        const allTokens = await this.loadTokens();
        return Object.keys(allTokens);
    }
}

// Global token store instance
export const tokenStore = new TokenStore();
