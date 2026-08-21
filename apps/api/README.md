# @sunrise/api

Sunrise GraphQL API server: Bun + Hono + GraphQL Yoga, with graphql-ws WebSocket subscriptions on the same `/graphql` endpoint and SQLite storage (bun:sqlite + drizzle-orm).

## Setup

### 1. Set up Google OAuth

1. Go to the [Google Cloud Console](https://console.cloud.google.com/)
2. Create a new project or select an existing one
3. Enable the **Google Calendar API**
4. Create OAuth 2.0 credentials:
   - Application type: Web application
   - Authorized redirect URI: `http://localhost:3000/auth/callback`
5. Copy the client ID and client secret

### 2. Configure environment

Copy `.env.example` to `.env` and fill in your credentials:

```bash
cp .env.example .env
```

| Variable | Required | Default | Description |
| --- | --- | --- | --- |
| `GOOGLE_CLIENT_ID` | **Yes** | — | Google OAuth client ID. The server refuses to start without it. |
| `GOOGLE_CLIENT_SECRET` | **Yes** | — | Google OAuth client secret. The server refuses to start without it. |
| `GOOGLE_REDIRECT_URI` | No | `http://localhost:3000/auth/callback` | OAuth redirect URI; must match the one registered in Google Cloud. |
| `JWT_SECRET` | **In production** | ephemeral random (dev only) | JWT signing secret. Required when `NODE_ENV=production`. In development, a random in-process secret is generated if unset (sessions do not survive restarts). Generate one with `openssl rand -hex 32`. |
| `PORT` | No | `3000` | HTTP/WebSocket port. |
| `DB_PATH` | No | `data/sunrise.db` (relative to cwd) | SQLite database file path. Parent directory is created automatically. |
| `ALLOWED_ORIGINS` | No | `http://localhost:1420,http://tauri.localhost,tauri://localhost` | Comma-separated CORS allowlist (exact origin match, no credentials). |
| `FRONTEND_ORIGIN` | No | first entry of `ALLOWED_ORIGINS` | `postMessage` target origin used by the OAuth callback page in the web popup flow. |
| `NODE_ENV` | No | — | `production` enables strict config validation; `development` enables debug logging. |
| `DEBUG` | No | `false` | `true` enables request logging outside development. |
| `DEBUG_VERBOSE` | No | `false` | `true` additionally logs request headers (authorization redacted). |

### 3. Run

```bash
bun install
bun dev        # GraphQL codegen + watch-mode server
bun run start  # Production start (no codegen, no watch)
```

Notes:

- The GraphQL endpoint is `http://localhost:3000/graphql` and serves **both** HTTP (queries/mutations, GraphiQL) and graphql-ws WebSocket subscriptions. WebSocket clients authenticate via `connectionParams: { authorization: "Bearer <jwt>" }`.
- The SQLite database is created at `DB_PATH` and the committed drizzle migrations (`drizzle/`) are applied automatically at startup — no manual migration step.
- `/auth/callback` renders the Google authorization code as copyable text and relays it to the opener window via `postMessage` (web popup flow). The desktop app uses the copy/paste path.
- `/health` is a simple health-check endpoint.
- A background polling service syncs each authenticated user's owned/writable Google calendars every 60 seconds and publishes changes to subscribers (see [../../SUBSCRIPTIONS.md](../../SUBSCRIPTIONS.md)).
