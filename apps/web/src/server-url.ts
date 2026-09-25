/**
 * The sync server this web client talks to (#11).
 *
 * Chosen at run time, per browser, never baked into a deployment: one hosted
 * copy of the static app serves every relay, and self-host is first-class. It
 * comes from, in order:
 *
 * 1. **The stored setting.** What this origin chose before, in `localStorage`.
 *    A server URL is not a secret — the vault is end-to-end encrypted and the
 *    relay sees ciphertext — so it may live there (web.md §Storage).
 * 2. **A link.** `?server=https://relay.example` on the page's address, which
 *    is how an operator points a user at their relay. A link is honoured only
 *    while nothing is stored, and is then stored: a link cannot move a user
 *    who has already chosen a relay onto another one, since nothing on screen
 *    would say it had.
 * 3. **The build default.** `SUNRISE_WEB_DEFAULT_SERVER_URL` at build time,
 *    for an operator hosting the app for their own relay. Not stored, so a
 *    rebuilt default reaches every browser that never chose.
 *
 * Nothing consumes the URL yet: the web core opens a local vault only, and the
 * SSE transport is native-only (web.md §Status). This is the setting the web
 * sync transport will read, landed ahead of it.
 *
 * Everything but `configuredServerUrl` is pure and is what
 * `server-url.test.ts` drives.
 */

/** The query parameter a link names a server in. */
export const SERVER_PARAM = "server";

/** The `localStorage` key the chosen server is stored under. */
export const STORAGE_KEY = "sunrise-web-server-url";

/** The longest URL accepted: the pairing QR payload's `relay_url` bound. */
export const MAX_SERVER_URL_LENGTH = 256;

/** Hosts a browser treats as secure over plain `http:`. */
const LOOPBACK_HOSTS = new Set(["localhost", "127.0.0.1", "[::1]"]);

/**
 * `input` as a relay origin — `https://relay.example:8443`, no trailing slash —
 * or `null` if it is not one.
 *
 * `https:` only, except on loopback, where `http:` is a local relay under
 * development: a page served over `https:` cannot reach an `http:` relay
 * anywhere else, since the browser blocks it as mixed content. A path, query,
 * fragment or credential is refused rather than dropped: the relay client
 * takes an origin (`sunrise-relay-client`'s `base_url`), and silently
 * discarding part of what a user typed would connect them somewhere they did
 * not say.
 */
export function parseServerUrl(input: string): string | null {
    const trimmed = input.trim();
    if (trimmed === "" || trimmed.length > MAX_SERVER_URL_LENGTH) {
        return null;
    }
    let url: URL;
    try {
        url = new URL(trimmed);
    } catch {
        return null;
    }
    const secure =
        url.protocol === "https:" ||
        (url.protocol === "http:" && LOOPBACK_HOSTS.has(url.hostname));
    if (
        !secure ||
        url.username !== "" ||
        url.password !== "" ||
        url.pathname !== "/" ||
        url.search !== "" ||
        url.hash !== ""
    ) {
        return null;
    }
    return url.origin;
}

/** Where a resolved server URL came from. */
export type ServerUrlSource = "stored" | "link" | "build" | "none";

export interface ServerUrlInputs {
    /** The stored setting, raw. */
    stored: string | null;
    /** The link's `?server=` value, raw; `null` if the address has none. */
    linked: string | null;
    /** `SUNRISE_WEB_DEFAULT_SERVER_URL`, raw; empty or absent for none. */
    buildDefault: string | undefined;
}

export interface ServerUrlResolution {
    /** The relay origin to use, or `null` when none is configured. */
    url: string | null;
    source: ServerUrlSource;
    /** What to write to storage, or `null` to leave it as it is. */
    store: string | null;
    /** Why a link was not followed, when it was not. */
    refused: string | null;
}

/**
 * Pick the server from its three sources, in the order the module comment
 * gives. A stored value that no longer parses counts as nothing stored.
 */
export function resolveServerUrl(inputs: ServerUrlInputs): ServerUrlResolution {
    const stored =
        inputs.stored === null ? null : parseServerUrl(inputs.stored);
    const linked =
        inputs.linked === null ? null : parseServerUrl(inputs.linked);

    let refused: string | null = null;
    if (inputs.linked !== null) {
        if (linked === null) {
            refused = `not an https relay origin: ${inputs.linked.slice(0, 80)}`;
        } else if (stored !== null && linked !== stored) {
            refused = `this browser already uses ${stored}`;
        }
    }

    if (stored !== null) {
        return { url: stored, source: "stored", store: null, refused };
    }
    if (linked !== null) {
        return { url: linked, source: "link", store: linked, refused };
    }
    const fallback =
        inputs.buildDefault === undefined
            ? null
            : parseServerUrl(inputs.buildDefault);
    return {
        url: fallback,
        source: fallback === null ? "none" : "build",
        store: null,
        refused,
    };
}

/** The address with `?server=` taken off, or `null` if it had none. */
export function withoutServerParam(href: string): string | null {
    const url = new URL(href);
    if (!url.searchParams.has(SERVER_PARAM)) {
        return null;
    }
    url.searchParams.delete(SERVER_PARAM);
    return url.href;
}

/** What `readServerUrl` touches, so a test can hand it fakes. */
export interface ServerUrlEnv {
    href: string;
    storage: Pick<Storage, "getItem" | "setItem">;
    replaceUrl(href: string): void;
    buildDefault: string | undefined;
    warn(message: string): void;
}

/**
 * Resolve the server against `env`, store a followed link, and take
 * `?server=` off the address so a reload or a bookmark does not carry it.
 */
export function readServerUrl(env: ServerUrlEnv): ServerUrlResolution {
    const linked = new URL(env.href).searchParams.get(SERVER_PARAM);
    let stored: string | null = null;
    try {
        stored = env.storage.getItem(STORAGE_KEY);
    } catch {
        // Storage withheld (a private window, a blocked origin): a link or the
        // build default still applies for this visit.
    }
    const resolution = resolveServerUrl({
        stored,
        linked,
        buildDefault: env.buildDefault,
    });
    if (resolution.store !== null) {
        try {
            env.storage.setItem(STORAGE_KEY, resolution.store);
        } catch {
            env.warn(
                "server link followed for this visit only: storage refused",
            );
        }
    }
    if (resolution.refused !== null) {
        env.warn(`server link ignored: ${resolution.refused}`);
    }
    const clean = withoutServerParam(env.href);
    if (clean !== null) {
        env.replaceUrl(clean);
    }
    return resolution;
}

let cached: string | null | undefined;

/**
 * The configured relay origin, or `null`. Read once per page load; the first
 * call follows a `?server=` link, so `main.tsx` calls it at start-up.
 */
export function configuredServerUrl(): string | null {
    if (cached === undefined) {
        cached = readServerUrl({
            href: location.href,
            // Reached through the global on each call, not captured: reading
            // `localStorage` itself throws where the origin is denied storage,
            // and `readServerUrl` catches only what its calls throw.
            storage: {
                getItem: (key) => localStorage.getItem(key),
                setItem: (key, value) => localStorage.setItem(key, value),
            },
            replaceUrl: (href) => history.replaceState(history.state, "", href),
            buildDefault: import.meta.env.SUNRISE_WEB_DEFAULT_SERVER_URL,
            warn: (message) => console.warn(message),
        }).url;
    }
    return cached;
}
