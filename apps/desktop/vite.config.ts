import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Tauri dev expects the renderer on a stable port; default to 5173.
export default defineConfig({
	plugins: [react()],
	clearScreen: false,
	server: {
		port: 5173,
		strictPort: true,
	},
	envPrefix: ["VITE_", "TAURI_"],
});
