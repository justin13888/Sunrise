import { defineConfig } from "vitest/config";

export default defineConfig({
    test: {
        globals: true,
        environment: "node",
        include: ["**/*.test.ts", "**/*.spec.ts"],
        // legacy/ holds the v0 prototype; preserved for reference, not built or
        // tested. crates/ is Rust; node_modules is third-party. See
        // spec/README.md for context on why apps/{api,app}, packages/{gcal,models}
        // were moved under legacy/.
        exclude: [
            "**/node_modules/**",
            "**/dist/**",
            "**/build/**",
            "**/coverage/**",
            "legacy/**",
            "crates/**",
            "target/**",
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
                "legacy/**",
                "crates/**",
                "target/**",
            ],
        },
    },
});
