#!/usr/bin/env bun
/**
 * `bun run packages/sunrise-i18n/plurals.ts <locale>…` — the plural-coverage
 * check, one CI matrix entry per locale (`mise run i18n-plurals <locale>`).
 *
 * With no locale named it runs the three `docs/10-cross-cutting/i18n.md`
 * names, which between them hold every CLDR plural category.
 *
 * Exit 0 when every message compiles and renders for every locale named, 1
 * when one does not, 2 when this runtime has no CLDR rules for a locale. See
 * `src/coverage.ts` for what "compiles and renders" means.
 */

import { argv, exit } from "node:process";
import { CatalogError, loadCatalog } from "./src/catalog";
import { coverage } from "./src/coverage";

/** The CI matrix: `one/other`, `one/few/many/other`, and all six. */
export const MATRIX = ["en", "pl", "ar"] as const;

const named = argv.slice(2);
const locales: readonly string[] = named.length > 0 ? named : MATRIX;

let catalog: Awaited<ReturnType<typeof loadCatalog>>;
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

let failed = false;
for (const locale of locales) {
    let supported: string[];
    try {
        supported = Intl.PluralRules.supportedLocalesOf(locale);
    } catch {
        supported = [];
    }
    if (supported.length === 0) {
        console.error(
            `${locale}: this runtime has no CLDR plural rules for it`,
        );
        exit(2);
    }
    const report = coverage(catalog, locale);
    for (const problem of report.problems) {
        console.error(`${locale}: ${problem}`);
    }
    failed ||= report.problems.length > 0;
    console.log(
        `${locale}: ${report.problems.length === 0 ? "ok" : "FAILED"} — ${report.messages} messages (${report.source}), ${report.plurals} plurals, categories ${report.categories.join(" ")}`,
    );
}
exit(failed ? 1 : 0);
