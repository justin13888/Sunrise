import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";
import { VitePWA } from "vite-plugin-pwa";

export default defineConfig({
	plugins: [
		react(),
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
				theme_color: "#0284c7",
				icons: [],
			},
		}),
	],
	server: {
		port: 5174,
		strictPort: true,
	},
});
