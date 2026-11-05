import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { t } from "./i18n";
import {
    currentServerText,
    forgetServerUrl,
    forgottenText,
    issueText,
    planServerChange,
    type ServerSettingsView,
    type SettingsStorage,
    saveServerUrl,
    settingsView,
} from "./server-settings";
import {
    type RelayUrlIssue,
    resolveServerUrl,
    STORAGE_KEY,
} from "./server-url";

const MINE = "https://mine.example";
const THEIRS = "https://theirs.example";
const OPS = "https://ops.example";

function view(
    stored: string | null,
    buildDefault?: string,
    linked: string | null = null,
): ServerSettingsView {
    return settingsView(resolveServerUrl({ stored, linked, buildDefault }));
}

function fakeStorage(
    initial: Record<string, string> = {},
    fail: { set?: boolean; remove?: boolean } = {},
) {
    const items = { ...initial };
    const storage: SettingsStorage = {
        getItem: (key) => items[key] ?? null,
        setItem(key, value) {
            if (fail.set) {
                throw new Error("QuotaExceededError");
            }
            items[key] = value;
        },
        removeItem(key) {
            if (fail.remove) {
                throw new Error("SecurityError");
            }
            delete items[key];
        },
    };
    return { storage, items };
}

describe("settingsView", () => {
    it("names a stored relay as this browser's choice, which can be forgotten", () => {
        expect(view(MINE, OPS)).toEqual({
            url: MINE,
            source: "stored",
            canForget: true,
            offered: null,
        });
    });

    it("names the build's relay and offers nothing to forget", () => {
        expect(view(null, OPS)).toEqual({
            url: OPS,
            source: "build",
            canForget: false,
            offered: null,
        });
        expect(view(null)).toMatchObject({ url: null, source: "none" });
    });

    it("counts a link followed at start-up as stored, since it was stored", () => {
        expect(view(null, OPS, THEIRS)).toMatchObject({
            url: THEIRS,
            source: "stored",
            canForget: true,
            offered: null,
        });
    });

    it("offers a link the start-up read refused for another stored relay", () => {
        expect(view(MINE, undefined, THEIRS).offered).toBe(THEIRS);
        // A malformed link is not a choice anyone can confirm.
        expect(view(MINE, undefined, "http://theirs.example").offered).toBe(
            null,
        );
    });
});

describe("planServerChange", () => {
    it("refuses what the shared rule refuses, naming the same issue", () => {
        const vectors = JSON.parse(
            readFileSync(
                new URL(
                    "../../../crates/sunrise-client-core/vectors/relay-url.json",
                    import.meta.url,
                ),
                "utf8",
            ),
        ) as { cases: { input: string; issue?: RelayUrlIssue }[] };
        const refused = vectors.cases.filter((c) => c.issue !== undefined);
        expect(refused.length).toBeGreaterThan(0);
        for (const { input, issue } of refused) {
            const expanded = input.replace("{256}", "a".repeat(256));
            expect(planServerChange(expanded, view(MINE)), input).toEqual({
                kind: "invalid",
                issue,
            });
        }
    });

    it("stores straight away when nothing was chosen before", () => {
        expect(
            planServerChange(" https://Relay.Example/ ", view(null)),
        ).toEqual({ kind: "save", origin: "https://relay.example" });
        expect(planServerChange(THEIRS, view(null, OPS))).toEqual({
            kind: "save",
            origin: THEIRS,
        });
    });

    it("asks before one stored relay replaces another", () => {
        expect(planServerChange(THEIRS, view(MINE))).toEqual({
            kind: "confirm",
            from: MINE,
            to: THEIRS,
        });
    });

    it("writes nothing for the relay already in use", () => {
        expect(planServerChange(`${MINE}/`, view(MINE))).toEqual({
            kind: "unchanged",
            origin: MINE,
        });
        expect(planServerChange(OPS, view(null, OPS)).kind).toBe("unchanged");
    });
});

describe("saveServerUrl", () => {
    it("stores the origin and resolves to it", () => {
        const { storage, items } = fakeStorage({ [STORAGE_KEY]: MINE });
        const write = saveServerUrl(storage, THEIRS, OPS);
        expect(items[STORAGE_KEY]).toBe(THEIRS);
        expect(write.persisted).toBe(true);
        expect(write.resolution).toMatchObject({
            url: THEIRS,
            source: "stored",
            offered: null,
        });
    });

    it("keeps the choice for the visit when storage refuses it", () => {
        const { storage, items } = fakeStorage({}, { set: true });
        const write = saveServerUrl(storage, THEIRS, undefined);
        expect(items[STORAGE_KEY]).toBeUndefined();
        expect(write).toMatchObject({
            persisted: false,
            resolution: { url: THEIRS },
        });
    });

    it("never stores a value the next start-up would discard", () => {
        const { storage, items } = fakeStorage();
        expect(() =>
            saveServerUrl(storage, "http://relay.example", undefined),
        ).toThrow("insecure_remote");
        expect(items).toEqual({});
    });
});

describe("forgetServerUrl", () => {
    it("removes only the setting and falls back to the build's relay", () => {
        const { storage, items } = fakeStorage({
            [STORAGE_KEY]: MINE,
            "sunrise-vault": "kept",
        });
        const write = forgetServerUrl(storage, OPS);
        expect(items).toEqual({ "sunrise-vault": "kept" });
        expect(write).toEqual({
            persisted: true,
            resolution: {
                url: OPS,
                source: "build",
                store: null,
                refused: null,
                offered: null,
            },
        });
        expect(forgottenText(write.resolution)).toBe(
            t.web.settings.forgotToBuild({ url: OPS }),
        );
    });

    it("leaves no relay when the build has none", () => {
        const { storage } = fakeStorage({ [STORAGE_KEY]: MINE });
        const write = forgetServerUrl(storage, undefined);
        expect(write.resolution.url).toBeNull();
        expect(forgottenText(write.resolution)).toBe(
            t.web.settings.forgotToNone(),
        );
    });

    it("says so when storage refuses the removal", () => {
        const { storage, items } = fakeStorage(
            { [STORAGE_KEY]: MINE },
            { remove: true },
        );
        expect(forgetServerUrl(storage, undefined).persisted).toBe(false);
        expect(items[STORAGE_KEY]).toBe(MINE);
    });
});

describe("the settings view's sentences", () => {
    it("gives every relay URL issue a sentence of its own", () => {
        const issues: RelayUrlIssue[] = [
            "empty",
            "too_long",
            "not_a_url",
            "web_socket",
            "unsupported_scheme",
            "insecure_remote",
            "not_an_origin",
        ];
        const texts = issues.map(issueText);
        expect(new Set(texts).size).toBe(issues.length);
        for (const text of texts) {
            expect(text.length).toBeGreaterThan(0);
        }
    });

    it("names the relay in use by where it came from", () => {
        expect(currentServerText(view(MINE))).toBe(
            t.web.settings.serverStored({ url: MINE }),
        );
        expect(currentServerText(view(null, OPS))).toBe(
            t.web.settings.serverBuild({ url: OPS }),
        );
        expect(currentServerText(view(null))).toBe(t.web.settings.serverNone());
    });
});
