import { ApolloClient, InMemoryCache, createHttpLink, from, split } from '@apollo/client'
import { setContext } from '@apollo/client/link/context'
import { onError } from '@apollo/client/link/error'
import { GraphQLWsLink } from '@apollo/client/link/subscriptions'
import { getMainDefinition } from '@apollo/client/utilities'
import { createClient } from 'graphql-ws'

// TODO: Check this

const httpLink = createHttpLink({
    uri: 'http://localhost:3000/graphql', // TODO: This hard-code is wrong. should be done at compile time.
})

const authLink = setContext((_, { headers }) => {
    // Get the authentication token from local storage if it exists
    const token = localStorage.getItem('access_token')
    const refreshToken = localStorage.getItem('refresh_token')
    const userId = localStorage.getItem('user_id')

    // Return the headers to the context so httpLink can read them
    return {
        headers: {
            ...headers,
            authorization: token ? `Bearer ${token}` : '',
            'x-refresh-token': refreshToken || '',
            'x-user-id': userId || '',
        }
    }
})

const errorLink = onError(({ graphQLErrors, networkError }) => {
    if (graphQLErrors) {
        graphQLErrors.forEach(({ message, locations, path, extensions }) => {
            console.error(
                `[GraphQL error]: Message: ${message}, Location: ${locations}, Path: ${path}`,
                extensions
            )

            // Handle authentication errors
            if (extensions?.code === 'UNAUTHENTICATED') {
                console.log('🔒 Authentication error detected - clearing tokens')
                // Clear tokens but DON'T redirect here
                // Let the component handle the redirect to avoid multiple redirects
                localStorage.removeItem('access_token')
                localStorage.removeItem('refresh_token')
                localStorage.removeItem('user_id')
            }
        })
    }

    if (networkError) {
        console.error(`[Network error]: ${networkError}`)
    }
})

// WebSocket link for subscriptions
const wsLink = typeof window !== 'undefined' ? new GraphQLWsLink(createClient({
    url: 'ws://localhost:3000/graphql',
    connectionParams: () => {
        const token = localStorage.getItem('access_token')
        const refreshToken = localStorage.getItem('refresh_token')
        const userId = localStorage.getItem('user_id')
        return {
            authorization: token ? `Bearer ${token}` : '',
            'x-refresh-token': refreshToken || '',
            'x-user-id': userId || '',
        }
    },
})) : null

// Split link: use WebSocket for subscriptions, HTTP for queries and mutations
const splitLink = typeof window !== 'undefined' && wsLink
    ? split(
        ({ query }) => {
            const definition = getMainDefinition(query)
            return (
                definition.kind === 'OperationDefinition' &&
                definition.operation === 'subscription'
            )
        },
        wsLink,
        from([errorLink, authLink.concat(httpLink)])
    )
    : from([errorLink, authLink.concat(httpLink)])

export const apolloClient = new ApolloClient({
    link: splitLink,
    cache: new InMemoryCache({
        typePolicies: {
            Query: {
                fields: {
                    events: {
                        // Custom cache key based on query variables
                        keyArgs: ['calendarId', 'timeMin', 'timeMax'],
                        merge(existing, incoming, { args }) {
                            if (!existing) return incoming

                            // If no cursor (initial load), replace
                            if (!args?.after) return incoming

                            // Merge edges for pagination
                            return {
                                ...incoming,
                                edges: [...(existing.edges || []), ...(incoming.edges || [])],
                            }
                        },
                    },
                },
            },
            Calendar: {
                fields: {
                    events: {
                        keyArgs: ['timeMin', 'timeMax'],
                        merge(existing, incoming, { args }) {
                            if (!existing) return incoming
                            if (!args?.after) return incoming

                            return {
                                ...incoming,
                                edges: [...(existing.edges || []), ...(incoming.edges || [])],
                            }
                        }
                    }
                }
            },
            EventConnection: {
                keyFields: false, // Don't use __typename + id for EventConnection
            },
            CalendarEvent: {
                keyFields: ['id'], // Use id as the cache key
            },
        }
    }),
    defaultOptions: {
        watchQuery: {
            errorPolicy: 'all',
            fetchPolicy: 'cache-first', // Prefer cache to reduce network requests
            nextFetchPolicy: 'cache-first', // Keep using cache after initial fetch
            notifyOnNetworkStatusChange: false, // Don't trigger re-renders on network status changes
        },
        query: {
            errorPolicy: 'all',
            fetchPolicy: 'cache-first',
        }
    }
})
