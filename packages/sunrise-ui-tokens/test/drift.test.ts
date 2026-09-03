/**
 * The drift gate.
 *
 * `generated/` is committed because the two build environments that need it
 * most cannot produce it: Xcode compiles `tokens.swift` with neither Bun nor
 * mise on `PATH`. That makes each file an *input* to somebody else's build
 * rather than a report about this one, and a stale input compiles perfectly
 * while being wrong.
 *
 * This is the TypeScript analogue of `the_committed_description_is_current` in
 * `crates/sunrise-server/src/api/mod.rs`, and — unlike the CI job, which is
 * dormant while Actions billing is blocked — it runs from `pre-push` through
 * `lefthook.yaml`.
 */

import { readFile } from "node:fs/promises";
import { describe, expect, it } from "vitest";
import { GENERATED_DIR, OUTPUTS } from "../build";
import { loadTokens } from "../src/model";

const tokens = await loadTokens();

describe("the committed token files are current", () => {
    for (const [name, emit] of OUTPUTS) {
        it(`generated/${name}`, async () => {
            const committed = await readFile(
                new URL(name, GENERATED_DIR),
                "utf8",
            );
            expect(
                committed,
                `generated/${name} is stale; regenerate it with \`mise run tokens\``,
            ).toBe(emit(tokens));
        });
    }

    it("emits every file the package advertises", () => {
        expect(OUTPUTS.map(([name]) => name)).toEqual([
            "tokens.css",
            "tokens.ts",
            "tokens.swift",
            "tokens.rs",
        ]);
    });

    it("ends every file with exactly one trailing newline", () => {
        for (const [name, emit] of OUTPUTS) {
            const rendered = emit(tokens);
            expect(
                rendered.endsWith("\n"),
                `${name} must end with a newline`,
            ).toBe(true);
            expect(
                rendered.endsWith("\n\n"),
                `${name} must not end with a blank line`,
            ).toBe(false);
        }
    });

    it("names `mise run tokens` in every banner, so an editor is told what to run", () => {
        for (const [name, emit] of OUTPUTS) {
            expect(emit(tokens), name).toContain("mise run tokens");
            expect(emit(tokens), name).toContain("Do not edit");
        }
    });
});
