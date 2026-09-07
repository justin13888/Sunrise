/**
 * The token model: what `tokens/*.toml` is allowed to say, and the loader that
 * refuses everything else.
 *
 * Validation is most of this file on purpose. Four emitters read one model, so
 * a malformed source is not a local mistake — it is four generated files that
 * disagree with each other, three of which are compiled by a toolchain that has
 * never seen the TOML. Failing here is the only cheap place to fail.
 *
 * The TOML parser is `smol-toml` rather than `Bun.TOML`, and that is not a
 * preference: vitest runs `test/drift.test.ts` under Node — `@vitest/coverage-v8`
 * cannot run under Bun at all ("Coverage APIs are not supported") — so a
 * Bun-only loader would put the drift gate out of reach of `mise run test`.
 */

import { readFile } from "node:fs/promises";
import { parse as parseToml } from "smol-toml";
import { contrastFailures } from "./contrast";

/** A colour, as `#rrggbb`. Lowercase, six digits, no shorthand and no alpha. */
export type Hex = `#${string}`;

/** The keys of `tokens/spacing.toml`, in emit order. */
export const SPACE_KEYS = ["xs", "sm", "md", "lg", "xl", "xxl"] as const;
/** The keys of `tokens/radius.toml`, in emit order. */
export const RADIUS_KEYS = ["sm", "md", "lg", "pill"] as const;
/** The keys of `tokens/type.toml`, in emit order. */
export const TYPE_KEYS = [
    "size_xs",
    "size_sm",
    "size_base",
    "size_lg",
    "size_xl",
    "size_2xl",
    "weight_regular",
    "weight_medium",
    "weight_semibold",
    "weight_bold",
    "line_tight",
    "line_normal",
] as const;
/**
 * The named curves in `tokens/motion.toml`, in emit order.
 *
 * `reduced` is deliberately not one of them. It is the no-motion *policy* —
 * duration zero — and a curve over zero milliseconds is not observable, so a
 * required `easing` on it would be a field that can never matter and can
 * always be wrong.
 */
export const MOTION_CURVE_KEYS = ["fast", "med", "slow", "linear"] as const;
/** The `[surface]` keys both themes must carry, in emit order. */
export const SURFACE_KEYS = [
    "bg",
    "fg",
    "muted",
    "accent",
    "accent_text",
    "border",
    "danger",
    "warning",
    "success",
    "info",
] as const;

export type SpaceKey = (typeof SPACE_KEYS)[number];
export type RadiusKey = (typeof RADIUS_KEYS)[number];
export type TypeKey = (typeof TYPE_KEYS)[number];
export type MotionCurveKey = (typeof MOTION_CURVE_KEYS)[number];
export type SurfaceKey = (typeof SURFACE_KEYS)[number];

/** A duration in milliseconds plus its four cubic-Bézier control points. */
export interface MotionToken {
    readonly durationMs: number;
    readonly easing: readonly [number, number, number, number];
}

/** The named curves, plus the one policy value that is not a curve. */
export interface Motion {
    readonly curves: Readonly<Record<MotionCurveKey, MotionToken>>;
    /** What every duration collapses to under a reduced-motion preference. */
    readonly reducedDurationMs: number;
}

/** One theme: the semantic surface palette plus the per-stream tints. */
export interface Theme {
    readonly surface: Readonly<Record<SurfaceKey, Hex>>;
    readonly stream: Readonly<Record<string, Hex>>;
}

/** Everything the emitters are allowed to see. */
export interface Tokens {
    readonly space: Readonly<Record<SpaceKey, number>>;
    readonly radius: Readonly<Record<RadiusKey, number>>;
    readonly type: Readonly<Record<TypeKey, number>>;
    readonly motion: Motion;
    readonly light: Theme;
    readonly dark: Theme;
    /** `[stream]` keys, in the order both themes declare them. */
    readonly streamKeys: readonly string[];
}

/** Raised when a TOML source says something the model does not allow. */
export class TokenError extends Error {
    constructor(message: string) {
        super(message);
        this.name = "TokenError";
    }
}

const HEX = /^#[0-9a-f]{6}$/;
const STREAM_NAME = /^[a-z]+$/;

function table(source: string, value: unknown): Record<string, unknown> {
    if (typeof value !== "object" || value === null || Array.isArray(value)) {
        throw new TokenError(`${source}: expected a table`);
    }
    return value as Record<string, unknown>;
}

/**
 * Read exactly `keys` out of `raw` — no key missing, no key extra.
 *
 * The "extra" half is the load-bearing one: a typo'd key in a TOML source is
 * otherwise a token that silently never reaches any output.
 */
function exactly<K extends string>(
    source: string,
    raw: Record<string, unknown>,
    keys: readonly K[],
): Record<K, unknown> {
    const missing = keys.filter((k) => !(k in raw));
    if (missing.length > 0) {
        throw new TokenError(`${source}: missing ${missing.join(", ")}`);
    }
    const extra = Object.keys(raw).filter(
        (k) => !(keys as readonly string[]).includes(k),
    );
    if (extra.length > 0) {
        throw new TokenError(`${source}: unexpected ${extra.join(", ")}`);
    }
    return raw as Record<K, unknown>;
}

function nonNegativeInt(source: string, key: string, value: unknown): number {
    if (typeof value !== "number" || !Number.isInteger(value) || value < 0) {
        throw new TokenError(
            `${source}: ${key} must be a non-negative integer, got ${String(value)}`,
        );
    }
    return value;
}

function positiveNumber(source: string, key: string, value: unknown): number {
    if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) {
        throw new TokenError(
            `${source}: ${key} must be a positive number, got ${String(value)}`,
        );
    }
    return value;
}

function hex(source: string, key: string, value: unknown): Hex {
    if (typeof value !== "string" || !HEX.test(value)) {
        throw new TokenError(
            `${source}: ${key} must be lowercase #rrggbb, got ${String(value)}`,
        );
    }
    return value as Hex;
}

function easing(
    source: string,
    key: string,
    value: unknown,
): readonly [number, number, number, number] {
    if (!Array.isArray(value) || value.length !== 4) {
        throw new TokenError(
            `${source}: ${key}.easing must be four control points`,
        );
    }
    const points = value.map((point) => {
        if (typeof point !== "number" || !Number.isFinite(point)) {
            throw new TokenError(
                `${source}: ${key}.easing must be finite numbers`,
            );
        }
        return point;
    });
    // CSS constrains the two *abscissae* to [0, 1] and leaves the ordinates
    // free — that is what lets a curve overshoot. A `cubic-bezier()` outside
    // that range is invalid at computed-value time, which browsers resolve by
    // silently discarding the declaration, so an out-of-range control point
    // here would emit a stylesheet that animates with the wrong curve and
    // reports nothing.
    for (const [index, point] of points.entries()) {
        if (index % 2 === 0 && (point < 0 || point > 1)) {
            throw new TokenError(
                `${source}: ${key}.easing x${index / 2 + 1} must be in [0, 1], got ${point}`,
            );
        }
    }
    return [
        points[0] as number,
        points[1] as number,
        points[2] as number,
        points[3] as number,
    ];
}

function scale<K extends string>(
    source: string,
    raw: unknown,
    keys: readonly K[],
    read: (key: string, value: unknown) => number,
): Record<K, number> {
    const values = exactly(source, table(source, raw), keys);
    const out = {} as Record<K, number>;
    for (const key of keys) {
        out[key] = read(key, values[key]);
    }
    return out;
}

/** Parse `tokens/spacing.toml`. */
export function parseSpacing(raw: unknown): Record<SpaceKey, number> {
    return scale("spacing.toml", raw, SPACE_KEYS, (key, value) =>
        nonNegativeInt("spacing.toml", key, value),
    );
}

/** Parse `tokens/radius.toml`. */
export function parseRadius(raw: unknown): Record<RadiusKey, number> {
    return scale("radius.toml", raw, RADIUS_KEYS, (key, value) =>
        nonNegativeInt("radius.toml", key, value),
    );
}

/**
 * Parse `tokens/type.toml`.
 *
 * Sizes and weights are integers; the two `line_*` multipliers are not, so they
 * are checked as positive numbers instead.
 */
export function parseType(raw: unknown): Record<TypeKey, number> {
    return scale("type.toml", raw, TYPE_KEYS, (key, value) =>
        key.startsWith("line_")
            ? positiveNumber("type.toml", key, value)
            : nonNegativeInt("type.toml", key, value),
    );
}

/** Parse `tokens/motion.toml`. */
export function parseMotion(raw: unknown): Motion {
    const values = exactly("motion.toml", table("motion.toml", raw), [
        ...MOTION_CURVE_KEYS,
        "reduced",
    ] as const);

    const curves = {} as Record<MotionCurveKey, MotionToken>;
    for (const key of MOTION_CURVE_KEYS) {
        const entry = exactly(
            `motion.toml [${key}]`,
            table(`motion.toml [${key}]`, values[key]),
            ["duration_ms", "easing"] as const,
        );
        curves[key] = {
            durationMs: nonNegativeInt(
                "motion.toml",
                `${key}.duration_ms`,
                entry.duration_ms,
            ),
            easing: easing("motion.toml", key, entry.easing),
        };
    }

    // `[reduced]` carries a duration and nothing else, so `exactly` rejects an
    // `easing` written there rather than silently ignoring it.
    const policy = exactly(
        "motion.toml [reduced]",
        table("motion.toml [reduced]", values.reduced),
        ["duration_ms"] as const,
    );
    const reducedDurationMs = nonNegativeInt(
        "motion.toml",
        "reduced.duration_ms",
        policy.duration_ms,
    );
    if (reducedDurationMs !== 0) {
        throw new TokenError(
            "motion.toml: [reduced] is the no-motion policy, so duration_ms must be 0",
        );
    }

    return { curves, reducedDurationMs };
}

/**
 * Parse one `tokens/color/<theme>.toml`, contrast included.
 *
 * A palette that cannot be read is not a palette the emitters should be asked
 * to compile, so the WCAG check in `contrast.ts` runs here rather than only in
 * a test: `mise run tokens` refuses to write the generated files, and
 * `mise run tokens-check`, the `tokens-current` CI job and `drift.test.ts`
 * inherit the refusal.
 */
export function parseTheme(source: string, raw: unknown): Theme {
    const tables = exactly(source, table(source, raw), [
        "surface",
        "stream",
    ] as const);
    const surfaceRaw = exactly(
        `${source} [surface]`,
        table(`${source} [surface]`, tables.surface),
        SURFACE_KEYS,
    );
    const surface = {} as Record<SurfaceKey, Hex>;
    for (const key of SURFACE_KEYS) {
        surface[key] = hex(`${source} [surface]`, key, surfaceRaw[key]);
    }

    const streamRaw = table(`${source} [stream]`, tables.stream);
    const stream: Record<string, Hex> = {};
    for (const [key, value] of Object.entries(streamRaw)) {
        if (!STREAM_NAME.test(key)) {
            throw new TokenError(
                `${source} [stream]: ${key} is not a lowercase StreamColor variant name`,
            );
        }
        stream[key] = hex(`${source} [stream]`, key, value);
    }
    if (Object.keys(stream).length === 0) {
        throw new TokenError(
            `${source} [stream]: at least one stream tint is required`,
        );
    }

    // Last, because it is the only check that reads two colours at once: it
    // needs a whole, well-formed theme, and there is nothing useful to say
    // about the ratio between a colour and a key that is missing or malformed.
    // `contrast.ts` holds the rules and the thresholds; see ADR-0030.
    const failures = contrastFailures(source, surface, stream);
    if (failures.length > 0) {
        throw new TokenError(failures.join("\n"));
    }

    return { surface, stream };
}

/**
 * Assemble a validated model from already-parsed TOML documents.
 *
 * Pure, and separate from the loader below, so the whole validator is testable
 * without a filesystem — which is also what keeps the emitters pure.
 */
export function buildTokens(sources: {
    spacing: unknown;
    radius: unknown;
    type: unknown;
    motion: unknown;
    light: unknown;
    dark: unknown;
}): Tokens {
    const light = parseTheme("color/light.toml", sources.light);
    const dark = parseTheme("color/dark.toml", sources.dark);

    const lightKeys = Object.keys(light.stream);
    const darkKeys = Object.keys(dark.stream);
    if (
        lightKeys.length !== darkKeys.length ||
        lightKeys.some((k, i) => k !== darkKeys[i])
    ) {
        throw new TokenError(
            `color/light.toml and color/dark.toml declare different [stream] keys: ` +
                `${lightKeys.join(", ")} vs ${darkKeys.join(", ")}`,
        );
    }

    return {
        space: parseSpacing(sources.spacing),
        radius: parseRadius(sources.radius),
        type: parseType(sources.type),
        motion: parseMotion(sources.motion),
        light,
        dark,
        streamKeys: lightKeys,
    };
}

/** Where the TOML sources live, relative to this file. */
const TOKENS_DIR = new URL("../tokens/", import.meta.url);

async function readToml(relative: string): Promise<unknown> {
    return parseToml(await readFile(new URL(relative, TOKENS_DIR), "utf8"));
}

/** Read and validate `tokens/`. The only impure function in the generator. */
export async function loadTokens(): Promise<Tokens> {
    const [spacing, radius, type, motion, light, dark] = await Promise.all([
        readToml("spacing.toml"),
        readToml("radius.toml"),
        readToml("type.toml"),
        readToml("motion.toml"),
        readToml("color/light.toml"),
        readToml("color/dark.toml"),
    ]);
    return buildTokens({ spacing, radius, type, motion, light, dark });
}

/**
 * The header every generated file opens with, as bare lines.
 *
 * Each emitter wraps these in its own comment syntax. The wording mirrors
 * `crates/sunrise-server/src/bin/openapi.rs`: the committed file is an *input*
 * to a build that cannot run this generator, not a report about one.
 */
export const BANNER: readonly string[] = [
    "Generated by packages/sunrise-ui-tokens/build.ts. Do not edit.",
    "",
    "Source: packages/sunrise-ui-tokens/tokens/*.toml.",
    "Regenerate with `mise run tokens`; `mise run tokens-check` fails if this",
    "file has drifted from the TOML that produced it.",
];

/** Render a token value that is a plain number, without a trailing `.0`. */
export function num(value: number): string {
    return String(value);
}
