import { readFileSync } from "node:fs";
import { join } from "node:path";
import { makeExecutableSchema } from "@graphql-tools/schema";
import { createYoga } from "graphql-yoga";
import { Hono } from "hono";
import { cors } from "hono/cors";
import { logger } from "hono/logger";
import { createContext } from "./context";
import { resolvers } from "./resolvers";

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

        console.log(`🔍 [DEBUG] ${method} ${url}`);

        // Log request headers in debug mode
        if (process.env.DEBUG_VERBOSE === "true") {
            const headers = c.req.header();
            console.log("📝 [DEBUG] Request Headers:", headers);

            // TODO: Fix
            // Note: We can't log the request body here because it would consume the stream
            // and make it unavailable for GraphQL Yoga. To debug request bodies,
            // enable GraphQL Yoga's own debug logging instead.
        }

        await next();

        const ms = Date.now() - start;
        console.log(`⏱️  [DEBUG] ${method} ${url} - ${c.res.status} (${ms}ms)`);
    });
}

// CORS configuration
app.use(
    "/*",
    cors({
        // origin: process.env.ALLOWED_ORIGINS ? process.env.ALLOWED_ORIGINS.split(',') : '*',
        origin: "*",
        credentials: true,
    }),
);

// Load GraphQL schema
const typeDefs = readFileSync(join(import.meta.dir, "schema.graphql"), "utf8");

// Create executable schema
const schema = makeExecutableSchema({
    typeDefs,
    resolvers,
});

// Create GraphQL Yoga instance
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

// Auth callback endpoint (for Google OAuth)
app.get("/auth/callback", async (c) => {
    const code = c.req.query("code");
    const error = c.req.query("error");
    const state = c.req.query("state"); // Contains userId for proper user identification

    if (error) {
        console.error("OAuth error:", error);
        return c.html(`
      <html>
        <body>
          <h1>Authentication Error</h1>
          <p>Error: ${error}</p>
          <script>
            window.opener?.postMessage({ error: '${error}' }, '*')
            window.close()
          </script>
        </body>
      </html>
    `);
    }

    if (code) {
        try {
            // TODO: For now, we'll use a dummy user ID. In production, you'd:
            // 1. Extract userId from JWT token in the 'state' parameter
            // 2. Validate the token
            // 3. Use the actual user ID
            const userId = state || "temp-user-" + Date.now();

            console.log(`🔐 Processing OAuth callback for user: ${userId}`);

            // Exchange the code for tokens using GraphQL mutation
            // This will redirect to the frontend with the code for the frontend to handle
            return c.html(`
      <html>
        <body>
          <h1>Authentication Successful</h1>
          <p>Completing authentication...</p>
          <script>
            // Pass the authorization code and user info to the parent window
            window.opener?.postMessage({ 
              code: '${code}', 
              userId: '${userId}',
              success: true 
            }, '*')
            window.close()
          </script>
        </body>
      </html>
    `);
        } catch (error) {
            console.error("Error processing OAuth callback:", error);
            return c.html(`
      <html>
        <body>
          <h1>Authentication Error</h1>
          <p>Failed to process authentication. Please try again.</p>
          <script>
            window.opener?.postMessage({ error: 'Processing failed' }, '*')
            window.close()
          </script>
        </body>
      </html>
    `);
        }
    }

    return c.text("Invalid callback - missing code parameter", 400);
});

// Alternative server-side OAuth callback (exchanges code for tokens directly)
app.post("/auth/exchange", async (c) => {
    try {
        const { code, userId } = await c.req.json();

        if (!code) {
            return c.json({ error: "Authorization code is required" }, 400);
        }

        // TODO: Validate userId from JWT token in production
        const finalUserId = userId || "temp-user-" + Date.now();

        console.log(`🔐 Server-side token exchange for user: ${finalUserId}`);

        // Get calendar service from context (we'll create it here)
        const clientId = process.env.GOOGLE_CLIENT_ID;
        const clientSecret = process.env.GOOGLE_CLIENT_SECRET;

        if (!clientId || !clientSecret) {
            return c.json({ error: "Server configuration error" }, 500);
        }

        const { GoogleCalendarService } = await import("@sunrise/gcal");
        const calendarService = new GoogleCalendarService(
            clientId,
            clientSecret,
            process.env.GOOGLE_REDIRECT_URI ||
                "http://localhost:3000/auth/callback",
        );

        // Exchange code for tokens
        const tokens = await calendarService.getTokensFromCode(code);

        if (!tokens.access_token || !tokens.refresh_token) {
            return c.json({ error: "Failed to exchange code for tokens" }, 400);
        }

        // Get user info from Google
        let userInfo = {
            email: "user@example.com",
            name: "User",
            picture: undefined as string | undefined,
        };

        try {
            const { OAuth2Client } = await import("google-auth-library");
            const oauth2Client = new OAuth2Client();
            oauth2Client.setCredentials({
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
            });

            const { google } = await import("googleapis");
            const oauth2 = google.oauth2({ version: "v2", auth: oauth2Client });
            const userInfoResponse = await oauth2.userinfo.get();

            if (userInfoResponse.data) {
                userInfo = {
                    email: userInfoResponse.data.email || userInfo.email,
                    name: userInfoResponse.data.name || userInfo.name,
                    picture: userInfoResponse.data.picture || undefined,
                };
            }
        } catch (userInfoError) {
            console.warn("Could not fetch user info:", userInfoError);
        }

        // Store tokens
        const expiresIn = tokens.expiry_date
            ? Math.floor((tokens.expiry_date - Date.now()) / 1000)
            : 3600;
        const expiresAt = new Date(Date.now() + expiresIn * 1000);

        const { tokenStore } = await import("./services/tokenStore");
        await tokenStore.storeTokens(finalUserId, {
            userId: finalUserId,
            accessToken: tokens.access_token,
            refreshToken: tokens.refresh_token,
            expiresAt,
            email: userInfo.email,
            name: userInfo.name,
            picture: userInfo.picture,
        });

        console.log(
            `✅ Successfully stored tokens for user: ${finalUserId} (${userInfo.email})`,
        );

        return c.json({
            success: true,
            user: {
                id: finalUserId,
                email: userInfo.email,
                name: userInfo.name,
                picture: userInfo.picture,
                verified: true,
            },
            accessToken: tokens.access_token,
            refreshToken: tokens.refresh_token,
            expiresIn,
        });
    } catch (error) {
        console.error("Token exchange failed:", error);
        return c.json({ error: "Token exchange failed" }, 500);
    }
});

const port = parseInt(process.env.PORT || "3000");

console.log(`🚀 Server running on http://localhost:${port}`);
console.log(`📊 GraphQL endpoint: http://localhost:${port}/graphql`);

if (isDebug) {
    console.log(`🐛 Debug logging enabled`);
    if (process.env.DEBUG_VERBOSE === "true") {
        console.log(`🔍 Verbose debug logging enabled`);
    }
}

export default {
    port,
    fetch: app.fetch,
};
