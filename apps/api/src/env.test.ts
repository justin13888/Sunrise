import { describe, expect, it } from "vitest";
import {
    DEFAULT_ALLOWED_ORIGINS,
    DEFAULT_REDIRECT_URI,
    validateEnv,
} from "./env";

const baseEnv = {
    GOOGLE_CLIENT_ID: "client-id",
    GOOGLE_CLIENT_SECRET: "client-secret",
} as NodeJS.ProcessEnv;

describe("validateEnv", () => {
    it("returns defaults when only required vars are set", () => {
        const env = validateEnv({ ...baseEnv });
        expect(env.googleClientId).toBe("client-id");
        expect(env.googleClientSecret).toBe("client-secret");
        expect(env.googleRedirectUri).toBe(DEFAULT_REDIRECT_URI);
        expect(env.allowedOrigins).toEqual(DEFAULT_ALLOWED_ORIGINS);
        expect(env.frontendOrigin).toBe(DEFAULT_ALLOWED_ORIGINS[0]);
        expect(env.port).toBe(3000);
    });

    it("throws listing every missing required variable", () => {
        expect(() => validateEnv({} as NodeJS.ProcessEnv)).toThrow(
            /GOOGLE_CLIENT_ID[\s\S]*GOOGLE_CLIENT_SECRET/,
        );
    });

    it("throws when only GOOGLE_CLIENT_SECRET is missing", () => {
        expect(() =>
            validateEnv({
                GOOGLE_CLIENT_ID: "client-id",
            } as NodeJS.ProcessEnv),
        ).toThrow(/GOOGLE_CLIENT_SECRET/);
    });

    it("parses ALLOWED_ORIGINS as a comma-separated list", () => {
        const env = validateEnv({
            ...baseEnv,
            ALLOWED_ORIGINS: "http://a.test, http://b.test ,,",
        });
        expect(env.allowedOrigins).toEqual(["http://a.test", "http://b.test"]);
        expect(env.frontendOrigin).toBe("http://a.test");
    });

    it("prefers explicit FRONTEND_ORIGIN over the first allowed origin", () => {
        const env = validateEnv({
            ...baseEnv,
            ALLOWED_ORIGINS: "http://a.test,http://b.test",
            FRONTEND_ORIGIN: "http://b.test",
        });
        expect(env.frontendOrigin).toBe("http://b.test");
    });

    it("honors PORT and GOOGLE_REDIRECT_URI overrides", () => {
        const env = validateEnv({
            ...baseEnv,
            PORT: "8080",
            GOOGLE_REDIRECT_URI: "https://example.com/auth/callback",
        });
        expect(env.port).toBe(8080);
        expect(env.googleRedirectUri).toBe("https://example.com/auth/callback");
    });

    it("rejects an invalid PORT", () => {
        expect(() => validateEnv({ ...baseEnv, PORT: "nope" })).toThrow(/PORT/);
    });

    it("requires JWT_SECRET in production", () => {
        expect(() =>
            validateEnv({ ...baseEnv, NODE_ENV: "production" }),
        ).toThrow(/JWT_SECRET/);
    });

    it("rejects the legacy default JWT_SECRET in production", () => {
        expect(() =>
            validateEnv({
                ...baseEnv,
                NODE_ENV: "production",
                JWT_SECRET: "sunrise-jwt-secret-change-in-production",
            }),
        ).toThrow(/JWT_SECRET/);
    });

    it("accepts a strong JWT_SECRET in production", () => {
        const env = validateEnv({
            ...baseEnv,
            NODE_ENV: "production",
            JWT_SECRET: "a".repeat(64),
        });
        expect(env.port).toBe(3000);
    });

    it("does not require JWT_SECRET outside production", () => {
        expect(() => validateEnv({ ...baseEnv })).not.toThrow();
    });
});
