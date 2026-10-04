import { describe, expect, it } from "vitest";
import {
    MAX_SERVER_URL_LENGTH,
    parseServerUrl,
    readServerUrl,
    resolveServerUrl,
    type ServerUrlEnv,
    STORAGE_KEY,
    withoutServerParam,
} from "./server-url";

describe("parseServerUrl", () => {
    it("reduces an https URL to its origin", () => {
        expect(parseServerUrl("https://relay.example")).toBe(
            "https://relay.example",
        );
        expect(parseServerUrl("  https://Relay.Example:8443/  ")).toBe(
            "https://relay.example:8443",
        );
    });

    it("allows http on loopback only", () => {
        expect(parseServerUrl("http://localhost:8443")).toBe(
            "http://localhost:8443",
        );
        expect(parseServerUrl("http://127.0.0.1:8443")).toBe(
            "http://127.0.0.1:8443",
        );
        expect(parseServerUrl("http://[::1]:8443")).toBe("http://[::1]:8443");
        expect(parseServerUrl("http://relay.example")).toBeNull();
        expect(parseServerUrl("http://192.168.1.10:8443")).toBeNull();
    });

    it("refuses anything that is not an origin", () => {
        for (const input of [
            "",
            "   ",
            "relay.example",
            "not a url",
            "wss://relay.example",
            "javascript:alert(1)",
            "https://relay.example/sync",
            "https://relay.example/?token=x",
            "https://relay.example/#x",
            "https://user:pw@relay.example",
            "https://user@relay.example",
        ]) {
            expect(parseServerUrl(input), input).toBeNull();
        }
    });

    it("refuses a URL over the pairing payload's bound", () => {
        const host = "a".repeat(MAX_SERVER_URL_LENGTH);
        expect(parseServerUrl(`https://${host}.example`)).toBeNull();
    });
});

describe("resolveServerUrl", () => {
    const none = { stored: null, linked: null, buildDefault: undefined };

    it("is none when nothing is configured", () => {
        expect(resolveServerUrl(none)).toEqual({
            url: null,
            source: "none",
            store: null,
            refused: null,
        });
        expect(resolveServerUrl({ ...none, buildDefault: "" }).source).toBe(
            "none",
        );
    });

    it("uses the build default without storing it", () => {
        expect(
            resolveServerUrl({ ...none, buildDefault: "https://ops.example/" }),
        ).toEqual({
            url: "https://ops.example",
            source: "build",
            store: null,
            refused: null,
        });
    });

    it("follows and stores a link when nothing is stored", () => {
        expect(
            resolveServerUrl({
                ...none,
                linked: "https://relay.example",
                buildDefault: "https://ops.example",
            }),
        ).toEqual({
            url: "https://relay.example",
            source: "link",
            store: "https://relay.example",
            refused: null,
        });
    });

    it("keeps the stored server over a link to another", () => {
        const resolution = resolveServerUrl({
            stored: "https://mine.example",
            linked: "https://theirs.example",
            buildDefault: "https://ops.example",
        });
        expect(resolution.url).toBe("https://mine.example");
        expect(resolution.source).toBe("stored");
        expect(resolution.store).toBeNull();
        expect(resolution.refused).toContain("https://mine.example");
    });

    it("does not call a link to the stored server refused", () => {
        expect(
            resolveServerUrl({
                ...none,
                stored: "https://mine.example",
                linked: "https://mine.example/",
            }).refused,
        ).toBeNull();
    });

    it("refuses a malformed link and falls through", () => {
        const resolution = resolveServerUrl({
            ...none,
            linked: "http://relay.example",
            buildDefault: "https://ops.example",
        });
        expect(resolution.url).toBe("https://ops.example");
        expect(resolution.store).toBeNull();
        expect(resolution.refused).toContain("http://relay.example");
    });

    it("treats a stored value that no longer parses as nothing stored", () => {
        expect(
            resolveServerUrl({
                ...none,
                stored: "garbage",
                linked: "https://relay.example",
            }),
        ).toMatchObject({ source: "link", store: "https://relay.example" });
    });
});

describe("withoutServerParam", () => {
    it("takes the parameter off and keeps the rest", () => {
        expect(
            withoutServerParam(
                "https://app.example/capture?server=https%3A%2F%2Fr.example&text=hi#x",
            ),
        ).toBe("https://app.example/capture?text=hi#x");
    });

    it("is null when there is nothing to take off", () => {
        expect(withoutServerParam("https://app.example/?text=hi")).toBeNull();
    });
});

function fakeEnv(
    href: string,
    stored: Record<string, string> = {},
    options: { failGet?: boolean; failSet?: boolean } = {},
) {
    const warnings: string[] = [];
    const replaced: string[] = [];
    const env: ServerUrlEnv = {
        href,
        storage: {
            getItem(key) {
                if (options.failGet) {
                    throw new Error("SecurityError");
                }
                return stored[key] ?? null;
            },
            setItem(key, value) {
                if (options.failSet) {
                    throw new Error("QuotaExceededError");
                }
                stored[key] = value;
            },
        },
        replaceUrl: (next) => replaced.push(next),
        buildDefault: undefined,
        warn: (message) => warnings.push(message),
    };
    return { env, stored, warnings, replaced };
}

describe("readServerUrl", () => {
    it("stores a followed link and cleans the address", () => {
        const { env, stored, warnings, replaced } = fakeEnv(
            "https://app.example/?server=https://relay.example",
        );
        expect(readServerUrl(env).url).toBe("https://relay.example");
        expect(stored[STORAGE_KEY]).toBe("https://relay.example");
        expect(replaced).toEqual(["https://app.example/"]);
        expect(warnings).toEqual([]);
    });

    it("warns about a refused link and still cleans the address", () => {
        const { env, stored, warnings, replaced } = fakeEnv(
            "https://app.example/?server=https://theirs.example",
            { [STORAGE_KEY]: "https://mine.example" },
        );
        expect(readServerUrl(env).url).toBe("https://mine.example");
        expect(stored[STORAGE_KEY]).toBe("https://mine.example");
        expect(replaced).toEqual(["https://app.example/"]);
        expect(warnings).toHaveLength(1);
        expect(warnings[0]).toContain("server link ignored");
    });

    it("leaves the address alone without a link", () => {
        const { env, replaced } = fakeEnv("https://app.example/", {
            [STORAGE_KEY]: "https://mine.example",
        });
        expect(readServerUrl(env).source).toBe("stored");
        expect(replaced).toEqual([]);
    });

    it("follows a link for the visit when storage is withheld", () => {
        const { env, warnings } = fakeEnv(
            "https://app.example/?server=https://relay.example",
            {},
            { failGet: true, failSet: true },
        );
        expect(readServerUrl(env).url).toBe("https://relay.example");
        expect(warnings).toEqual([
            "server link followed for this visit only: storage refused",
        ]);
    });
});
