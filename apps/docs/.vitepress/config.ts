/**
 * The documentation site: `docs/` rendered by VitePress, with the site's own
 * words — title, description, navigation — read from the string catalog
 * (`i18n/en.toml`, `[docs.site]`) like every other surface's.
 *
 * `docs/` stays the source. The site adds no page of its own and moves none,
 * so a link that works on GitHub works here and the docs link gate
 * (`.github/scripts/docs-link-gate.py`) keeps guarding both. The two things
 * GitHub does that VitePress does not are bridged below: a directory's
 * `README.md` is its index page, and a link out of `docs/` into the code goes
 * to the file on GitHub (`siteHref` in `../site.ts`).
 *
 * Localized docs are a directory per locale, `docs/<locale>/…`, mirroring the
 * English tree; each adds an entry to `locales` with its label and direction.
 * See `docs/11-adr/0054-string-catalog-pipeline.md` §The docs site.
 */

import { createMessages } from "@sunrise/i18n";
import { defineConfig } from "vitepress";
import { REPO, sidebar, siteHref } from "../site";

const en = createMessages(["en"]);

export default defineConfig({
    srcDir: "../../docs",
    outDir: "./dist",
    cacheDir: "./.vitepress/cache",
    title: en.docs.site.title(),
    description: en.docs.site.description(),
    lang: en.locale,
    cleanUrls: true,
    // GitHub renders `README.md` as a directory's page; VitePress wants
    // `index.md`. Rewriting keeps the source layout GitHub reads unchanged.
    rewrites: {
        "README.md": "index.md",
        ":dir/README.md": ":dir/index.md",
    },
    locales: {
        root: { label: "English", lang: en.locale, dir: en.dir },
    },
    // Dead links stay fatal. Every link the Markdown carries either names a
    // page of this site or is rewritten by `siteHref` to GitHub, so one that
    // is still dead is a page this site failed to build.
    markdown: {
        config(md) {
            // The docs are Markdown written for GitHub, and they use no HTML:
            // a `<id>` or `<branch>` in prose is a placeholder, not a tag.
            // VitePress passes raw HTML through to Vue's template compiler,
            // which rejects `<id>` as an unclosed element; with `html` off it
            // is text, as GitHub shows it.
            md.set({ html: false });
            // For the same reason a `{{` is text, not a Vue interpolation —
            // the docs quote GitHub Actions' `${{ … }}` often. Fenced code
            // is already `v-pre`; inline code and prose are not.
            md.renderer.rules.code_inline = (tokens, idx) =>
                `<code v-pre>${md.utils.escapeHtml(tokens[idx]?.content ?? "")}</code>`;
            const text = md.renderer.rules.text;
            md.renderer.rules.text = (tokens, idx, options, env, self) => {
                const out = text
                    ? text(tokens, idx, options, env, self)
                    : md.utils.escapeHtml(tokens[idx]?.content ?? "");
                return out
                    .replaceAll("{{", "&#123;&#123;")
                    .replaceAll("}}", "&#125;&#125;");
            };
            const linkOpen =
                md.renderer.rules.link_open ??
                ((tokens, idx, options, _env, self) =>
                    self.renderToken(tokens, idx, options));
            md.renderer.rules.link_open = (tokens, idx, options, env, self) => {
                const token = tokens[idx];
                const href = token?.attrGet("href");
                const from: unknown = env?.relativePath;
                if (token && href && typeof from === "string") {
                    const rewritten = siteHref(href, from);
                    if (rewritten !== undefined) {
                        token.attrSet("href", rewritten);
                    }
                }
                return linkOpen(tokens, idx, options, env, self);
            };
        },
    },
    themeConfig: {
        nav: [
            { text: en.docs.site.navAdrs(), link: "/11-adr/" },
            { text: en.docs.site.navRoadmap(), link: "/roadmap" },
        ],
        sidebar: sidebar(),
        search: { provider: "local" },
        socialLinks: [{ icon: "github", link: REPO }],
    },
});
