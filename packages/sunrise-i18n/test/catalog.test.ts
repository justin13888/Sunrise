/**
 * The catalog loader and the message subset: what `i18n/*.toml` may say, and
 * the reasons it is refused when it says something else.
 */

import { describe, expect, it } from "vitest";
import {
    buildCatalog,
    CatalogError,
    flatten,
    keysFor,
    loadCatalog,
    surfaceOf,
} from "../src/catalog";
import { argumentsOf, MessageError, parseMessage } from "../src/message";

function problemsOf(sources: Record<string, string>): string[] {
    try {
        buildCatalog(new Map(Object.entries(sources)));
    } catch (error) {
        if (error instanceof CatalogError) {
            return [...error.problems];
        }
        throw error;
    }
    return [];
}

describe("parseMessage", () => {
    it("reads text, arguments, numbers and plurals", () => {
        expect(
            parseMessage(
                "{name} has {count, plural, =0 {none} one {# task} other {# tasks}}, {n, number} total",
            ),
        ).toEqual([
            { kind: "arg", name: "name" },
            { kind: "text", value: " has " },
            {
                kind: "plural",
                name: "count",
                arms: [
                    { key: "=0", parts: [{ kind: "text", value: "none" }] },
                    {
                        key: "one",
                        parts: [
                            { kind: "pound" },
                            { kind: "text", value: " task" },
                        ],
                    },
                    {
                        key: "other",
                        parts: [
                            { kind: "pound" },
                            { kind: "text", value: " tasks" },
                        ],
                    },
                ],
            },
            { kind: "text", value: ", " },
            { kind: "number", name: "n" },
            { kind: "text", value: " total" },
        ]);
    });

    it("keeps apostrophes and quoted braces as text", () => {
        expect(parseMessage("vault's '{literal}' <b>")).toEqual([
            { kind: "text", value: "vault's {literal} <b>" },
        ]);
    });

    it.each([
        ["{a, select, x {1} other {2}}", "select"],
        ["{a, selectordinal, one {#st} other {#th}}", "selectordinal"],
        ["{a, date}", "date"],
        ["{a, time}", "time"],
        ["{a, number, percent}", "style"],
        ["{a, plural, offset:1 one {x} other {y}}", "offset"],
        ["{a, plural, =1 {x} other {y}}", "arm `=1`"],
        [
            "{a, plural, one {{b, plural, one {x} other {y}}} other {z}}",
            "nested",
        ],
        ["{a, plural, one {x}}", "not valid ICU"],
        ["{Bad}", "snake_case"],
        ["{a_1}", "snake_case"],
        ["{self}", "not self"],
        ["{unclosed", "not valid ICU"],
    ])("refuses %s (%s)", (source, reason) => {
        expect(() => parseMessage(source)).toThrow(MessageError);
        expect(() => parseMessage(source)).toThrow(reason);
    });
});

describe("argumentsOf", () => {
    it("types a plural or number argument as an integer wherever it appears", () => {
        const args = argumentsOf(
            parseMessage(
                "{who}: {count} of {count, plural, one {# {thing}} other {# {thing}s}} {total, number}",
            ),
        );
        expect([...args]).toEqual([
            ["who", "string"],
            ["count", "integer"],
            ["thing", "string"],
            ["total", "integer"],
        ]);
    });
});

describe("flatten and surfaces", () => {
    it("flattens nested tables into dotted keys", () => {
        const problems: string[] = [];
        expect([
            ...flatten("t", { web: { app: { title: "x" } } }, problems),
        ]).toEqual([["web.app.title", "x"]]);
        expect(problems).toEqual([]);
    });

    it("names the surface of a key", () => {
        expect(surfaceOf("cli.sync.live")).toBe("cli");
        expect(surfaceOf("tui.x")).toBeUndefined();
        expect(surfaceOf("toString.x")).toBeUndefined();
    });
});

describe("buildCatalog", () => {
    it("refuses a missing source catalog", () => {
        expect(problemsOf({ pl: "" })).toEqual(["i18n/en.toml is missing"]);
    });

    it("reports every problem at once, each with its file and key", () => {
        const problems = problemsOf({
            en: [
                'top = "no surface"',
                "[web]",
                'Bad = "x"',
                "n = 3",
                'bad_arg = "{Name}"',
                'zero_arm = "{n, plural, zero {none} one {#} other {#}}"',
                'missing = "{n, plural, other {#}}"',
                "[tui]",
                'x = "y"',
            ].join("\n"),
        });
        expect(problems).toEqual([
            expect.stringContaining("`top`: a message needs a surface table"),
            expect.stringContaining("`web.Bad`: key segments are snake_case"),
            expect.stringContaining("`web.n`: a message must be a string"),
            expect.stringContaining("`web.bad_arg`: argument `Name`"),
            expect.stringContaining(
                "`web.zero_arm`: {n, plural, …} has `zero`, which en never selects",
            ),
            expect.stringContaining(
                "`web.missing`: {n, plural, …} is missing `one`",
            ),
            expect.stringContaining(
                "`tui.x`: the first segment must be a surface",
            ),
        ]);
    });

    it("reports a TOML syntax error with its file", () => {
        expect(problemsOf({ en: "[web" })[0]).toMatch(/^i18n\/en\.toml: /);
    });

    it("holds a translation to the source's keys and arguments", () => {
        const problems = problemsOf({
            en: '[web]\ngreet = "Hi {name}"\ncount = "{n, plural, one {#} other {#}}"',
            pl: [
                "[web]",
                'greet = "Cześć {who}"',
                'extra = "x"',
                'count = "{n, plural, one {#} other {#}}"',
            ].join("\n"),
        });
        expect(problems).toEqual([
            "i18n/pl.toml: `web.greet`: takes {who: string}, but the source takes {name: string}",
            "i18n/pl.toml: `web.extra`: not a key of i18n/en.toml",
            expect.stringContaining(
                "i18n/pl.toml: `web.count`: {n, plural, …} is missing `few`, `many` for pl",
            ),
        ]);
    });

    it("refuses =0 in a locale whose rules have zero", () => {
        const problems = problemsOf({
            en: '[web]\nc = "{n, plural, =0 {none} one {#} other {#}}"',
            ar: '[web]\nc = "{n, plural, =0 {a} zero {b} one {c} two {d} few {e} many {f} other {g}}"',
        });
        expect(problems).toEqual([
            expect.stringContaining("has =0, but ar has a `zero` category"),
        ]);
    });

    it("orders locales source first and routes keys to their bindings", () => {
        const catalog = buildCatalog(
            new Map([
                ["pl", '[web]\na = "b"'],
                [
                    "en",
                    '[common]\nx = "x"\n[web]\na = "a"\n[cli]\nb = "b"\n[apple]\nc = "c"\n[docs]\nd = "d"',
                ],
            ]),
        );
        expect(catalog.locales).toEqual(["en", "pl"]);
        expect(keysFor(catalog, "ts")).toEqual(["common.x", "web.a", "docs.d"]);
        expect(keysFor(catalog, "swift")).toEqual(["common.x", "apple.c"]);
        expect(keysFor(catalog, "rust")).toEqual(["common.x", "cli.b"]);
    });
});

describe("the committed catalog", () => {
    it("loads without a problem", async () => {
        const catalog = await loadCatalog();
        expect(catalog.locales[0]).toBe("en");
        expect(catalog.messages.get("en")?.size).toBeGreaterThan(0);
    });

    it("refuses a file that is not <locale>.toml", async () => {
        const dir = new URL("./fixtures/stray/", import.meta.url);
        await expect(loadCatalog(dir)).rejects.toThrow(
            "i18n/notes.txt: not a catalog",
        );
    });
});
