import { ApolloClient, InMemoryCache, createHttpLink, from } from '@apollo/client'
import { setContext } from '@apollo/client/link/context'
import { onError } from '@apollo/client/link/error'

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

export const apolloClient = new ApolloClient({
    link: from([
        errorLink,
        authLink.concat(httpLink)
    ]),
    cache: new InMemoryCache({
        typePolicies: {
            Calendar: {
                fields: {
                    events: {
                        merge(_, incoming) {
                            return incoming
                        }
                    }
                }
            }
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
