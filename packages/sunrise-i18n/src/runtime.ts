/**
 * The TypeScript formatter the generated `messages.ts` runs on.
 *
 * It interprets the subset `message.ts` admits and nothing else, using the
 * platform's own CLDR data through `Intl.PluralRules` and `Intl.NumberFormat`
 * — the same data Foundation formats the Apple String Catalog with and ICU4X
 * gives the CLI, so the three bindings agree on which arm a count selects.
 *
 * This file has no imports on purpose: it ships in the web bundle, while the
 * parser that produced its input stays a build-time dependency.
 */

/** One piece of a compiled message. Mirrors `Part` in `message.ts`. */
export type Part =
    | { readonly kind: "text"; readonly value: string }
    | { readonly kind: "arg"; readonly name: string }
    | { readonly kind: "number"; readonly name: string }
    | { readonly kind: "pound" }
    | {
          readonly kind: "plural";
          readonly name: string;
          readonly arms: ReadonlyArray<{
              readonly key: string;
              readonly parts: readonly Part[];
          }>;
      };

/** A message's arguments by name. */
export type Args = Readonly<Record<string, string | number>>;

/** Every locale's messages: `locale → key → parts`. */
export type Catalog = Readonly<
    Record<string, Readonly<Record<string, readonly Part[]>>>
>;

const pluralRules = new Map<string, Intl.PluralRules>();
const numberFormats = new Map<string, Intl.NumberFormat>();

function rulesFor(locale: string): Intl.PluralRules {
    let rules = pluralRules.get(locale);
    if (rules === undefined) {
        rules = new Intl.PluralRules(locale);
        pluralRules.set(locale, rules);
    }
    return rules;
}

function numberFormatFor(locale: string): Intl.NumberFormat {
    let format = numberFormats.get(locale);
    if (format === undefined) {
        format = new Intl.NumberFormat(locale, { maximumFractionDigits: 0 });
        numberFormats.set(locale, format);
    }
    return format;
}

function integer(name: string, args: Args): number {
    const value = args[name];
    if (typeof value !== "number" || !Number.isInteger(value)) {
        throw new TypeError(`argument \`${name}\` must be an integer`);
    }
    return value;
}

/**
 * Render `parts` in `locale`.
 *
 * `locale` is the locale the message was *written* in, not the one the user
 * asked for: an English fallback shown to a Polish reader selects its arms by
 * English rules, because English arms are the only ones it has.
 */
export function formatParts(
    locale: string,
    parts: readonly Part[],
    args: Args,
    count?: number,
): string {
    let out = "";
    for (const part of parts) {
        switch (part.kind) {
            case "text":
                out += part.value;
                break;
            case "arg": {
                const value = args[part.name];
                if (value === undefined) {
                    throw new TypeError(`argument \`${part.name}\` is missing`);
                }
                out +=
                    typeof value === "number"
                        ? numberFormatFor(locale).format(value)
                        : value;
                break;
            }
            case "number":
                out += numberFormatFor(locale).format(integer(part.name, args));
                break;
            case "pound":
                if (count === undefined) {
                    throw new TypeError("`#` outside a plural arm");
                }
                out += numberFormatFor(locale).format(count);
                break;
            case "plural": {
                const n = integer(part.name, args);
                out += formatParts(
                    locale,
                    selectArm(locale, part.arms, n),
                    args,
                    n,
                );
                break;
            }
        }
    }
    return out;
}

/**
 * The arm `n` selects: `=0` when it matches, else the CLDR category, else
 * `other` — which the parser guarantees exists.
 */
export function selectArm(
    locale: string,
    arms: ReadonlyArray<{
        readonly key: string;
        readonly parts: readonly Part[];
    }>,
    n: number,
): readonly Part[] {
    const byKey = new Map(arms.map((arm) => [arm.key, arm.parts]));
    const exact = n === 0 ? byKey.get("=0") : undefined;
    if (exact !== undefined) {
        return exact;
    }
    const category = rulesFor(locale).select(n);
    const parts = byKey.get(category) ?? byKey.get("other");
    if (parts === undefined) {
        throw new TypeError("plural without an `other` arm");
    }
    return parts;
}

/**
 * The catalog locale to serve for a list of requested locales, in the
 * requester's preference order (`navigator.languages`, say).
 *
 * An exact tag wins, then the requested tag's language subtag, then the
 * catalog's source locale. Matching is case-insensitive, as BCP 47 is.
 */
export function negotiate(
    available: readonly string[],
    requested: readonly string[],
    source: string,
): string {
    const lower = new Map(available.map((tag) => [tag.toLowerCase(), tag]));
    for (const tag of requested) {
        const exact = lower.get(tag.toLowerCase());
        if (exact !== undefined) {
            return exact;
        }
        const language = lower.get(tag.split("-")[0]?.toLowerCase() ?? "");
        if (language !== undefined) {
            return language;
        }
    }
    return source;
}

/**
 * The languages written right to left, by primary language subtag.
 *
 * A list rather than `Intl.Locale#getTextInfo()`, which Firefox does not
 * implement; the list covers every right-to-left language with a CLDR
 * locale that has more than a handful of speakers.
 */
const RTL_LANGUAGES = new Set([
    "ar",
    "arc",
    "ckb",
    "dv",
    "fa",
    "he",
    "ks",
    "ku",
    "ps",
    "sd",
    "ug",
    "ur",
    "yi",
]);

/** The base writing direction of `locale`, for `<html dir>`. */
export function direction(locale: string): "ltr" | "rtl" {
    const language = locale.split("-")[0]?.toLowerCase() ?? "";
    return RTL_LANGUAGES.has(language) ? "rtl" : "ltr";
}

/**
 * A formatter over `catalog` for one requested locale.
 *
 * A key the requested locale has not translated falls back to the source
 * locale's message, formatted by the source locale's rules (see
 * `formatParts`). A key absent from both is a generator bug, since the
 * generated accessors only name keys the source catalog holds.
 */
export function createFormatter(
    catalog: Catalog,
    source: string,
    locale: string,
): (key: string, args?: Args) => string {
    return (key, args = {}) => {
        const translated = catalog[locale]?.[key];
        if (translated !== undefined) {
            return formatParts(locale, translated, args);
        }
        const fallback = catalog[source]?.[key];
        if (fallback === undefined) {
            throw new RangeError(`no message \`${key}\``);
        }
        return formatParts(source, fallback, args);
    };
}
