/**
 * The catalog: `i18n/<locale>.toml`, read, flattened, parsed and checked.
 *
 * `i18n/en.toml` is the single source (`docs/10-cross-cutting/i18n.md`
 * §String catalog). Every other `i18n/<locale>.toml` is a translation of it
 * and may hold only keys `en.toml` holds, with the same arguments of the same
 * kinds. A translation may omit a key; the key then falls back to English.
 *
 * Keys are dotted paths from TOML tables. The first segment is the *surface*
 * the string belongs to, and it decides which bindings carry it — so the web
 * bundle does not ship the CLI's help text, and the Apple String Catalog does
 * not hold the web's.
 */

import { readdir, readFile } from "node:fs/promises";
import { parse as parseToml } from "smol-toml";
import {
    type ArgKind,
    argumentsOf,
    type Message,
    MessageError,
    parseMessage,
} from "./message";
import { pluralProblems } from "./plural";

/** The catalog's source locale. */
export const SOURCE_LOCALE = "en";

/**
 * The surfaces a key may belong to, and which binding carries each.
 *
 * `common` is carried by every binding: a string two clients must say the
 * same way (a product name, a shared status) is written once.
 */
export const SURFACES = {
    common: ["ts", "swift", "rust"],
    web: ["ts"],
    docs: ["ts"],
    apple: ["swift"],
    cli: ["rust"],
} as const satisfies Record<string, readonly Binding[]>;

/** A generated binding. */
export type Binding = "ts" | "swift" | "rust";

/** A surface name. */
export type Surface = keyof typeof SURFACES;

/** One locale's messages, by full dotted key, in source order. */
export type LocaleMessages = ReadonlyMap<string, Message>;

/** A loaded and validated catalog. */
export interface Catalog {
    /** Every locale present, source first, the rest sorted. */
    readonly locales: readonly string[];
    /** `locale → key → message`. */
    readonly messages: ReadonlyMap<string, LocaleMessages>;
    /** Every source key's arguments, which every translation must match. */
    readonly args: ReadonlyMap<string, ReadonlyMap<string, ArgKind>>;
}

/** A catalog the loader refuses. Every problem found, not just the first. */
export class CatalogError extends Error {
    override name = "CatalogError";
    constructor(readonly problems: readonly string[]) {
        super(problems.join("\n"));
    }
}

/** Where the catalog lives: `i18n/` at the repository root. */
export const CATALOG_DIR = new URL("../../../i18n/", import.meta.url);

/**
 * One key segment: snake_case whose every `_` is followed by a letter, so the
 * camelCase and PascalCase spellings the bindings derive are one-to-one with
 * it (`a_1` and `a1` would otherwise both be `a1`).
 */
const SEGMENT = /^[a-z][a-z0-9]*(?:_[a-z][a-z0-9]*)*$/;

/** Segments no binding can spell: `r#self` is not a Rust identifier. */
const RESERVED_SEGMENTS = new Set(["self", "super", "crate"]);
const LOCALE_FILE = /^([a-z]{2,3}(?:-[A-Z][a-z]{3})?(?:-[A-Z]{2})?)\.toml$/;

/**
 * Flatten one parsed TOML document into `dotted.key → value`, refusing any
 * value that is not a string and any key segment that is not snake_case.
 */
export function flatten(
    file: string,
    doc: Record<string, unknown>,
    problems: string[],
): Map<string, string> {
    const out = new Map<string, string>();
    const walk = (prefix: string[], node: Record<string, unknown>): void => {
        for (const [segment, value] of Object.entries(node)) {
            const path = [...prefix, segment];
            const key = path.join(".");
            if (!SEGMENT.test(segment) || RESERVED_SEGMENTS.has(segment)) {
                problems.push(
                    `${file}: \`${key}\`: key segments are snake_case, each \`_\` followed by a letter, and not self, super or crate`,
                );
                continue;
            }
            if (typeof value === "string") {
                if (path.length < 2) {
                    problems.push(
                        `${file}: \`${key}\`: a message needs a surface table, e.g. [web] or [cli.login]`,
                    );
                    continue;
                }
                out.set(key, value);
            } else if (
                typeof value === "object" &&
                value !== null &&
                !Array.isArray(value) &&
                !(value instanceof Date)
            ) {
                walk(path, value as Record<string, unknown>);
            } else {
                problems.push(
                    `${file}: \`${key}\`: a message must be a string`,
                );
            }
        }
    };
    walk([], doc);
    return out;
}

/** The surface a key belongs to, or `undefined` when it names none. */
export function surfaceOf(key: string): Surface | undefined {
    const first = key.split(".")[0] ?? "";
    return Object.hasOwn(SURFACES, first) ? (first as Surface) : undefined;
}

/**
 * Build a catalog from already-read sources, `locale → TOML text`.
 *
 * Separate from `loadCatalog` so the tests can hand it a Polish or Arabic
 * catalog without one existing in `i18n/`.
 */
export function buildCatalog(sources: ReadonlyMap<string, string>): Catalog {
    const problems: string[] = [];
    const sourceText = sources.get(SOURCE_LOCALE);
    if (sourceText === undefined) {
        throw new CatalogError([`i18n/${SOURCE_LOCALE}.toml is missing`]);
    }
    const locales = [
        SOURCE_LOCALE,
        ...[...sources.keys()].filter((l) => l !== SOURCE_LOCALE).sort(),
    ];
    const messages = new Map<string, Map<string, Message>>();
    const args = new Map<string, Map<string, ArgKind>>();

    for (const locale of locales) {
        const file = `i18n/${locale}.toml`;
        let doc: Record<string, unknown>;
        try {
            doc = parseToml(sources.get(locale) ?? "");
        } catch (error) {
            problems.push(`${file}: ${(error as Error).message}`);
            continue;
        }
        const parsed = new Map<string, Message>();
        for (const [key, value] of flatten(file, doc, problems)) {
            const where = `${file}: \`${key}\``;
            if (surfaceOf(key) === undefined) {
                problems.push(
                    `${where}: the first segment must be a surface (${Object.keys(SURFACES).join(", ")})`,
                );
                continue;
            }
            let message: Message;
            try {
                message = parseMessage(value);
            } catch (error) {
                if (error instanceof MessageError) {
                    problems.push(`${where}: ${error.message}`);
                    continue;
                }
                throw error;
            }
            for (const problem of pluralProblems(locale, message)) {
                problems.push(`${where}: ${problem}`);
            }
            const found = argumentsOf(message);
            if (locale === SOURCE_LOCALE) {
                args.set(key, found);
            } else {
                const expected = args.get(key);
                if (expected === undefined) {
                    problems.push(
                        `${where}: not a key of i18n/${SOURCE_LOCALE}.toml`,
                    );
                    continue;
                }
                const describe = (m: ReadonlyMap<string, ArgKind>): string =>
                    [...m]
                        .map(([n, k]) => `${n}: ${k}`)
                        .sort()
                        .join(", ") || "none";
                if (describe(found) !== describe(expected)) {
                    problems.push(
                        `${where}: takes {${describe(found)}}, but the source takes {${describe(expected)}}`,
                    );
                    continue;
                }
            }
            parsed.set(key, message);
        }
        messages.set(locale, parsed);
    }

    if (problems.length > 0) {
        throw new CatalogError(problems);
    }
    return { locales, messages, args };
}

/** Read every `i18n/<locale>.toml` and build the catalog from them. */
export async function loadCatalog(dir: URL = CATALOG_DIR): Promise<Catalog> {
    const sources = new Map<string, string>();
    const problems: string[] = [];
    for (const name of (await readdir(dir)).sort()) {
        const match = LOCALE_FILE.exec(name);
        if (match?.[1] === undefined) {
            problems.push(
                `i18n/${name}: not a catalog; files here are <locale>.toml, e.g. en.toml or pt-BR.toml`,
            );
            continue;
        }
        sources.set(match[1], await readFile(new URL(name, dir), "utf8"));
    }
    if (problems.length > 0) {
        throw new CatalogError(problems);
    }
    return buildCatalog(sources);
}

/**
 * The keys a binding carries, in source order, with each locale's message for
 * it where that locale has one.
 */
export function keysFor(catalog: Catalog, binding: Binding): string[] {
    const source = catalog.messages.get(SOURCE_LOCALE) ?? new Map();
    return [...source.keys()].filter((key) => {
        const surface = surfaceOf(key);
        return (
            surface !== undefined &&
            (SURFACES[surface] as readonly Binding[]).includes(binding)
        );
    });
}
