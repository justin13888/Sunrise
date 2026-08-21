import { LEGACY_DEFAULT_JWT_SECRET } from "./services/jwt";

export const DEFAULT_REDIRECT_URI = "http://localhost:3000/auth/callback";
export const DEFAULT_ALLOWED_ORIGINS = [
    "http://localhost:1420",
    "http://tauri.localhost",
    "tauri://localhost",
];

export interface ApiEnv {
    googleClientId: string;
    googleClientSecret: string;
    googleRedirectUri: string;
    /** Exact-match CORS allowlist. */
    allowedOrigins: string[];
    /** postMessage target origin for the OAuth callback page. */
    frontendOrigin: string;
    port: number;
}

/**
 * Validate process.env and return the typed configuration.
 *
 * Fails fast with a single clear message listing every problem, so the server
 * never starts (or polls Google) with an incomplete configuration.
 */
export function validateEnv(env: NodeJS.ProcessEnv = process.env): ApiEnv {
    const problems: string[] = [];

    const googleClientId = env.GOOGLE_CLIENT_ID;
    const googleClientSecret = env.GOOGLE_CLIENT_SECRET;
    if (!googleClientId) {
        problems.push("GOOGLE_CLIENT_ID is required (Google OAuth client ID)");
    }
    if (!googleClientSecret) {
        problems.push(
            "GOOGLE_CLIENT_SECRET is required (Google OAuth client secret)",
        );
    }

    if (env.NODE_ENV === "production") {
        if (!env.JWT_SECRET || env.JWT_SECRET === LEGACY_DEFAULT_JWT_SECRET) {
            problems.push(
                "JWT_SECRET must be set to a strong unique value in production " +
                    "(generate one with: openssl rand -hex 32)",
            );
        }
    }

    const port = Number.parseInt(env.PORT || "3000", 10);
    if (Number.isNaN(port) || port <= 0 || port > 65535) {
        problems.push(`PORT must be a valid port number (got "${env.PORT}")`);
    }

    const allowedOrigins = (env.ALLOWED_ORIGINS ?? "")
        .split(",")
        .map((origin) => origin.trim())
        .filter((origin) => origin.length > 0);
    if (allowedOrigins.length === 0) {
        allowedOrigins.push(...DEFAULT_ALLOWED_ORIGINS);
    }

    if (problems.length > 0) {
        throw new Error(
            `Invalid server configuration:\n${problems
                .map((p) => `  - ${p}`)
                .join("\n")}`,
        );
    }

    return {
        // Presence is guaranteed above; problems.length would be > 0 otherwise.
        googleClientId: googleClientId as string,
        googleClientSecret: googleClientSecret as string,
        googleRedirectUri: env.GOOGLE_REDIRECT_URI || DEFAULT_REDIRECT_URI,
        allowedOrigins,
        frontendOrigin: env.FRONTEND_ORIGIN || allowedOrigins[0],
        port,
    };
}
