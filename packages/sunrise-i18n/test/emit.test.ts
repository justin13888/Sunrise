/**
 * The four emitted files: that the committed ones are current, and that each
 * emitter says what its consumer needs.
 *
 * The drift half compares the emitters to themselves and is structurally
 * blind to a wrong emitter, as the design tokens' drift test notes of its own.
 * The rest pins the shapes a consumer depends on: the String Catalog's
 * positional arguments and plural substitutions (which the Apple build only
 * checks for well-formedness), the Rust binding's locale dispatch, and the
 * TypeScript accessor tree the web client calls.
 */

import { readFile } from "node:fs/promises";
import { describe, expect, it } from "vitest";
import { GENERATED_DIR, OUTPUTS } from "../build";
import { createMessages, locales } from "../generated/messages";
import { buildCatalog, loadCatalog } from "../src/catalog";
import { emitRust } from "../src/emit-rust";
import { emitSwift, emitXcstrings } from "../src/emit-swift";
import { emitTs } from "../src/emit-ts";
import { camel, pascal, rustIdent, swiftIdent } from "../src/names";

const committed = await loadCatalog();

describe("the committed binding files are current", () => {
    for (const [name, emit] of OUTPUTS) {
        it(`generated/${name}`, async () => {
            const onDisk = await readFile(new URL(name, GENERATED_DIR), "utf8");
            expect(
                onDisk,
                `generated/${name} is stale; regenerate it with \`mise run i18n\``,
            ).toBe(emit(committed));
        });
    }
});

const fixture = buildCatalog(
    new Map([
        [
            "en",
            [
                "[common]",
                'product_name = "Sunrise"',
                "[apple.devices]",
                'title = "Devices"',
                'unwound = "{name} — 100% {count, plural, =0 {none} one {# device of {name}} other {# devices}}"',
                "[cli.sync]",
                'live = "sync: live"',
                'pending = "{count, plural, one {# pending} other {# pending}}."',
                'quote = "\'"',
                "[cli.match]",
                'type = "keyword {n, number}"',
                "[web.app]",
                'title = "Hi */"',
            ].join("\n"),
        ],
        [
            "pl",
            [
                "[cli.sync]",
                'pending = "{count, plural, one {# oczekuje} few {# oczekują} many {# oczekuje} other {# oczekuje}}."',
                "[apple.devices]",
                'title = "Urządzenia"',
            ].join("\n"),
        ],
    ]),
);

describe("emitXcstrings", () => {
    const doc = JSON.parse(emitXcstrings(fixture));

    it("carries only the Apple and common keys", () => {
        expect(Object.keys(doc.strings)).toEqual([
            "apple.devices.title",
            "apple.devices.unwound",
            "common.product_name",
        ]);
        expect(doc.sourceLanguage).toBe("en");
    });

    it("writes each translation as a localization", () => {
        expect(doc.strings["apple.devices.title"].localizations).toEqual({
            en: { stringUnit: { state: "translated", value: "Devices" } },
            pl: { stringUnit: { state: "translated", value: "Urządzenia" } },
        });
    });

    it("numbers arguments by source order, escapes %, and renders a plural as a substitution", () => {
        expect(doc.strings["apple.devices.unwound"].localizations.en).toEqual({
            stringUnit: {
                state: "translated",
                value: "%1$@ — 100%% %#@count@",
            },
            substitutions: {
                count: {
                    argNum: 2,
                    formatSpecifier: "lld",
                    variations: {
                        plural: {
                            zero: {
                                stringUnit: {
                                    state: "translated",
                                    value: "none",
                                },
                            },
                            one: {
                                stringUnit: {
                                    state: "translated",
                                    value: "%arg device of %1$@",
                                },
                            },
                            other: {
                                stringUnit: {
                                    state: "translated",
                                    value: "%arg devices",
                                },
                            },
                        },
                    },
                },
            },
        });
    });
});

describe("emitSwift", () => {
    const swift = emitSwift(fixture);

    it("drops the apple surface and keeps common under its own enum", () => {
        expect(swift).toContain("    enum Devices {");
        expect(swift).toContain("    enum Common {");
        expect(swift).toContain(
            '        static var productName: String { L10n.tr("common.product_name") }',
        );
        expect(swift).not.toContain("cli.");
        expect(swift).not.toContain("web.");
    });

    it("types integer arguments as Int and string ones as String, in source order", () => {
        expect(swift).toContain(
            "        static func unwound(name: String, count: Int) -> String {",
        );
        expect(swift).toContain(
            'L10n.tr("apple.devices.unwound", name, count)',
        );
    });

    it("refuses an apple.common key, which would collide with common", () => {
        const clash = buildCatalog(
            new Map([["en", '[common]\na = "a"\n[apple.common]\nb = "b"']]),
        );
        expect(() => emitSwift(clash)).toThrow("apple.common");
    });
});

describe("emitRust", () => {
    const rust = emitRust(fixture);

    it("carries only the CLI and common keys", () => {
        expect(rust).toContain('pub const LOCALES: &[&str] = &["en", "pl"];');
        expect(rust).toContain("pub mod sync {");
        expect(rust).toContain("pub mod common {");
        expect(rust).not.toContain("Devices");
    });

    it("dispatches a translated key on the runtime locale", () => {
        expect(rust).toContain(
            '        match rt::locale() {\n            "pl" => {',
        );
        expect(rust).toContain("rt::Category::Few => {");
    });

    it("escapes keywords, pushes single characters, and imports rt only where used", () => {
        expect(rust).toContain("pub mod r#match {");
        expect(rust).toContain("pub fn r#type(n: i64) -> String {");
        expect(rust).toContain("out.push('\\'');");
        expect(rust).toContain("out.push('.');");
        // `common` holds one literal message: nothing there calls `rt`.
        expect(rust).toMatch(/pub mod common \{\n {4}\/\/\/ Sunrise/);
    });

    it("refuses a cli.common key", () => {
        const clash = buildCatalog(
            new Map([["en", '[common]\na = "a"\n[cli.common]\nb = "b"']]),
        );
        expect(() => emitRust(clash)).toThrow("cli.common");
    });
});

describe("emitTs", () => {
    it("carries web, docs and common, and escapes a doc comment's terminator", () => {
        const ts = emitTs(fixture);
        expect(ts).toContain('export const locales = ["en"] as const;');
        expect(ts).toContain("/** Hi *\\/ */");
        expect(ts).not.toContain("cli.sync");
    });

    it("the committed binding renders through the typed tree", () => {
        expect(locales[0]).toBe("en");
        const t = createMessages(["pl-PL", "en-US"]);
        expect(t.locale).toBe("en");
        expect(t.dir).toBe("ltr");
        expect(t.web.app.empty()).toBe("Nothing on the list.");
        expect(t.common.productName()).toBe("Sunrise");
    });
});

describe("names", () => {
    it("spells segments for each binding", () => {
        expect(camel("remove_confirm_title")).toBe("removeConfirmTitle");
        expect(pascal("devices")).toBe("Devices");
        expect(swiftIdent("default")).toBe("`default`");
        expect(swiftIdent("title")).toBe("title");
        expect(rustIdent("type")).toBe("r#type");
        expect(rustIdent("live")).toBe("live");
    });
});
