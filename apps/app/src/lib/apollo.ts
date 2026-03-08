import {
    ApolloClient,
    createHttpLink,
    from,
    InMemoryCache,
    split,
} from "@apollo/client";
import { setContext } from "@apollo/client/link/context";
import { onError } from "@apollo/client/link/error";
import { GraphQLWsLink } from "@apollo/client/link/subscriptions";
import { getMainDefinition } from "@apollo/client/utilities";
import { createClient } from "graphql-ws";

const API_URL = import.meta.env.VITE_API_URL || "http://localhost:3000";
const WS_URL = import.meta.env.VITE_WS_URL || API_URL.replace(/^http/, "ws");

const httpLink = createHttpLink({
    uri: `${API_URL}/graphql`,
});

const authLink = setContext((_, { headers }) => {
    const token = localStorage.getItem("access_token");
    return {
        headers: {
            ...headers,
            authorization: token ? `Bearer ${token}` : "",
        },
    };
});

const errorLink = onError(({ graphQLErrors, networkError }) => {
    if (graphQLErrors) {
        graphQLErrors.forEach(({ message, locations, path, extensions }) => {
            console.error(
                `[GraphQL error]: Message: ${message}, Location: ${locations}, Path: ${path}`,
                extensions,
            );

            // Handle authentication errors
            if (extensions?.code === "UNAUTHENTICATED") {
                localStorage.removeItem("access_token");
            }
        });
    }

    if (networkError) {
        console.error(`[Network error]: ${networkError}`);
    }
});

// WebSocket link for subscriptions
const wsLink =
    typeof window !== "undefined"
        ? new GraphQLWsLink(
              createClient({
                  url: `${WS_URL}/graphql`,
                  connectionParams: () => {
                      const token = localStorage.getItem("access_token");
                      return {
                          authorization: token ? `Bearer ${token}` : "",
                      };
                  },
              }),
          )
        : null;

// Split link: use WebSocket for subscriptions, HTTP for queries and mutations
const splitLink =
    typeof window !== "undefined" && wsLink
        ? split(
              ({ query }) => {
                  const definition = getMainDefinition(query);
                  return (
                      definition.kind === "OperationDefinition" &&
                      definition.operation === "subscription"
                  );
              },
              wsLink,
              from([errorLink, authLink.concat(httpLink)]),
          )
        : from([errorLink, authLink.concat(httpLink)]);

export const apolloClient = new ApolloClient({
    link: splitLink,
    cache: new InMemoryCache({
        typePolicies: {
            Query: {
                fields: {
                    events: {
                        // Custom cache key based on query variables
                        keyArgs: ["calendarId", "timeMin", "timeMax"],
                        merge(existing, incoming, { args }) {
                            if (!existing) return incoming;

                            // If no cursor (initial load), replace
                            if (!args?.after) return incoming;

                            // Merge edges for pagination
                            return {
                                ...incoming,
                                edges: [
                                    ...(existing.edges || []),
                                    ...(incoming.edges || []),
                                ],
                            };
                        },
                    },
                },
            },
            Calendar: {
                fields: {
                    events: {
                        keyArgs: ["timeMin", "timeMax"],
                        merge(existing, incoming, { args }) {
                            if (!existing) return incoming;
                            if (!args?.after) return incoming;

                            return {
                                ...incoming,
                                edges: [
                                    ...(existing.edges || []),
                                    ...(incoming.edges || []),
                                ],
                            };
                        },
                    },
                },
            },
            EventConnection: {
                keyFields: false, // Don't use __typename + id for EventConnection
            },
            CalendarEvent: {
                keyFields: ["id"], // Use id as the cache key
            },
        },
    }),
    defaultOptions: {
        watchQuery: {
            errorPolicy: "all",
            fetchPolicy: "cache-first", // Prefer cache to reduce network requests
            nextFetchPolicy: "cache-first", // Keep using cache after initial fetch
            notifyOnNetworkStatusChange: false, // Don't trigger re-renders on network status changes
        },
        query: {
            errorPolicy: "all",
            fetchPolicy: "cache-first",
        },
    },
});
