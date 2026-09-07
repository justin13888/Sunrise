/**
 * WCAG 2.1 contrast, and the gate that refuses a palette below it.
 *
 * `docs/10-cross-cutting/accessibility.md` §Color and contrast asks for
 * 4.5:1 on text and on interactive elements, and prefers 7:1 for body text.
 * Until this file existed that was a sentence: `test/invariants.test.ts`
 * checked three pairs it had picked by hand, and the seven colours that fell
 * short — `warning` 3.08, `info` 3.56, `success` 3.64, and the `amber`,
 * `emerald`, `sky` and `pink` tints — were named in ADR-0029's Consequences
 * and asserted by nothing. See ADR-0030.
 *
 * So the rule table below is the gate, and it is *exhaustive*: every key a
 * theme declares must either appear in it or be listed as exempt with a
 * reason, and a key that is neither is itself a failure. That is what stops
 * the next token added to `[surface]` from slipping in unmeasured, which is
 * how the seven above got in.
 *
 * `model.ts` calls this from `parseTheme`, which puts it on the generator
 * rather than beside it: `mise run tokens` refuses to emit a palette that
 * fails, so `mise run tokens-check`, the `tokens-current` CI job and
 * `test/drift.test.ts` all inherit the check without knowing about it.
 *
 * Nothing here imports `model.ts` — the dependency runs one way, and the
 * checks are written against the parsed records rather than against
 * `SURFACE_KEYS`, so "every key is covered" is a statement about the data.
 */

/** AA for text and for interactive elements: `accessibility.md` §Color. */
export const AA_TEXT = 4.5;
/** AAA, which the same doc prefers for body text. */
export const AAA_TEXT = 7;

/** One asserted pair: `foreground` drawn on `background`, at `min` or better. */
export interface ContrastRule {
    /** The `[surface]` key drawn in front. */
    readonly foreground: string;
    /** The `[surface]` key it is drawn on. */
    readonly background: string;
    /** The ratio it must reach, from `accessibility.md`. */
    readonly min: number;
    /** What it is doing, which is what picks `min`. Quoted in the failure. */
    readonly role: string;
}

/**
 * Every `[surface]` pair that is drawn one on the other, and its threshold.
 *
 * `bg` is the background rather than a foreground, so it is not a rule; every
 * other key is here or in `EXEMPT_SURFACE_KEYS`.
 *
 * `accent` earns 4.5 rather than 3 because it is the interactive foreground —
 * a link, a control's label — which the doc holds to AA against its
 * background. `danger`/`warning`/`success`/`info` earn it because they are
 * status text: a state this product colours also carries a glyph or a label
 * (`accessibility.md` forbids colour as the only signal), and that label is
 * drawn in the status colour.
 */
export const SURFACE_CONTRAST_RULES: readonly ContrastRule[] = [
    { foreground: "fg", background: "bg", min: AAA_TEXT, role: "body text" },
    {
        foreground: "muted",
        background: "bg",
        min: AA_TEXT,
        role: "secondary text",
    },
    {
        foreground: "accent",
        background: "bg",
        min: AA_TEXT,
        role: "the interactive foreground",
    },
    {
        foreground: "accent_text",
        background: "accent",
        min: AA_TEXT,
        role: "text on an accent fill",
    },
    {
        foreground: "danger",
        background: "bg",
        min: AA_TEXT,
        role: "status text",
    },
    {
        foreground: "warning",
        background: "bg",
        min: AA_TEXT,
        role: "status text",
    },
    {
        foreground: "success",
        background: "bg",
        min: AA_TEXT,
        role: "status text",
    },
    { foreground: "info", background: "bg", min: AA_TEXT, role: "status text" },
];

/** A key with no rule, and the reason it needs none. */
export interface ContrastExemption {
    readonly key: string;
    readonly why: string;
}

/**
 * The `[surface]` keys no rule covers, each with why.
 *
 * This list is short on purpose. It is the only way a colour avoids being
 * measured, so an entry is a claim that has to hold — and `border`'s does not
 * hold unconditionally, which is why its numbers are written down here rather
 * than left to be rediscovered.
 */
export const EXEMPT_SURFACE_KEYS: readonly ContrastExemption[] = [
    {
        key: "border",
        why:
            "a hairline separator, which WCAG 2.1 1.4.11 exempts as decoration. " +
            "It is 1.21:1 on the light background and 1.29:1 on the dark one, so " +
            "the moment it draws the boundary that identifies a control it owes " +
            "1.4.11's 3:1 and needs a rule here — which is a palette change, not " +
            "a rule change. Nothing in `apps/` reads it today",
    },
];

/**
 * What a `[stream]` tint must clear against the theme background.
 *
 * The tints are held to text contrast, not to 1.4.11's 3:1 for a graphical
 * object, and ADR-0030 records why: two of them are the same hex as
 * `warning` and `success`, which are status *text*, so a 3:1 tint set would
 * put two near-identical ambers in one palette; and holding all eight to
 * 4.5 is what lets a tint become a stream's label colour — the use
 * `shared-ui-system.md` never ruled out — without an audit first.
 */
export const STREAM_CONTRAST_RULE = {
    background: "bg",
    min: AA_TEXT,
    role: "a stream label colour",
} as const;

/** WCAG 2.1 relative luminance of an `#rrggbb` colour. */
export function luminance(hex: string): number {
    const channel = (at: number) => {
        const value = Number.parseInt(hex.slice(at, at + 2), 16) / 255;
        return value <= 0.03928
            ? value / 12.92
            : ((value + 0.055) / 1.055) ** 2.4;
    };
    return 0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5);
}

/** WCAG 2.1 contrast ratio between two `#rrggbb` colours, in `[1, 21]`. */
export function contrastRatio(a: string, b: string): number {
    const [lighter, darker] = [luminance(a), luminance(b)].sort(
        (x, y) => y - x,
    ) as [number, number];
    return (lighter + 0.05) / (darker + 0.05);
}

/**
 * Two decimals, rounded *down*.
 *
 * Rounding to nearest would print "4.50:1, below the 4.5:1 required" for
 * anything in `[4.495, 4.5)` — a failure that reads as a rounding bug. Down
 * cannot: the number shown is never larger than the number compared.
 */
function ratio(value: number): string {
    return (Math.floor(value * 100) / 100).toFixed(2);
}

/**
 * Every way `surface` and `stream` fall short, as messages, newest concern
 * first: uncovered keys, then the `[surface]` rules, then the tints.
 *
 * Returns rather than throws so the caller reports all of them at once. A
 * palette edit that darkens the background breaks several pairs together, and
 * fixing them one exception at a time is the slow way to find that out.
 */
export function contrastFailures(
    source: string,
    surface: Readonly<Record<string, string>>,
    stream: Readonly<Record<string, string>>,
): string[] {
    const failures: string[] = [];

    const covered = new Set<string>([
        "bg",
        ...SURFACE_CONTRAST_RULES.map((rule) => rule.foreground),
        ...EXEMPT_SURFACE_KEYS.map((exemption) => exemption.key),
    ]);
    for (const key of Object.keys(surface)) {
        if (!covered.has(key)) {
            failures.push(
                `${source} [surface]: ${key} has no contrast rule. Add one to ` +
                    `SURFACE_CONTRAST_RULES in src/contrast.ts, or list it in ` +
                    `EXEMPT_SURFACE_KEYS with why it needs none.`,
            );
        }
    }

    const background = surface.bg;
    for (const rule of SURFACE_CONTRAST_RULES) {
        const fg = surface[rule.foreground];
        const bg = surface[rule.background];
        if (fg === undefined || bg === undefined) {
            continue; // A missing key is `exactly`'s failure to report, not this one's.
        }
        const got = contrastRatio(fg, bg);
        if (got < rule.min) {
            failures.push(
                `${source} [surface]: ${rule.foreground} on ${rule.background} ` +
                    `is ${ratio(got)}:1, below the ${rule.min}:1 ` +
                    `docs/10-cross-cutting/accessibility.md §Color and contrast ` +
                    `requires of ${rule.role}.`,
            );
        }
    }

    if (background !== undefined) {
        for (const [key, tint] of Object.entries(stream)) {
            const got = contrastRatio(tint, background);
            if (got < STREAM_CONTRAST_RULE.min) {
                failures.push(
                    `${source} [stream]: ${key} on ${STREAM_CONTRAST_RULE.background} ` +
                        `is ${ratio(got)}:1, below the ${STREAM_CONTRAST_RULE.min}:1 ` +
                        `docs/10-cross-cutting/accessibility.md §Color and contrast ` +
                        `requires of ${STREAM_CONTRAST_RULE.role}.`,
                );
            }
        }
    }

    return failures;
}
