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

    // Return the headers to the context so httpLink can read them
    return {
        headers: {
            ...headers,
            authorization: token ? `Bearer ${token}` : '',
            'x-refresh-token': refreshToken || '',
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
                // Clear tokens and redirect to auth
                localStorage.removeItem('access_token')
                localStorage.removeItem('refresh_token')
                // Could trigger a redirect to login page
                window.location.href = '/auth'
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
            errorPolicy: 'all'
        },
        query: {
            errorPolicy: 'all'
        }
    }
})
