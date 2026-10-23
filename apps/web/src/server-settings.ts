/**
 * Seeing, changing and clearing the sync server (#387).
 *
 * `server-url.ts` decides which relay a page load uses and refuses to let a
 * `?server=` link move a browser that already chose one, because nothing on
 * screen would say the relay had moved. This is the screen that says so: it
 * shows the relay, accepts a new one through the shared relay URL rule
 * (`checkRelayUrl`, held to `crates/sunrise-client-core/vectors/relay-url.json`),
 * asks before one stored relay replaces another, and offers the same
 * confirmation to a link the start-up read refused.
 *
 * Only the setting changes. The vault is in this browser's own storage and is
 * not touched by any of this: clearing the site's data was the old way to
 * change relay, and it deleted the vault with it.
 *
 * Everything but the storage writes in `saveServerUrl` and `forgetServerUrl`
 * is pure and is what `server-settings.test.ts` drives; `ServerSettings.tsx`
 * renders it.
 */

import { t } from "./i18n";
import {
    checkRelayUrl,
    type RelayUrlIssue,
    resolveServerUrl,
    type ServerUrlResolution,
    STORAGE_KEY,
} from "./server-url";

/** What the settings view shows about the relay in use. */
export interface ServerSettingsView {
    /** The relay origin in use, or `null` when none is configured. */
    url: string | null;
    /** Whether `url` is this browser's own choice, the build's, or nothing. */
    source: "stored" | "build" | "none";
    /** Whether "Forget this server" applies: only a stored choice is forgotten. */
    canForget: boolean;
    /**
     * A relay a `?server=` link named and the start-up read refused because a
     * different one is stored. The view offers it behind the same confirmation
     * a typed replacement gets; `null` when there is none.
     */
    offered: string | null;
}

/** The view for a resolution, including the link it refused, if any. */
export function settingsView(
    resolution: ServerUrlResolution,
): ServerSettingsView {
    const offered = resolution.offered;
    const source = resolution.source === "link" ? "stored" : resolution.source;
    return {
        url: resolution.url,
        source,
        canForget: source === "stored",
        offered:
            offered !== null && offered !== resolution.url ? offered : null,
    };
}

/** What submitting a typed address leads to. */
export type ServerChange =
    /** The address breaks the shared rule; the field keeps the typed text. */
    | { kind: "invalid"; issue: RelayUrlIssue }
    /** The address is the relay already in use; nothing to write. */
    | { kind: "unchanged"; origin: string }
    /** Nothing was stored, so the new relay is stored straight away. */
    | { kind: "save"; origin: string }
    /** Another relay is stored, and replacing it is confirmed first. */
    | { kind: "confirm"; from: string; to: string };

/**
 * Decide what a typed address does, given the relay in use.
 *
 * Only a *stored* relay asks for confirmation before it is replaced: that is
 * this browser's own earlier choice, and the one a link was refused for. A
 * build default was never chosen here, so choosing over it needs no second
 * question.
 */
export function planServerChange(
    input: string,
    current: ServerSettingsView,
): ServerChange {
    const checked = checkRelayUrl(input);
    if ("issue" in checked) {
        return { kind: "invalid", issue: checked.issue };
    }
    const origin = checked.origin;
    if (origin === current.url) {
        return { kind: "unchanged", origin };
    }
    if (current.source === "stored" && current.url !== null) {
        return { kind: "confirm", from: current.url, to: origin };
    }
    return { kind: "save", origin };
}

/** The storage the settings view writes, so a test can hand it a fake. */
export type SettingsStorage = Pick<
    Storage,
    "getItem" | "setItem" | "removeItem"
>;

/** What a write left in force. */
export interface SettingsWrite {
    /** The resolution the page now uses. */
    resolution: ServerUrlResolution;
    /** Whether storage accepted the write; `false` means this visit only. */
    persisted: boolean;
}

/**
 * Store `origin` as this browser's relay. Called for a `save`, and for a
 * `confirm` or a link offer once the user has confirmed it.
 *
 * `origin` must be one `planServerChange` produced, or a link offer that
 * `readServerUrl` already parsed; anything else is refused rather than stored,
 * so the store never holds a value the next start-up would discard.
 */
export function saveServerUrl(
    storage: SettingsStorage,
    origin: string,
    buildDefault: string | undefined,
): SettingsWrite {
    const checked = checkRelayUrl(origin);
    if ("issue" in checked) {
        throw new Error(`not a relay origin: ${checked.issue}`);
    }
    let persisted = true;
    try {
        storage.setItem(STORAGE_KEY, checked.origin);
    } catch {
        persisted = false;
    }
    return {
        resolution: resolveServerUrl({
            stored: checked.origin,
            linked: null,
            buildDefault,
        }),
        persisted,
    };
}

/**
 * Remove this browser's stored relay. What applies afterwards is the build
 * default, or nothing; a link is not consulted, because the page's address
 * no longer carries one once the start-up read has run.
 */
export function forgetServerUrl(
    storage: SettingsStorage,
    buildDefault: string | undefined,
): SettingsWrite {
    let persisted = true;
    try {
        storage.removeItem(STORAGE_KEY);
    } catch {
        persisted = false;
    }
    return {
        resolution: resolveServerUrl({
            stored: null,
            linked: null,
            buildDefault,
        }),
        persisted,
    };
}

/** The sentence shown under the field for a refused address. */
export function issueText(issue: RelayUrlIssue): string {
    const issues = t.web.settings.issue;
    switch (issue) {
        case "empty":
            return issues.empty();
        case "too_long":
            return issues.tooLong();
        case "not_a_url":
            return issues.notAUrl();
        case "web_socket":
            return issues.webSocket();
        case "unsupported_scheme":
            return issues.unsupportedScheme();
        case "insecure_remote":
            return issues.insecureRemote();
        case "not_an_origin":
            return issues.notAnOrigin();
    }
}

/** The sentence naming the relay in use, by where it came from. */
export function currentServerText(view: ServerSettingsView): string {
    if (view.url === null) {
        return t.web.settings.serverNone();
    }
    return view.source === "stored"
        ? t.web.settings.serverStored({ url: view.url })
        : t.web.settings.serverBuild({ url: view.url });
}

/** The sentence shown once a forget has run. */
export function forgottenText(resolution: ServerUrlResolution): string {
    return resolution.url === null
        ? t.web.settings.forgotToNone()
        : t.web.settings.forgotToBuild({ url: resolution.url });
}
