/**
 * The formatter the web bundle ships, and the plural-coverage check built on
 * it — run here over a Polish and an Arabic catalog that `i18n/` does not
 * have yet, so the shapes the CI matrix exists for are exercised by an actual
 * translation and not only by a template.
 */

import { describe, expect, it } from "vitest";
import { buildCatalog, loadCatalog } from "../src/catalog";
import { coverage } from "../src/coverage";
import { parseMessage } from "../src/message";
import {
    categories,
    integerSample,
    pluralProblems,
    reshape,
} from "../src/plural";
import {
    createFormatter,
    direction,
    formatParts,
    negotiate,
} from "../src/runtime";

describe("formatParts", () => {
    const tasks = parseMessage(
        "{stream}: {count, plural, =0 {no tasks} one {# task} other {# tasks}}",
    );

    it.each([
        [0, "Inbox: no tasks"],
        [1, "Inbox: 1 task"],
        [2, "Inbox: 2 tasks"],
        [1234, "Inbox: 1,234 tasks"],
    ])("renders %i", (count, want) => {
        expect(formatParts("en", tasks, { stream: "Inbox", count })).toBe(want);
    });

    it("formats integer arguments in the locale's digits", () => {
        expect(
            formatParts("en", parseMessage("{a} / {b, number}"), {
                a: 10000,
                b: 2500,
            }),
        ).toBe("10,000 / 2,500");
    });

    it("refuses a missing or non-integer argument", () => {
        expect(() => formatParts("en", parseMessage("{a}"), {})).toThrow(
            "argument `a` is missing",
        );
        expect(() =>
            formatParts("en", tasks, { stream: "x", count: 1.5 }),
        ).toThrow("argument `count` must be an integer");
        expect(() => formatParts("en", [{ kind: "pound" }], {})).toThrow(
            "`#` outside a plural arm",
        );
    });

    it("selects Arabic and Polish categories", () => {
        const ar = parseMessage(
            "{n, plural, zero {z} one {o} two {t} few {f} many {m} other {x}}",
        );
        // No `#` here: whether `ar` defaults to Arabic-Indic or Latin digits
        // moved between CLDR releases, and this is about arm selection.
        expect(
            [0, 1, 2, 3, 11, 100].map((n) => formatParts("ar", ar, { n })),
        ).toEqual(["z", "o", "t", "f", "m", "x"]);
        const pl = parseMessage(
            "{n, plural, one {o} few {f} many {m} other {x}}",
        );
        expect(
            [1, 2, 5, 22, 25].map((n) => formatParts("pl", pl, { n })),
        ).toEqual(["o", "f", "m", "f", "m"]);
    });
});

describe("negotiate and direction", () => {
    it("prefers an exact tag, then the language, then the source", () => {
        expect(negotiate(["en", "pt-BR", "pt"], ["pt-br"], "en")).toBe("pt-BR");
        expect(negotiate(["en", "pt"], ["pt-PT", "en"], "en")).toBe("pt");
        expect(negotiate(["en", "pl"], ["de-DE", "fr"], "en")).toBe("en");
        expect(negotiate(["en"], [], "en")).toBe("en");
    });

    it("knows which languages are written right to left", () => {
        expect(direction("ar")).toBe("rtl");
        expect(direction("he-IL")).toBe("rtl");
        expect(direction("en")).toBe("ltr");
        expect(direction("pl")).toBe("ltr");
    });
});

describe("createFormatter", () => {
    const catalog = {
        en: {
            "web.a": [{ kind: "text" as const, value: "A" }],
            "web.n": parseMessage("{n, plural, one {# file} other {# files}}"),
        },
        pl: { "web.a": [{ kind: "text" as const, value: "Ą" }] },
    };

    it("serves the translation where there is one", () => {
        expect(createFormatter(catalog, "en", "pl")("web.a")).toBe("Ą");
    });

    it("falls back to the source, formatted by the source's rules", () => {
        // Polish would file 2 under `few`, which the English message has no
        // arm for; English rules pick `other`, which it does.
        expect(createFormatter(catalog, "en", "pl")("web.n", { n: 2 })).toBe(
            "2 files",
        );
    });

    it("refuses a key no locale has", () => {
        expect(() => createFormatter(catalog, "en", "en")("web.zzz")).toThrow(
            "no message `web.zzz`",
        );
    });
});

describe("plural rules", () => {
    it("reads the matrix locales' categories from CLDR", () => {
        expect(categories("en")).toEqual(["one", "other"]);
        expect(categories("pl")).toEqual(["one", "few", "many", "other"]);
        expect(categories("ar")).toEqual([
            "zero",
            "one",
            "two",
            "few",
            "many",
            "other",
        ]);
    });

    it("finds an integer sample per category, and none where only fractions reach", () => {
        expect(integerSample("ar", "many")).toBe(11);
        expect(integerSample("pl", "few")).toBe(2);
        expect(integerSample("pl", "other")).toBeUndefined();
    });

    it("reshapes a source message into a translator's template", () => {
        const en = parseMessage(
            "{n, plural, =0 {none} one {# file} other {# files}}",
        );
        const ar = reshape(en, "ar");
        expect(pluralProblems("ar", ar)).toEqual([]);
        const arms = ar[0]?.kind === "plural" ? ar[0].arms : [];
        expect(arms.map((a) => a.key)).toEqual([
            "zero",
            "one",
            "two",
            "few",
            "many",
            "other",
        ]);
        expect(arms[0]?.parts).toEqual([{ kind: "text", value: "none" }]);
        const pl = reshape(en, "pl");
        expect(pluralProblems("pl", pl)).toEqual([]);
        expect(pl[0]?.kind === "plural" && pl[0].arms[0]?.key).toBe("=0");
        expect(reshape(parseMessage("plain"), "ar")).toEqual(
            parseMessage("plain"),
        );
    });
});

describe("coverage", () => {
    const catalog = buildCatalog(
        new Map([
            [
                "en",
                '[web]\nfiles = "{n, plural, =0 {No files} one {# file} other {# files}} in {dir}"\nplain = "x"',
            ],
            [
                "pl",
                '[web]\nfiles = "{n, plural, =0 {Brak plików} one {# plik} few {# pliki} many {# plików} other {# pliku}} w {dir}"',
            ],
        ]),
    );

    it("checks a translated locale against its own messages", () => {
        const report = coverage(catalog, "pl");
        expect(report).toMatchObject({
            locale: "pl",
            source: "translated",
            messages: 1,
            plurals: 1,
            problems: [],
        });
    });

    it("checks an untranslated locale against the source's template", () => {
        expect(coverage(catalog, "ar")).toMatchObject({
            source: "template",
            messages: 2,
            plurals: 1,
            categories: ["zero", "one", "two", "few", "many", "other"],
            problems: [],
        });
    });

    it("passes for the committed catalog in every matrix locale", async () => {
        const committed = await loadCatalog();
        for (const locale of ["en", "pl", "ar"]) {
            expect(coverage(committed, locale).problems).toEqual([]);
        }
    });

    it("fails a message that is missing an arm", () => {
        const bad = {
            ...catalog,
            messages: new Map([
                ...catalog.messages,
                [
                    "pl",
                    new Map([
                        [
                            "web.files",
                            parseMessage(
                                "{n, plural, one {#} other {#} } {dir}",
                            ),
                        ],
                    ]),
                ],
            ]),
        };
        expect(coverage(bad, "pl").problems).toEqual(
            expect.arrayContaining([
                expect.stringContaining("is missing `few`, `many` for pl"),
                expect.stringContaining(
                    "n=2 (pl `few`) did not select its `few` arm",
                ),
            ]),
        );
    });
});
