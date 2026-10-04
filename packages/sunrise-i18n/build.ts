#!/usr/bin/env bun
/**
 * Compile `i18n/*.toml` into the four committed files under `generated/`.
 *
 * `mise run i18n` is this script. The outputs are committed for the reason the
 * design tokens' are (ADR-0029 §2): Xcode compiles `Strings.swift` and
 * `Localizable.xcstrings` with neither Bun nor mise on `PATH`, and the CLI's
 * `cargo build` should not need Bun either. `mise run i18n-check` — and
 * `test/emit.test.ts`, which fires from `pre-push` — keeps them honest. See
 * `docs/11-adr/0054-string-catalog-pipeline.md`.
 */

import { mkdir, readFile, writeFile } from "node:fs/promises";
import { argv, exit } from "node:process";
import { fileURLToPath } from "node:url";
import { type Catalog, CatalogError, loadCatalog } from "./src/catalog";
import { emitRust } from "./src/emit-rust";
import { emitSwift, emitXcstrings } from "./src/emit-swift";
import { emitTs } from "./src/emit-ts";

/** Every output, as `(filename, emitter)`. The drift test reads the same list. */
export const OUTPUTS: ReadonlyArray<
    readonly [string, (catalog: Catalog) => string]
> = [
    ["messages.ts", emitTs],
    ["Localizable.xcstrings", emitXcstrings],
    ["Strings.swift", emitSwift],
    ["strings.rs", emitRust],
];

/** Where the generated files live, relative to this file. */
export const GENERATED_DIR = new URL("./generated/", import.meta.url);

if (argv[1] !== undefined && fileURLToPath(import.meta.url) === argv[1]) {
    // A `CatalogError` lists every problem in the TOML, each naming its file
    // and key; a stack trace would bury them.
    let catalog: Catalog;
    try {
        catalog = await loadCatalog();
    } catch (error) {
        if (error instanceof CatalogError) {
            for (const problem of error.problems) {
                console.error(`i18n: ${problem}`);
            }
            exit(1);
        }
        throw error;
    }
    await mkdir(GENERATED_DIR, { recursive: true });
    for (const [name, emit] of OUTPUTS) {
        // Written only when the bytes change. `strings.rs` is `include!`d by
        // the CLI, so a rewrite bumps its mtime past the dep-info rustc left,
        // which is a rebuild for cargo and a hard failure for the log-field
        // gate (`crates/sunrise-log/tests/event_catalog.rs`) until one happens.
        const file = new URL(name, GENERATED_DIR);
        const text = emit(catalog);
        const current = await readFile(file, "utf8").catch(() => undefined);
        if (current !== text) {
            await writeFile(file, text);
        }
        console.log(`packages/sunrise-i18n/generated/${name}`);
    }
}
