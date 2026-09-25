/// <reference types="vite/client" />

/**
 * The build-time variables the web client reads. Only names under `envPrefix`
 * (`SUNRISE_WEB_`, `vite.config.ts`) reach the bundle, and every one of them is
 * public: anything here ships to every browser that loads the app.
 */
interface ImportMetaEnv {
    /**
     * The relay a browser uses until its user chooses one; see
     * `server-url.ts`. An `https:` origin with no path (`http:` only on
     * loopback), or empty for none. Set it in the build environment — the
     * `web-pages` job in `.github/workflows/release.yml` reads it from the
     * repository variable of the same name — or in an untracked `.env.local`.
     */
    readonly SUNRISE_WEB_DEFAULT_SERVER_URL?: string;
}

interface ImportMeta {
    readonly env: ImportMetaEnv;
}
