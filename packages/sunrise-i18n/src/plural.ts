/**
 * CLDR plural rules as the catalog's checks see them.
 *
 * The rule data is the platform's (`Intl.PluralRules`), which is CLDR's. It is
 * read here rather than tabulated so a CLDR revision reaches the checks the
 * same way it reaches the browser, Foundation and ICU4X: by upgrading the
 * runtime, not by editing a table someone has to remember exists.
 */

import {
    type ArmKey,
    CATEGORIES,
    type Category,
    type Message,
    type Part,
} from "./message";

/** The cardinal categories `locale` distinguishes, in CLDR order. */
export function categories(locale: string): Category[] {
    const found = new Intl.PluralRules(locale).resolvedOptions()
        .pluralCategories as string[];
    return CATEGORIES.filter((c) => found.includes(c));
}

/**
 * The smallest non-negative integer `locale` files under `category`, or
 * `undefined` when only fractions reach it (Polish `other`, for one).
 *
 * Plural arguments are integers in every binding, so a category no integer
 * reaches is one no rendered message can select. Its arm is still required —
 * ICU requires `other`, and a CLDR category is not the catalog's to drop —
 * but there is no count to render it with.
 */
export function integerSample(
    locale: string,
    category: Category,
): number | undefined {
    const rules = new Intl.PluralRules(locale);
    for (let n = 0; n <= 1000; n++) {
        if (rules.select(n) === category) {
            return n;
        }
    }
    for (const n of [10_000, 100_000, 1_000_000, 10_000_000]) {
        if (rules.select(n) === category) {
            return n;
        }
    }
    return undefined;
}

function plurals(message: Message): Extract<Part, { kind: "plural" }>[] {
    const out: Extract<Part, { kind: "plural" }>[] = [];
    for (const part of message) {
        if (part.kind === "plural") {
            out.push(part);
        }
    }
    return out;
}

/**
 * What is wrong with `message`'s plurals, read as a message *written in*
 * `locale`. Empty when nothing is.
 *
 * - **A missing arm.** Every category `locale` distinguishes needs its own
 *   arm. ICU would quietly render `other` in its place — the "3 file" bug in
 *   a language whose `few` nobody wrote — so here it is a build failure
 *   (`docs/10-cross-cutting/i18n.md` §Plural-rule test coverage).
 * - **A dead arm.** A category `locale` never selects is text no reader will
 *   ever see. English `zero` is the usual one; English wants `=0`.
 * - **`=0` where `zero` exists.** A String Catalog renders `=0` through its
 *   `zero` variation, so in a locale whose CLDR rules already have `zero`
 *   (Arabic, Latvian) the two would collide. Write the `zero` arm.
 */
export function pluralProblems(locale: string, message: Message): string[] {
    const problems: string[] = [];
    const wanted = categories(locale);
    for (const plural of plurals(message)) {
        const where = `{${plural.name}, plural, …}`;
        const keys = new Set<ArmKey>(plural.arms.map((a) => a.key));
        const missing = wanted.filter((c) => !keys.has(c));
        if (missing.length > 0) {
            problems.push(
                `${where} is missing ${missing.map((c) => `\`${c}\``).join(", ")} for ${locale} (${locale} distinguishes ${wanted.join(" ")})`,
            );
        }
        const dead = [...keys].filter(
            (k): k is Category => k !== "=0" && !wanted.includes(k),
        );
        if (dead.length > 0) {
            problems.push(
                `${where} has ${dead.map((c) => `\`${c}\``).join(", ")}, which ${locale} never selects (${locale} distinguishes ${wanted.join(" ")})${dead.includes("zero") ? "; use =0 for an exact zero" : ""}`,
            );
        }
        if (keys.has("=0") && wanted.includes("zero")) {
            problems.push(
                `${where} has =0, but ${locale} has a \`zero\` category; write the \`zero\` arm instead`,
            );
        }
    }
    return problems;
}

/**
 * Re-shape `message`'s plurals for `locale`: the template a translator into
 * `locale` starts from.
 *
 * Every category `locale` distinguishes gets an arm, seeded from the source's
 * `other`; an arm `locale` never selects is dropped; `=0` survives where
 * `locale` has no `zero` category and seeds `zero` where it has one. Text
 * outside the plurals is untouched — the template is the source's words in
 * the target's shape, which is exactly what the coverage matrix needs to
 * prove the pipeline can carry.
 */
export function reshape(message: Message, locale: string): Message {
    const wanted = categories(locale);
    return message.map((part): Part => {
        if (part.kind !== "plural") {
            return part;
        }
        const byKey = new Map(part.arms.map((a) => [a.key, a.parts]));
        const other = byKey.get("other") ?? [];
        const exactZero = byKey.get("=0");
        const arms: { key: ArmKey; parts: readonly Part[] }[] = [];
        if (exactZero !== undefined && !wanted.includes("zero")) {
            arms.push({ key: "=0", parts: exactZero });
        }
        for (const category of wanted) {
            const seeded =
                category === "zero" && exactZero !== undefined
                    ? exactZero
                    : (byKey.get(category) ?? other);
            arms.push({ key: category, parts: seeded });
        }
        return { ...part, arms };
    });
}
