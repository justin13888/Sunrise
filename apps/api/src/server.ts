import { readFileSync } from "node:fs";
import { join } from "node:path";
import { makeExecutableSchema } from "@graphql-tools/schema";
import type { Server } from "bun";
import { handleProtocols, makeHandler } from "graphql-ws/use/bun";
import { createYoga } from "graphql-yoga";
import { Hono } from "hono";
import { cors } from "hono/cors";
import { logger } from "hono/logger";
import { createContext, createContextFromAuthHeader } from "./context";
import { validateEnv } from "./env";
import { resolvers } from "./resolvers";
import { PollingService } from "./services/poller";

// Validate configuration before anything else so we fail fast with a clear
// message instead of starting a half-configured server or poller.
const env = validateEnv();

const app = new Hono();

// Debug logging middleware
const isDebug =
    process.env.NODE_ENV === "development" || process.env.DEBUG === "true";
if (isDebug) {
    app.use("*", logger());

    // Additional debug middleware for detailed request logging
    app.use("*", async (c, next) => {
        const start = Date.now();
        const { method, url } = c.req;

        console.log(`[debug] ${method} ${url}`);

        if (process.env.DEBUG_VERBOSE === "true") {
            // Never log credentials, even in verbose debug mode.
            const headers: Record<string, string> = { ...c.req.header() };
            for (const name of Object.keys(headers)) {
                if (name.toLowerCase() === "authorization") {
                    headers[name] = "<redacted>";
                }
            }
            console.log("[debug] request headers:", headers);
        }

        await next();

        const ms = Date.now() - start;
        console.log(`[debug] ${method} ${url} - ${c.res.status} (${ms}ms)`);
    });
}

// CORS: exact-match allowlist, no credentials (auth is via Bearer tokens).
app.use(
    "/*",
    cors({
        origin: (origin) =>
            env.allowedOrigins.includes(origin) ? origin : undefined,
    }),
);

// Load GraphQL schema
const typeDefs = readFileSync(join(import.meta.dir, "schema.graphql"), "utf8");

// Create executable schema (shared by the HTTP and WebSocket transports)
const schema = makeExecutableSchema({
    typeDefs,
    resolvers,
});

// Create GraphQL Yoga instance (HTTP transport)
const yoga = createYoga({
    schema,
    context: async ({ request }) => createContext(request),
    cors: false, // We handle CORS above
    graphqlEndpoint: "/graphql",
    // Enable GraphiQL with subscriptions support
    graphiql: {
        subscriptionsProtocol: "WS",
    },
});

// GraphQL endpoint
app.all("/graphql", async (c) => yoga.fetch(c.req.raw));

// Health check endpoint
app.get("/health", (c) => {
    return c.json({ status: "ok", timestamp: new Date().toISOString() });
});

/** Escape a string for interpolation into HTML text content. */
function escapeHtml(value: string): string {
    return value
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;")
        .replace(/"/g, "&quot;")
        .replace(/'/g, "&#39;");
}

/** Encode a value as a JS expression safe to embed inside a <script> block. */
function jsValue(value: unknown): string {
    return JSON.stringify(value).replace(/</g, "\\u003c");
}

// Auth callback endpoint (for Google OAuth).
// Relays the authorization code to the opener (web popup flow) and always
// renders it as copyable text (desktop/system-browser flow). The user is
// derived server-side by the authenticateWithCode mutation.
app.get("/auth/callback", async (c) => {
    const code = c.req.query("code");
    const error = c.req.query("error");

    if (error) {
        console.error("OAuth error:", error);
        return c.html(`<!doctype html>
<html>
  <body>
    <h1>Authentication Error</h1>
    <p>Error: ${escapeHtml(error)}</p>
    <p>You can close this window and try again.</p>
    <script>
      if (window.opener) {
        window.opener.postMessage(
          { source: "sunrise-oauth", error: ${jsValue(error)} },
          ${jsValue(env.frontendOrigin)}
        );
        window.close();
      }
    </script>
  </body>
</html>`);
    }

    if (!code) {
        return c.text("Invalid callback - missing code parameter", 400);
    }

    return c.html(`<!doctype html>
<html>
  <body>
    <h1>Authentication Successful</h1>
    <p>
      If this window does not close automatically, copy the authorization
      code below and paste it into Sunrise:
    </p>
    <p><code>${escapeHtml(code)}</code></p>
    <script>
      var code = ${jsValue(code)};
      if (window.opener) {
        window.opener.postMessage(
          { source: "sunrise-oauth", code: code },
          ${jsValue(env.frontendOrigin)}
        );
        window.close();
      }
    </script>
  </body>
</html>`);
});

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

// Start background polling service (only after env validation succeeded).
const pollingService = new PollingService(
    env.googleClientId,
    env.googleClientSecret,
    env.googleRedirectUri,
);
pollingService.start();

function shutdown(signal: string): void {
    console.log(`Received ${signal}, shutting down...`);
    pollingService.stop();
    process.exit(0);
}
process.on("SIGINT", () => shutdown("SIGINT"));
process.on("SIGTERM", () => shutdown("SIGTERM"));

console.log(`Sunrise API configured for http://localhost:${env.port}`);
console.log(
    `GraphQL endpoint (HTTP + WebSocket): http://localhost:${env.port}/graphql`,
);
if (isDebug) {
    console.log("Debug logging enabled");
    if (process.env.DEBUG_VERBOSE === "true") {
        console.log("Verbose debug logging enabled");
    }
}

export default {
    port: env.port,
    fetch(req: Request, server: Server<undefined>) {
        if (
            req.headers.get("upgrade")?.toLowerCase() === "websocket" &&
            new URL(req.url).pathname === "/graphql"
        ) {
            if (
                !handleProtocols(
                    req.headers.get("sec-websocket-protocol") ?? "",
                )
            ) {
                return new Response(
                    "Bad Request: unsupported WebSocket subprotocol",
                    { status: 400 },
                );
            }
            if (server.upgrade(req)) {
                return;
            }
            return new Response("Internal Server Error", { status: 500 });
        }
        return app.fetch(req);
    },
    websocket,
};
