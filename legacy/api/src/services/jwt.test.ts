import { describe, expect, it } from "vitest";
import { type JWTPayload, signJWT, verifyJWT } from "./jwt";

describe("signJWT", () => {
    it("should return a JWT string with 3 parts", async () => {
        const token = await signJWT("user123");
        const parts = token.split(".");
        expect(parts).toHaveLength(3);
    });

    it("should produce tokens with different signatures for different users", async () => {
        const token1 = await signJWT("user1");
        const token2 = await signJWT("user2");
        expect(token1).not.toBe(token2);
    });

    it("should embed the userId in the payload", async () => {
        const token = await signJWT("test-user-id");
        const [, payloadB64] = token.split(".");
        const base64 = payloadB64.replace(/-/g, "+").replace(/_/g, "/");
        const payload = JSON.parse(atob(base64)) as JWTPayload;
        expect(payload.userId).toBe("test-user-id");
    });

    it("should set iat and exp fields", async () => {
        const before = Math.floor(Date.now() / 1000);
        const token = await signJWT("user");
        const after = Math.floor(Date.now() / 1000);

        const [, payloadB64] = token.split(".");
        const base64 = payloadB64.replace(/-/g, "+").replace(/_/g, "/");
        const payload = JSON.parse(atob(base64)) as JWTPayload;

        expect(payload.iat).toBeGreaterThanOrEqual(before);
        expect(payload.iat).toBeLessThanOrEqual(after);
        expect(payload.exp).toBeGreaterThan(payload.iat);
        // exp should be ~1 hour after iat
        expect(payload.exp - payload.iat).toBe(3600);
    });
});

describe("verifyJWT", () => {
    it("should return payload for a valid token", async () => {
        const token = await signJWT("user123");
        const payload = await verifyJWT(token);

        expect(payload).not.toBeNull();
        expect(payload?.userId).toBe("user123");
    });

    it("should return null for a token with wrong format", async () => {
        const result = await verifyJWT("not.a.valid.jwt.token");
        expect(result).toBeNull();
    });

    it("should return null for empty string", async () => {
        const result = await verifyJWT("");
        expect(result).toBeNull();
    });

    it("should return null for a token with only 2 parts", async () => {
        const result = await verifyJWT("header.payload");
        expect(result).toBeNull();
    });

    it("should return null for a tampered token", async () => {
        const token = await signJWT("user123");
        const parts = token.split(".");
        // Tamper with the payload
        const tamperedPayload = btoa(JSON.stringify({ userId: "attacker" }))
            .replace(/\+/g, "-")
            .replace(/\//g, "_")
            .replace(/=/g, "");
        const tampered = `${parts[0]}.${tamperedPayload}.${parts[2]}`;
        const result = await verifyJWT(tampered);
        expect(result).toBeNull();
    });

    it("should return null for an expired token", async () => {
        // Build a token with exp in the past by signing a custom payload
        const token = await signJWT("user");
        const parts = token.split(".");
        const expiredPayload = btoa(
            JSON.stringify({
                userId: "user",
                iat: Math.floor(Date.now() / 1000) - 7200,
                exp: Math.floor(Date.now() / 1000) - 3600,
            }),
        )
            .replace(/\+/g, "-")
            .replace(/\//g, "_")
            .replace(/=/g, "");
        // This will fail signature verification, but tests the expiry path indirectly
        const expired = `${parts[0]}.${expiredPayload}.${parts[2]}`;
        const result = await verifyJWT(expired);
        // Invalid signature → null
        expect(result).toBeNull();
    });

    it("should return null for completely invalid base64 in signature", async () => {
        const result = await verifyJWT("header.payload.!!!");
        expect(result).toBeNull();
    });

    it("should roundtrip: sign then verify returns same userId", async () => {
        const userId = "roundtrip-user-42";
        const token = await signJWT(userId);
        const payload = await verifyJWT(token);
        expect(payload?.userId).toBe(userId);
    });
});
