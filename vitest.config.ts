import { defineConfig } from "vitest/config";

export default defineConfig({
    test: {
        globals: true,
        environment: "node",
        include: ["**/*.test.ts", "**/*.spec.ts"],
        coverage: {
            provider: "v8",
            reporter: ["text", "json", "html"],
            thresholds: {
                lines: 80,
                functions: 80,
                branches: 80,
                statements: 80,
            },
            exclude: [
                "node_modules/",
                "**/dist/",
                "**/build/",
                "**/generated/",
                "**/*.d.ts",
                "**/*.config.*",
                "**/coverage/**",
                // Legacy demo/CLI code - public API is tested via index.test.ts
                "packages/gcal/src/index.ts",
            ],
        },
    },
});
