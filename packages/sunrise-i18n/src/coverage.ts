/**
 * The plural-coverage check one CI matrix entry runs for one locale.
 *
 * `docs/10-cross-cutting/i18n.md` §Plural-rule test coverage runs it for `en`,
 * `pl` and `ar`, which between them hold every CLDR category. For a locale the
 * catalog translates, the messages checked are its own. For a locale it does
 * not translate yet — `pl` and `ar` today — they are the *template* a
 * translator into it starts from: every source message reshaped to that
 * locale's categories (`reshape` in `plural.ts`). Either way the same two
 * things must hold of every message:
 *
 * 1. It compiles for the locale: it parses under the subset, and its plurals
 *    have exactly the locale's categories — none missing, none dead.
 * 2. It renders for the locale: for every category an integer can reach,
 *    formatting with that category's smallest sample count selects that
 *    category's arm, through the same runtime the web bundle ships.
 *
 * The second half is what makes the matrix more than a restatement of the
 * first. It is a round trip through `Intl.PluralRules` in the environment CI
 * runs, so a runtime whose CLDR data disagrees with the one the categories
 * were read from fails here rather than on a reader's screen.
 */

import { type Catalog, SOURCE_LOCALE } from "./catalog";
import type { Message, Part } from "./message";
import { categories, integerSample, pluralProblems, reshape } from "./plural";
import { type Args, formatParts, selectArm } from "./runtime";

/** What one locale's run found. */
export interface CoverageReport {
    readonly locale: string;
    /** `translated` when the catalog has the locale, else `template`. */
    readonly source: "translated" | "template";
    readonly messages: number;
    readonly plurals: number;
    readonly categories: readonly string[];
    readonly problems: readonly string[];
}

function pluralsOf(message: Message): Extract<Part, { kind: "plural" }>[] {
    return message.filter(
        (p): p is Extract<Part, { kind: "plural" }> => p.kind === "plural",
    );
}

function sampleArgs(message: Message, n: number): Args {
    const args: Record<string, string | number> = {};
    const visit = (parts: readonly Part[]): void => {
        for (const part of parts) {
            if (part.kind === "arg") {
                args[part.name] ??= "x";
            } else if (part.kind === "number") {
                args[part.name] = 1;
            } else if (part.kind === "plural") {
                args[part.name] = n;
                for (const arm of part.arms) {
                    visit(arm.parts);
                }
            }
        }
    };
    visit(message);
    return args;
}

/** Run the check for `locale` over `catalog`. */
export function coverage(catalog: Catalog, locale: string): CoverageReport {
    const translated = catalog.messages.get(locale);
    const source = catalog.messages.get(SOURCE_LOCALE) ?? new Map();
    const messages: [string, Message][] =
        translated !== undefined
            ? [...translated]
            : [...source].map(([k, m]): [string, Message] => [
                  k,
                  reshape(m, locale),
              ]);
    const wanted = categories(locale);
    const problems: string[] = [];
    let plurals = 0;

    for (const [key, message] of messages) {
        for (const problem of pluralProblems(locale, message)) {
            problems.push(`${key}: ${problem}`);
        }
        for (const plural of pluralsOf(message)) {
            plurals++;
            for (const category of wanted) {
                const n = integerSample(locale, category);
                if (n === undefined) {
                    continue;
                }
                const expected =
                    n === 0
                        ? (plural.arms.find((a) => a.key === "=0") ??
                          plural.arms.find((a) => a.key === category))
                        : plural.arms.find((a) => a.key === category);
                const selected = selectArm(locale, plural.arms, n);
                if (expected === undefined || selected !== expected.parts) {
                    problems.push(
                        `${key}: {${plural.name}, plural, …} with ${plural.name}=${n} (${locale} \`${category}\`) did not select its \`${category}\` arm`,
                    );
                    continue;
                }
                try {
                    formatParts(locale, message, sampleArgs(message, n));
                } catch (error) {
                    problems.push(
                        `${key}: rendering with ${plural.name}=${n} threw: ${(error as Error).message}`,
                    );
                }
            }
        }
    }

    return {
        locale,
        source: translated !== undefined ? "translated" : "template",
        messages: messages.length,
        plurals,
        categories: wanted,
        problems,
    };
}
