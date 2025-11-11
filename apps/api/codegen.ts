import type { CodegenConfig } from '@graphql-codegen/cli'

const config: CodegenConfig = {
    schema: './src/schema.graphql',
    documents: [
        './src/queries/**/*.graphql',
        './src/mutations/**/*.graphql',
        './src/subscriptions/**/*.graphql',
        '../app/src/graphql/**/*.graphql'
    ],
    generates: {
        // Server-side resolver types
        './src/generated/resolvers-types.ts': {
            plugins: [
                'typescript',
                'typescript-resolvers'
            ],
            config: {
                useIndexSignature: true,
                contextType: './context#GraphQLContext',
                mappers: {
                    CalendarEvent: '../types/calendar#CalendarEvent',
                    Calendar: '../types/calendar#Calendar',
                    User: '../types/user#User'
                }
            }
        },
        // Client-side types and React hooks for the frontend app
        '../app/src/generated/graphql.ts': {
            plugins: [
                'typescript',
                'typescript-operations',
                'typescript-react-apollo'
            ],
            config: {
                withHooks: true,
                withComponent: false,
                withHOC: false
            }
        }
    },
    ignoreNoDocuments: true
}

export default config
