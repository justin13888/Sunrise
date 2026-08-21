# Sunrise

Sunrise is an open-source daily routine app that helps you focus on what matters! It aims to be accessible, available on all major desktop platforms, open source, and built with performant technologies.

<!-- TODO: Add screenshot and demo link -->

## Why Sunrise?

It's simple. Everybody has their own ways to stay organized but we give you simple, well-thought tools, for free! Self-host to maintain control of your data. Contribute to the open-source codebase to add features. Give feedback to help everyone else.

## Features

- **Google Calendar integration**: Connect your Google Calendar to view and manage events
- **Real-time sync**: The server polls your owned/writable Google calendars every 60 seconds, diffs against the last snapshot, and pushes changes to clients over GraphQL subscriptions (WebSocket) — so edits made directly in Google Calendar show up in Sunrise in near-real-time. Mutations made through Sunrise publish updates instantly.
- **Desktop-friendly OAuth**: Sign in with Google via a popup on the web, or via your system browser plus a copyable authorization code in the Tauri desktop app
- **Routines**: Create, update, and delete routines with schema-validated inputs (TypeBox — e.g. flexibility 0–100, duration 5–480 minutes)
- **GraphQL API**: Typed end to end with code generation on both server and client

## Development

### Technologies

- **Frontend**: React 19, TanStack Router, Apollo Client (split HTTP/WS link), Tailwind CSS
- **Backend**: Bun, Hono, GraphQL Yoga, graphql-ws
- **Desktop**: Tauri v2 (Rust)
- **Database**: SQLite via `bun:sqlite` + drizzle-orm, with committed migrations applied automatically at startup
- **Testing**: Vitest with coverage thresholds enforced in CI
- **Build tools**: Vite, TypeScript, GraphQL Code Generator, Biome, lefthook

### Project Structure

```
apps/
  api/        - GraphQL API server (Bun + Hono + Yoga), SQLite storage, Google Calendar sync
  app/        - React frontend with Tauri v2 desktop wrapper
packages/
  gcal/       - Google Calendar service library (pure, no server code)
  models/     - Shared TypeBox schemas and compiled validators
```

### Getting Started

1. **Install dependencies**:

   ```bash
   bun install
   ```

2. **Install git hooks** (lefthook is a dev dependency):

   ```bash
   bunx lefthook install
   ```

3. **Configure the API**: copy `apps/api/.env.example` to `apps/api/.env` and fill in your Google OAuth credentials. See [apps/api/README.md](apps/api/README.md) for the full environment reference and Google Cloud setup steps.

4. **Start the API server**:

   ```bash
   cd apps/api
   bun dev
   ```

   The GraphQL endpoint (HTTP + WebSocket) is at `http://localhost:3000/graphql`. The SQLite database is created and migrated automatically on startup.

5. **Start the frontend**:

   ```bash
   cd apps/app
   bun dev            # Web at http://localhost:1420
   bun run tauri dev  # Desktop (Tauri) - see apps/app/README.md for prerequisites
   ```

### Testing

> **Note**: Use `bun run test:run`, not `bun test`. `bun test` invokes Bun's built-in test runner instead of Vitest and will not run the suite correctly.

```bash
bun run test:run       # Run the Vitest suite once
bun run test           # Watch mode
bun run test:ui        # Vitest UI
bun run test:coverage  # Coverage report (80% thresholds enforced)
```

The suite covers the API services (JWT, token store, routine service, polling/diffing), resolvers and scalar types, the gcal library, and the models package validators.

Other useful root scripts:

```bash
bun run codegen    # GraphQL codegen for api + app (offline, from the committed schema)
bun run typecheck  # TypeScript typecheck across all workspaces
bun run ci         # Biome lint + format check
bun run validate   # ci + codegen + typecheck + test:coverage
```

## Architecture

### Authentication Flow

1. The client requests a Google OAuth URL from the API and opens it (popup on web, system browser on desktop).
2. Google redirects to the API's `/auth/callback` page, which relays the authorization code to the opener via `postMessage` (web) and always displays it as copyable text (desktop flow, where there is no opener window).
3. The client sends the code to the `authenticateWithCode` mutation. The server exchanges it with Google, stores the Google access/refresh tokens **server-side only**, and returns a short-lived (1 hour) JWT.
4. The client authenticates subsequent HTTP requests with `Authorization: Bearer <jwt>` and WebSocket connections via graphql-ws `connectionParams: { authorization: "Bearer <jwt>" }`. Google tokens never reach the client.

### Real-Time Sync

- Queries and mutations go over HTTP; subscriptions go over a graphql-ws WebSocket on the same `/graphql` endpoint.
- Mutations publish `eventCreated`/`eventUpdated`/`eventDeleted` and `routineCreated`/`routineUpdated`/`routineDeleted` events immediately.
- A background `PollingService` polls each authenticated user's owned/writable Google calendars every 60 seconds and publishes diffs, so changes made outside Sunrise propagate to connected clients too.

See [SUBSCRIPTIONS.md](SUBSCRIPTIONS.md) for details.

### API Design

- **GraphQL schema**: single committed schema (`apps/api/src/schema.graphql`) consumed by both server and client codegen
- **Custom scalars**: DateTime and URL with validation
- **Input validation**: routine inputs validated against shared TypeBox schemas from `@sunrise/models`
- **Error handling**: structured errors with stable extension codes

## License

Sunrise is licensed under the [AGPL-3.0 License](LICENSE).
