import { defineConfig } from "vitest/config";

export default defineConfig({
    test: {
        globals: true,
        environment: "node",
        include: ["**/*.test.ts", "**/*.spec.ts"],
        // crates/ is Rust; node_modules is third-party. .claude/ holds
        // transient agent worktrees (full repo copies).
        exclude: [
            "**/node_modules/**",
            "**/dist/**",
            "**/build/**",
            "**/coverage/**",
            "crates/**",
            "target/**",
            ".claude/**",
        ],
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
                "crates/**",
                "target/**",
                ".claude/**",
            ],
        },
    },
});
