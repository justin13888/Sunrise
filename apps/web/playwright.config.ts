import { defineConfig, devices } from "@playwright/test";

/**
 * The browser run of the web client (#1257): the built page, the real
 * `core.worker.ts`, the real `sunrise-core-wasm` bundle and real OPFS
 * together, in headless Chromium. `mise run web-e2e` builds the bundle first;
 * without it the specs skip (`e2e/harness.ts`).
 *
 * Specs are `e2e/*.e2e.ts`, not `*.spec.ts`: the root `vitest.config.ts`
 * collects every `*.spec.ts` in the tree and would run these under Node.
 */

/**
 * Not Vite's preview default (4173), so a preview someone left running is
 * not mistaken for this build; `--strictPort` makes a clash fail rather than
 * move.
 */
const PORT = 4179;

export default defineConfig({
    testDir: "./e2e",
    testMatch: "**/*.e2e.ts",
    // Inside `node_modules`, which is ignored everywhere, so a run leaves
    // nothing for git or Biome to see. CI uploads it when a spec fails.
    outputDir: "node_modules/.cache/playwright-results",
    forbidOnly: Boolean(process.env.CI),
    // One worker: each test already gets a fresh context (its own OPFS and
    // lock manager), and the suite is a handful of tests over one server.
    workers: 1,
    reporter: "list",
    use: {
        baseURL: `http://localhost:${PORT}`,
        // The PWA's service worker would answer a reload from its precache;
        // what is under test is the page and the core worker, not Workbox.
        serviceWorkers: "block",
        trace: "retain-on-failure",
    },
    projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
    webServer: {
        // The built app, as it ships: `vite build` copies `public/wasm/` into
        // `dist/`, and `vite preview` serves `dist/`.
        command: `bun run build && bun run preview --port ${PORT} --strictPort`,
        url: `http://localhost:${PORT}`,
        // Never reuse: on a machine with several checkouts, a server already
        // on this port may be serving another tree's build.
        reuseExistingServer: false,
        timeout: 180_000,
    },
});
