import { color } from "@sunrise/ui-tokens";
import react from "@vitejs/plugin-react";
import { defineConfig, type Plugin } from "vite";
import { VitePWA } from "vite-plugin-pwa";

/**
 * The colour browser chrome takes — the address bar on Android, the PWA
 * splash, the task-switcher card.
 *
 * `surface.accent` rather than `stream.sky`. `stream.*` is the eight-entry
 * palette a user picks a Stream's colour from (`streamColors` in
 * `@sunrise/ui-tokens`); one of its entries standing in for the product's
 * identity is a coincidence, not a decision, and it broke as one — #124 moved
 * `light.stream.sky` from `#0284c7` to `#0369a1` for a contrast rule that
 * `theme-color` is not even subject to, and the two copies of the old hex here
 * and in `index.html` named a colour that then existed nowhere in the palette
 * (#128). `surface.accent` is the semantic accent of the light surface, which
 * is what chrome sitting beside that surface should match.
 *
 * The light theme unconditionally: `theme-color` is one value, and a
 * `media`-split pair would be a second decision to keep in step with no gate
 * over it.
 */
const themeColor = color.light.surface.accent;

/**
 * Write `<meta name="theme-color">` into the HTML from the token above.
 *
 * The tag used to be a literal in `index.html`, which put it outside the token
 * pipeline where neither the drift test nor the contrast gate could see it.
 * Injecting it here leaves one source for both the tag and the PWA manifest.
 */
function themeColorMeta(): Plugin {
    return {
        name: "sunrise-theme-color",
        transformIndexHtml() {
            return [
                {
                    tag: "meta",
                    attrs: { name: "theme-color", content: themeColor },
                    injectTo: "head",
                },
            ];
        },
    };
}

export default defineConfig({
    plugins: [
        react(),
        themeColorMeta(),
        VitePWA({
            registerType: "autoUpdate",
            workbox: {
                navigateFallback: "/index.html",
                globPatterns: ["**/*.{js,css,html,svg,wasm}"],
            },
            manifest: {
                name: "Sunrise",
                short_name: "Sunrise",
                description: "Local-first, end-to-end encrypted productivity",
                theme_color: themeColor,
                icons: [],
            },
        }),
    ],
    server: {
        port: 5174,
        strictPort: true,
    },
});
