# Real-Time Calendar Updates with GraphQL Subscriptions

## Overview

This document describes the implementation of real-time calendar event updates using GraphQL subscriptions, efficient date-range caching, and optimized rendering for large event datasets.

## Architecture

### Backend (API)

- **GraphQL Yoga**: Built-in WebSocket support for subscriptions
- **PubSub System**: Event emitter for broadcasting real-time updates
- **Subscription Resolvers**: Handle eventCreated, eventUpdated, and eventDeleted subscriptions

### Frontend (App)
- **Apollo Client**: Split link configuration routing subscriptions via WebSocket
- **graphql-ws**: WebSocket transport protocol
- **Custom Hook**: `useEventSubscriptions` for managing event subscriptions
- **Cache Optimization**: Field policies for efficient pagination and date-range queries

## Implementation Details

### 1. WebSocket Configuration

#### Backend (`apps/api/src/server.ts`)
```typescript
const yoga = createYoga({
    schema,
    context: async ({ request }) => createContext(request),
    cors: false,
    graphqlEndpoint: "/graphql",
    graphiql: {
        subscriptionsProtocol: 'WS', // Enable WebSocket subscriptions
    },
})
```

#### Frontend (`apps/app/src/lib/apollo.ts`)
```typescript
// WebSocket link for subscriptions
const wsLink = new GraphQLWsLink(createClient({
    url: 'ws://localhost:3000/graphql',
    connectionParams: () => ({
        authorization: token ? `Bearer ${token}` : '',
        'x-refresh-token': refreshToken || '',
        'x-user-id': userId || '',
    }),
}))

// Split link: WebSocket for subscriptions, HTTP for queries/mutations
const splitLink = split(
    ({ query }) => {
        const definition = getMainDefinition(query)
        return definition.kind === 'OperationDefinition' && definition.operation === 'subscription'
    },
    wsLink,
    httpLink
)
```

### 2. PubSub System (`apps/api/src/services/pubsub.ts`)
```typescript
export type PubSubEvents = {
    eventCreated: [{ calendarId: string; event: CalendarEvent }]
    eventUpdated: [{ calendarId: string; event: CalendarEvent }]
    eventDeleted: [{ calendarId: string; payload: EventDeletedPayload }]
}

export const pubsub = createPubSub<PubSubEvents>()
```

### 3. Subscription Resolvers (`apps/api/src/resolvers/index.ts`)
```typescript
Subscription: {
    eventCreated: {
        subscribe: (_, { calendarId }, context) => {
            ensureAuth(context)
            return pipe(
                pubsub.subscribe('eventCreated'),
                filter(({ calendarId: eventCalendarId }) => 
                    !calendarId || eventCalendarId === calendarId
                )
            )
        },
        resolve: (payload) => payload.event,
    },
    // Similar for eventUpdated and eventDeleted
}
```

### 4. Apollo Cache Configuration

#### Field Policies for Pagination
```typescript
cache: new InMemoryCache({
    typePolicies: {
        Query: {
            fields: {
                events: {
                    keyArgs: ['calendarId', 'timeMin', 'timeMax'],
                    merge(existing, incoming, { args }) {
                        if (!existing) return incoming
                        if (!args?.after) return incoming // Initial load
                        
                        // Merge edges for pagination
                        return {
                            ...incoming,
                            edges: [...existing.edges, ...incoming.edges],
                        }
                    },
                },
            },
        },
    }
})
```

**Benefits:**
- `keyArgs`: Separate cache entries for different date ranges and calendars
- `merge`: Properly combines paginated results when using `fetchMore`
- Prevents unnecessary refetches for already-loaded data

### 5. Custom Subscription Hook (`apps/app/src/hooks/useEventSubscriptions.ts`)
```typescript
export function useEventSubscriptions(
    calendarId?: string,
    options?: {
        onEventCreated?: (event: CalendarEvent) => void
        onEventUpdated?: (event: CalendarEvent) => void
        onEventDeleted?: (payload: { id: string; calendarId: string }) => void
    }
)
```

**Features:**
- Subscribes to all three event types simultaneously
- Filters by calendarId when provided
- Provides callbacks for handling each event type
- Automatically manages subscription lifecycle

### 6. UI Integration (`apps/app/src/routes/schedule.tsx`)
```typescript
useEventSubscriptions(selectedCalendarId, {
    onEventCreated: (event) => {
        console.log('📅 New event created:', event)
        refetchEvents()
    },
    onEventUpdated: (event) => {
        console.log('📝 Event updated:', event)
        refetchEvents()
    },
    onEventDeleted: (payload) => {
        console.log('🗑️ Event deleted:', payload)
        client.cache.evict({ 
            id: client.cache.identify({ 
                __typename: 'CalendarEvent', 
                id: payload.id 
            }) 
        })
        client.cache.gc()
    },
})
```

## Performance Optimizations

### 1. Memoized Time Ranges
```typescript
const [timeRange] = useState(() => ({
    timeMin: new Date().toISOString(),
    timeMax: new Date(Date.now() + 30 * 24 * 60 * 60 * 1000).toISOString()
}))
```
- Computed once on component mount
- Prevents unnecessary refetches on re-renders

### 2. Cache-First Fetch Policy
```typescript
const { data: userData } = useGetMeQuery({
    fetchPolicy: 'cache-first',
    errorPolicy: 'all',
})
```
- Uses cached data when available
- Reduces API calls

### 3. Infinite Scroll Pagination
```typescript
fetchMore({
    variables: { after: eventsData.events.pageInfo.endCursor },
    updateQuery: (prev, { fetchMoreResult }) => ({
        events: {
            ...fetchMoreResult.events,
            edges: [...prev.events.edges, ...fetchMoreResult.events.edges],
        },
    }),
})
```
- Loads events incrementally
- Efficient for large datasets

### 4. Calendar Grouping and Sorting
```typescript
const myCalendars = calendarsData?.calendars
    ?.filter(cal => cal.accessRole === 'OWNER')
    .sort((a, b) => {
        if (a.primary && !b.primary) return -1
        if (!a.primary && b.primary) return 1
        return a.summary.localeCompare(b.summary)
    })
```
- Primary calendar always first
- Alphabetical sorting within groups
- Improved UX

## Usage

### Testing Subscriptions
1. Start the API server: `cd apps/api && bun run dev`
2. Start the app: `cd apps/app && bun run dev`
3. Open the schedule page and authenticate
4. In another tab, use GraphiQL to trigger mutations (when implemented)
5. Watch events update in real-time

### Publishing Events (Future Implementation)
When mutations are implemented, publish events after successful operations:

```typescript
// In createEvent mutation
const newEvent = await calendarService.createEvent(...)
pubsub.publish('eventCreated', { 
    calendarId: args.calendarId, 
    event: newEvent 
})
```

## Future Enhancements

1. **Optimistic Updates**: Update UI immediately before server confirmation
2. **Conflict Resolution**: Handle concurrent edits from multiple clients
3. **Batch Updates**: Group multiple rapid updates to reduce re-renders
4. **Selective Subscriptions**: Subscribe only to visible date ranges
5. **Offline Support**: Queue mutations when offline, sync when reconnected

## Dependencies

### Backend
- `graphql-yoga`: ^5.16.2 (includes WebSocket support)

### Frontend
- `@apollo/client`: ^3.11.11
- `graphql-ws`: ^6.0.6
- `@graphql-codegen/cli`: ^6.0.1
- `@graphql-codegen/client-preset`: ^5.1.1

## Files Modified/Created

### Backend
- ✅ `apps/api/src/server.ts` - Enabled WebSocket in Yoga config
- ✅ `apps/api/src/services/pubsub.ts` - Created PubSub system
- ✅ `apps/api/src/resolvers/index.ts` - Added subscription resolvers

### Frontend
- ✅ `apps/app/src/lib/apollo.ts` - Configured WebSocket split link and cache policies
- ✅ `apps/app/src/hooks/useEventSubscriptions.ts` - Custom subscription hook
- ✅ `apps/app/src/routes/schedule.tsx` - Integrated subscriptions
- ✅ `apps/app/src/graphql/queries.graphql` - Added subscription documents
- ✅ `apps/app/codegen.ts` - GraphQL code generator config
- ✅ `apps/app/package.json` - Added codegen script

## Conclusion

The implementation provides:
- ✅ Real-time event updates via GraphQL subscriptions
- ✅ Efficient date-range caching with Apollo Client
- ✅ Optimized pagination for large event datasets
- ✅ Clean separation of concerns with custom hooks
- ✅ Scalable architecture for future enhancements
