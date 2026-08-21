import type { CodegenConfig } from "@graphql-codegen/cli";

const config: CodegenConfig = {
    schema: "./src/schema.graphql",
    generates: {
        // Server-side resolver types
        "./src/generated/resolvers-types.ts": {
            plugins: ["typescript", "typescript-resolvers"],
            config: {
                useIndexSignature: true,
                useTypeImports: true,
                typesPrefix: "Gql",
                contextType: "../context#GraphQLContext",
                mappers: {
                    Calendar: "../types/calendar#Calendar",
                    User: "../types/user#User",
                },
            },
        },
    },
};

export default config;
