/**
 * The message model: the subset of ICU MessageFormat the catalog may use, and
 * the parser that refuses everything outside it.
 *
 * Parsing is delegated to `@formatjs/icu-messageformat-parser`, which owns the
 * grammar (apostrophe quoting, nesting, whitespace) so this package never
 * grows a second, subtly different reading of it. What this file owns is the
 * *subset*. Three emitters read one model and one of them — Apple's String
 * Catalog — can express much less than ICU can, so every construct here is one
 * that all three render identically:
 *
 * - literal text;
 * - `{name}` — a string argument;
 * - `{name, number}` — an integer argument, formatted with the locale's
 *   digits and grouping;
 * - `{name, plural, …}` — CLDR cardinal plural selection over an integer, with
 *   the category arms (`zero one two few many other`), an optional `=0` arm,
 *   and `#` for the formatted count inside an arm.
 *
 * Refused, each with a reason the author reads: `select`, `selectordinal`,
 * `date`, `time`, number styles and skeletons, `offset:`, exact arms other
 * than `=0`, and a plural nested inside a plural arm. Every one of those is a
 * real ICU feature; each is refused because a String Catalog has no
 * construct that means the same thing, and a message that renders one way on
 * the web and another on macOS is worse than one that cannot be written. See
 * `docs/10-cross-cutting/i18n.md` §Message subset.
 */

import {
    type MessageFormatElement,
    parse,
    TYPE,
} from "@formatjs/icu-messageformat-parser";

/** The six CLDR plural categories, in CLDR's canonical order. */
export const CATEGORIES = [
    "zero",
    "one",
    "two",
    "few",
    "many",
    "other",
] as const;

/** One CLDR plural category. */
export type Category = (typeof CATEGORIES)[number];

/** The key of one plural arm: a CLDR category, or the exact match `=0`. */
export type ArmKey = Category | "=0";

/** One piece of a message. */
export type Part =
    | { readonly kind: "text"; readonly value: string }
    | { readonly kind: "arg"; readonly name: string }
    | { readonly kind: "number"; readonly name: string }
    | { readonly kind: "pound" }
    | {
          readonly kind: "plural";
          readonly name: string;
          readonly arms: ReadonlyArray<{
              readonly key: ArmKey;
              readonly parts: readonly Part[];
          }>;
      };

/** A parsed message: its parts, in order. */
export type Message = readonly Part[];

/**
 * What an argument is, which decides its type in every binding: a `string` is
 * `String` / `&str` / `string`, an `integer` is `Int` / `i64` / `number`.
 */
export type ArgKind = "string" | "integer";

/** A message the subset refuses, with the reason written for its author. */
export class MessageError extends Error {
    override name = "MessageError";
}

/** Whether `key` names a CLDR plural category. */
export function isCategory(key: string): key is Category {
    return (CATEGORIES as readonly string[]).includes(key);
}

/**
 * Parse one catalog value into the subset.
 *
 * `ignoreTag` is on: `<` has no meaning in the subset, so a literal `<` in a
 * message is text rather than the start of a rich-text tag nothing renders.
 */
export function parseMessage(source: string): Message {
    let elements: MessageFormatElement[];
    try {
        elements = parse(source, {
            ignoreTag: true,
            requiresOtherClause: true,
        });
    } catch (error) {
        throw new MessageError(
            `not valid ICU MessageFormat: ${(error as Error).message}`,
        );
    }
    return merge(convert(elements, false));
}

function convert(
    elements: readonly MessageFormatElement[],
    inPlural: boolean,
): Part[] {
    const out: Part[] = [];
    for (const el of elements) {
        switch (el.type) {
            case TYPE.literal:
                out.push({ kind: "text", value: el.value });
                break;
            case TYPE.argument:
                out.push({ kind: "arg", name: argName(el.value) });
                break;
            case TYPE.number:
                if (el.style !== undefined && el.style !== null) {
                    throw new MessageError(
                        `{${el.value}, number, …} carries a style; only a bare {${el.value}, number} is supported`,
                    );
                }
                out.push({ kind: "number", name: argName(el.value) });
                break;
            case TYPE.pound:
                // formatjs only emits `#` as a pound element inside a plural
                // arm; outside one it is literal text.
                out.push({ kind: "pound" });
                break;
            case TYPE.plural: {
                if (el.pluralType === "ordinal") {
                    throw new MessageError(
                        `{${el.value}, selectordinal, …} is not supported: a String Catalog has no ordinal variation`,
                    );
                }
                if (inPlural) {
                    throw new MessageError(
                        `{${el.value}, plural, …} is nested inside another plural arm; split the message instead`,
                    );
                }
                if (el.offset !== 0) {
                    throw new MessageError(
                        `{${el.value}, plural, offset:${el.offset} …} is not supported`,
                    );
                }
                const arms: { key: ArmKey; parts: Part[] }[] = [];
                for (const [key, option] of Object.entries(el.options)) {
                    if (key !== "=0" && !isCategory(key)) {
                        throw new MessageError(
                            `{${el.value}, plural, …} has an arm \`${key}\`; arms are CLDR categories (${CATEGORIES.join(" ")}) or =0`,
                        );
                    }
                    arms.push({
                        key,
                        parts: merge(convert(option.value, true)),
                    });
                }
                arms.sort((a, b) => armOrder(a.key) - armOrder(b.key));
                out.push({ kind: "plural", name: argName(el.value), arms });
                break;
            }
            case TYPE.select:
                throw new MessageError(
                    `{${el.value}, select, …} is not supported: a String Catalog has no select variation; use one key per case`,
                );
            case TYPE.date:
            case TYPE.time:
                throw new MessageError(
                    `{${el.value}, ${el.type === TYPE.date ? "date" : "time"}} is not supported: format the value with the platform's date API and pass it as a string`,
                );
            default:
                throw new MessageError(
                    `unsupported element type ${String((el as { type: unknown }).type)}`,
                );
        }
    }
    return out;
}

/** `=0` sorts first, then the categories in CLDR order. */
function armOrder(key: ArmKey): number {
    return key === "=0" ? -1 : CATEGORIES.indexOf(key);
}

/**
 * Argument names become identifiers in Swift, Rust and TypeScript, so they are
 * held to the one shape all three accept without escaping.
 */
function argName(name: string): string {
    if (
        !/^[a-z][a-z0-9]*(?:_[a-z][a-z0-9]*)*$/.test(name) ||
        ["self", "super", "crate"].includes(name)
    ) {
        throw new MessageError(
            `argument \`${name}\` must be snake_case, each \`_\` followed by a letter, and not self, super or crate`,
        );
    }
    return name;
}

/** Join adjacent text parts, which formatjs can split around quotes. */
function merge(parts: Part[]): Part[] {
    const out: Part[] = [];
    for (const part of parts) {
        const last = out.at(-1);
        if (part.kind === "text" && last?.kind === "text") {
            out[out.length - 1] = {
                kind: "text",
                value: last.value + part.value,
            };
        } else {
            out.push(part);
        }
    }
    return out;
}

/**
 * Every argument a message takes, with its kind, in first-use order.
 *
 * An argument used as a plural selector or with `number` anywhere is an
 * `integer`, even where it also appears bare as `{name}`; one used only bare
 * is a `string`.
 */
export function argumentsOf(message: Message): Map<string, ArgKind> {
    const args = new Map<string, ArgKind>();
    const visit = (parts: readonly Part[]): void => {
        for (const part of parts) {
            switch (part.kind) {
                case "arg":
                    if (!args.has(part.name)) {
                        args.set(part.name, "string");
                    }
                    break;
                case "number":
                    args.set(part.name, "integer");
                    break;
                case "plural":
                    args.set(part.name, "integer");
                    for (const arm of part.arms) {
                        visit(arm.parts);
                    }
                    break;
                default:
                    break;
            }
        }
    };
    visit(message);
    return args;
}
