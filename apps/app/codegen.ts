import type { CodegenConfig } from "@graphql-codegen/cli";

const config: CodegenConfig = {
    schema: "../api/src/schema.graphql",
    documents: ["src/graphql/**/*.graphql"],
    generates: {
        "./src/generated/graphql.ts": {
            plugins: [
                "typescript",
                "typescript-operations",
                "typescript-react-apollo",
            ],
            config: {
                withHooks: true,
                scalars: {
                    DateTime: "string",
                    URL: "string",
                },
            },
        },
    },
};

export default config;
