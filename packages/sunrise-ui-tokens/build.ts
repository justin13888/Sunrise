#!/usr/bin/env bun
/**
 * Compile `tokens/*.toml` into the four committed files under `generated/`.
 *
 * `mise run tokens` is this script. The outputs are committed rather than built
 * on demand because the consumers that need them most cannot run it: Xcode's
 * build environment has neither Bun nor mise on `PATH`. `mise run tokens-check`
 * — and `test/drift.test.ts`, which fires from `pre-push` — is what keeps the
 * committed files honest. See `docs/11-adr/0029-design-token-pipeline.md`.
 */

import { emitCss } from "./src/emit-css";
import { emitRust } from "./src/emit-rust";
import { emitSwift } from "./src/emit-swift";
import { emitTs } from "./src/emit-ts";
import { loadTokens, type Tokens } from "./src/model";

/** Every output, as `(filename, emitter)`. The drift test reads the same list. */
export const OUTPUTS: ReadonlyArray<
    readonly [string, (tokens: Tokens) => string]
> = [
    ["tokens.css", emitCss],
    ["tokens.ts", emitTs],
    ["tokens.swift", emitSwift],
    ["tokens.rs", emitRust],
];

/** Where the generated files live, relative to this file. */
export const GENERATED_DIR = new URL("./generated/", import.meta.url);

if (import.meta.main) {
    const tokens = await loadTokens();
    for (const [name, emit] of OUTPUTS) {
        await Bun.write(new URL(name, GENERATED_DIR), emit(tokens));
        console.log(`packages/sunrise-ui-tokens/generated/${name}`);
    }
}
