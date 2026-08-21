# Real-Time Updates with GraphQL Subscriptions

## Overview

Sunrise pushes calendar and routine changes to clients in real time over GraphQL subscriptions. Queries and mutations use HTTP; subscriptions use a [graphql-ws](https://github.com/enisdenjo/graphql-ws) WebSocket on the **same** `/graphql` endpoint.

Two things publish events:

1. **Mutations** — creating/updating/deleting an event or routine through the API publishes immediately.
2. **The polling service** — a background poller checks each authenticated user's owned/writable Google calendars every 60 seconds, diffs against the previous snapshot, and publishes any changes. This is how edits made directly in Google Calendar (outside Sunrise) reach connected clients in near-real-time.

## Architecture

### Backend (`apps/api`)

- **GraphQL Yoga** serves the HTTP transport; **graphql-ws (`graphql-ws/use/bun`)** serves the WebSocket transport. Both share one executable schema built with `@graphql-tools/schema`.
- **PubSub** (`createPubSub` from graphql-yoga) broadcasts typed events in-process.
- **Subscription resolvers** handle `eventCreated`/`eventUpdated`/`eventDeleted` (optionally filtered by `calendarId`) and `routineCreated`/`routineUpdated`/`routineDeleted`.
- **PollingService** publishes external changes detected by diffing Google Calendar snapshots.

### Frontend (`apps/app`)

- **Apollo Client** with a split link: subscriptions route to a `GraphQLWsLink`, everything else to HTTP.
- **`useEventSubscriptions`** hook subscribes to all three event subscriptions for a calendar and exposes callbacks.
- **Cache field policies** for paginated, date-range-keyed event queries.

## Implementation

### WebSocket transport (`apps/api/src/server.ts`)

The Bun server routes WebSocket upgrades on `/graphql` to the graphql-ws handler; all other requests go to Hono (which mounts Yoga at `/graphql` for HTTP):

```typescript
import { handleProtocols, makeHandler } from "graphql-ws/use/bun";

// WebSocket transport (graphql-ws) for GraphQL subscriptions.
// Clients authenticate by sending { authorization: "Bearer <jwt>" } in the
// connectionParams of the graphql-ws connection init message.
const websocket = makeHandler({
    schema,
    context: (ctx) =>
        createContextFromAuthHeader(
            ctx.connectionParams?.authorization as string | undefined,
        ),
});

export default {
    port: env.port,
    fetch(req: Request, server: Server<undefined>) {
        if (
            req.headers.get("upgrade") === "websocket" &&
            new URL(req.url).pathname === "/graphql"
        ) {
            // ...validate subprotocol, then server.upgrade(req)
        }
        return app.fetch(req); // Hono handles HTTP (incl. Yoga at /graphql)
    },
    websocket,
};
```

### Authentication

WebSocket connections authenticate with a **single** connection param — the same JWT used for HTTP requests:

```typescript
// apps/app/src/lib/apollo.ts
const wsLink = new GraphQLWsLink(
    createClient({
        url: `${WS_URL}/graphql`,
        connectionParams: () => {
            const token = localStorage.getItem("access_token");
            return {
                authorization: token ? `Bearer ${token}` : "",
            };
        },
    }),
);
```

Google refresh tokens live server-side only and are never sent by the client.

> **User scoping:** subscriptions are user-scoped — every published payload carries the owning `userId` and subscription resolvers filter on it, so clients only receive events for their own account.

> **Context lifetime:** graphql-ws resolves the GraphQL context once per operation, so an already-running subscription continues to receive events after its JWT expires, until the socket closes. New operations started on the same socket re-authenticate.

### Split link (`apps/app/src/lib/apollo.ts`)

```typescript
const splitLink = split(
    ({ query }) => {
        const definition = getMainDefinition(query);
        return (
            definition.kind === "OperationDefinition" &&
            definition.operation === "subscription"
        );
    },
    wsLink,
    from([errorLink, authLink.concat(httpLink)]),
);
```

### PubSub event map (`apps/api/src/services/pubsub.ts`)

```typescript
export type PubSubEvents = {
    eventCreated: [{ calendarId: string; event: GqlCalendarEvent }];
    eventUpdated: [{ calendarId: string; event: GqlCalendarEvent }];
    eventDeleted: [{ calendarId: string; payload: GqlEventDeletedPayload }];
    routineCreated: [{ routine: GqlRoutine }];
    routineUpdated: [{ routine: GqlRoutine }];
    routineDeleted: [{ payload: GqlRoutineDeletedPayload }];
};

export const pubsub = createPubSub<PubSubEvents>();
```

### Subscription resolvers (`apps/api/src/resolvers/index.ts`)

Event subscriptions require authentication and support optional `calendarId` filtering:

```typescript
eventCreated: {
    subscribe: (_, { calendarId }, context: GraphQLContext) => {
        ensureAuth(context);
        return pipe(
            pubsub.subscribe("eventCreated"),
            filter(
                ({ calendarId: eventCalendarId }) =>
                    !calendarId || eventCalendarId === calendarId,
            ),
        );
    },
    resolve: (payload) => payload.event,
},
```

`eventUpdated`/`eventDeleted` follow the same shape; routine subscriptions (`routineCreated`/`routineUpdated`/`routineDeleted`) are unfiltered but require an authenticated user.

### Publishing events

**Mutations** publish after each successful operation, e.g.:

```typescript
// createEvent mutation
pubsub.publish("eventCreated", {
    calendarId: input.calendarId,
    event: gqlEvent,
});

// createRoutine mutation
pubsub.publish("routineCreated", { routine: gqlRoutine });
```

**The poller** (`apps/api/src/services/poller.ts`) publishes external changes. Every 60 seconds (configurable interval) it lists events in a rolling window for each user's owned/writable calendars, compares event ids and `updated` timestamps against the previous snapshot, and publishes:

```typescript
for (const [id, event] of current) {
    if (!previous.has(id)) {
        pubsub.publish("eventCreated", {
            calendarId,
            event: mapGCalEvent(event, calendarId),
        });
    } else if (previous.get(id) !== (event.updated ?? "")) {
        pubsub.publish("eventUpdated", { /* ... */ });
    }
}
for (const id of previous.keys()) {
    if (!current.has(id)) {
        pubsub.publish("eventDeleted", {
            calendarId,
            payload: { id, calendarId },
        });
    }
}
```

The first observation of a user/calendar seeds the snapshot silently (no event storm on startup).

### Client subscription hook (`apps/app/src/hooks/useEventSubscriptions.ts`)

```typescript
export function useEventSubscriptions(
    calendarId?: string,
    options?: {
        onEventCreated?: (event: CalendarEvent) => void;
        onEventUpdated?: (event: CalendarEvent) => void;
        onEventDeleted?: (payload: { id: string; calendarId: string }) => void;
    },
): EventSubscriptionHookResult;
```

Subscribes to all three event subscriptions via generated Apollo hooks; subscriptions are skipped until a resolved calendar id is provided (the server publishes real calendar ids, not `"primary"`). Used by `apps/app/src/routes/schedule.tsx` to refetch/evict on incoming events.

### Apollo cache configuration

`apps/app/src/lib/apollo.ts` defines field policies so event queries are cached per calendar and date range and merge correctly under cursor pagination:

- `keyArgs: ["calendarId", "timeMin", "timeMax"]` on `Query.events` (and `["timeMin", "timeMax"]` on `Calendar.events`)
- a `merge` function that appends `edges` when an `after` cursor is present and replaces on initial load
- `CalendarEvent` keyed by `id` so subscription payloads update cached entities

## Trying It Out

1. Start the API: `cd apps/api && bun dev`
2. Start the app: `cd apps/app && bun dev`
3. Sign in and open the schedule page.
4. Create/edit/delete an event directly in Google Calendar — the change appears in Sunrise within ~60 seconds. Changes made through Sunrise itself appear instantly in other connected clients.

You can also test from GraphiQL at `http://localhost:3000/graphql` (subscriptions run over WS).

## Future Enhancements

1. **Optimistic updates**: update the UI immediately before server confirmation
2. **Google push notifications**: replace polling with Google Calendar push channels (watch/webhook) for lower latency and fewer API calls
3. **Selective date-range subscriptions**: subscribe only to visible date ranges to cut payload volume

## Dependencies

### Backend (`apps/api`)

- `graphql-yoga` ^5.16.2 (HTTP transport, `createPubSub`, `pipe`/`filter`)
- `graphql-ws` ^6.0.6 (WebSocket transport via `graphql-ws/use/bun`)
- `@graphql-tools/schema` ^10 (shared executable schema)

### Frontend (`apps/app`)

- `@apollo/client` ^3.11.11
- `graphql-ws` ^6.0.6
- `@graphql-codegen/cli` ^6 + `typescript-react-apollo` (generated subscription hooks)

## Key Files

### Backend

- `apps/api/src/server.ts` — Bun WebSocket upgrade + graphql-ws handler; Yoga HTTP transport
- `apps/api/src/services/pubsub.ts` — typed PubSub event map
- `apps/api/src/services/poller.ts` — polling/diffing service that publishes external changes
- `apps/api/src/resolvers/index.ts` — subscription resolvers + mutation-side publishes
- `apps/api/src/schema.graphql` — `Subscription` type definition

### Frontend

- `apps/app/src/lib/apollo.ts` — WS client, split link, cache policies
- `apps/app/src/hooks/useEventSubscriptions.ts` — subscription hook
- `apps/app/src/graphql/queries.graphql` — subscription documents (events + routines)
- `apps/app/src/routes/schedule.tsx` — UI integration
