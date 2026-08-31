const JWT_SECRET =
    process.env.JWT_SECRET || "sunrise-jwt-secret-change-in-production";
const JWT_EXPIRY = 60 * 60; // 1 hour

export interface JWTPayload {
    userId: string;
    iat: number;
    exp: number;
}

function base64url(input: ArrayBuffer | string): string {
    let binary = "";
    if (typeof input === "string") {
        for (let i = 0; i < input.length; i++) {
            binary += String.fromCharCode(input.charCodeAt(i) & 0xff);
        }
    } else {
        for (const byte of new Uint8Array(input)) {
            binary += String.fromCharCode(byte);
        }
    }
    return btoa(binary)
        .replace(/\+/g, "-")
        .replace(/\//g, "_")
        .replace(/=/g, "");
}

function fromBase64url(input: string): Uint8Array {
    const base64 = input.replace(/-/g, "+").replace(/_/g, "/");
    const binary = atob(base64);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) {
        bytes[i] = binary.charCodeAt(i);
    }
    return bytes;
}

async function getKey(): Promise<CryptoKey> {
    return crypto.subtle.importKey(
        "raw",
        new TextEncoder().encode(JWT_SECRET),
        { name: "HMAC", hash: "SHA-256" },
        false,
        ["sign", "verify"],
    );
}

export async function signJWT(userId: string): Promise<string> {
    const header = base64url(JSON.stringify({ alg: "HS256", typ: "JWT" }));
    const now = Math.floor(Date.now() / 1000);
    const payload = base64url(
        JSON.stringify({ userId, iat: now, exp: now + JWT_EXPIRY }),
    );
    const message = `${header}.${payload}`;
    const key = await getKey();
    const sig = await crypto.subtle.sign(
        "HMAC",
        key,
        new TextEncoder().encode(message),
    );
    return `${message}.${base64url(sig)}`;
}

export async function verifyJWT(token: string): Promise<JWTPayload | null> {
    try {
        const parts = token.split(".");
        if (parts.length !== 3) return null;
        const [header, payload, sig] = parts;
        const message = `${header}.${payload}`;
        const key = await getKey();
        const valid = await crypto.subtle.verify(
            "HMAC",
            key,
            fromBase64url(sig),
            new TextEncoder().encode(message),
        );
        if (!valid) return null;
        const data = JSON.parse(
            new TextDecoder().decode(fromBase64url(payload)),
        ) as JWTPayload;
        if (data.exp < Math.floor(Date.now() / 1000)) return null;
        return data;
    } catch {
        return null;
    }
}
