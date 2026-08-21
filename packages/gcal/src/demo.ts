/* v8 ignore start */
/**
 * Demo / CLI helpers for local experimentation with the Google Calendar API.
 *
 * This module is intentionally NOT exported from the package. It reads
 * credentials from files in the current working directory and prints to the
 * console; none of that belongs in the library surface (see ./index.ts).
 *
 * Usage: `bun run packages/gcal/src/demo.ts` with a `token.json` (and
 * optionally `credentials.json`) in the current working directory.
 */
import * as fs from "node:fs/promises";
import * as path from "node:path";
import * as process from "node:process";
import { OAuth2Client } from "google-auth-library";
import { google } from "googleapis";

const CREDENTIALS_PATH = path.join(process.cwd(), "credentials.json");
const TOKEN_PATH = path.join(process.cwd(), "token.json");

/**
 * Reads previously authorized credentials from the save file.
 */
async function loadSavedCredentialsIfExist() {
    try {
        const content = await fs.readFile(TOKEN_PATH);
        const credentials = JSON.parse(content.toString());
        return google.auth.fromJSON(credentials);
    } catch {
        return null;
    }
}

/**
 * Serializes credentials to a file compatible with GoogleAuth.fromJSON.
 */
async function _saveCredentials(client: OAuth2Client): Promise<void> {
    const content = await fs.readFile(CREDENTIALS_PATH);
    const keys = JSON.parse(content.toString());
    const key = keys.installed || keys.web;
    const payload = JSON.stringify({
        type: "authorized_user",
        client_id: key.client_id,
        client_secret: key.client_secret,
        refresh_token: client.credentials.refresh_token,
    });
    await fs.writeFile(TOKEN_PATH, payload);
}

/**
 * Load or request authorization to call APIs.
 * For server applications, this should be replaced with proper token management.
 */
async function authorize(): Promise<OAuth2Client | null> {
    const savedClient = await loadSavedCredentialsIfExist();
    if (savedClient && savedClient instanceof OAuth2Client) {
        return savedClient;
    }

    console.log("No saved credentials found.");
    console.log("For server applications, you should:");
    console.log(
        "1. Use GoogleCalendarService.getAuthUrl() to get authorization URL",
    );
    console.log("2. Direct users to that URL to grant permissions");
    console.log(
        "3. Handle the callback with GoogleCalendarService.getTokensFromCode()",
    );
    console.log("4. Store the refresh token for future use");

    return null;
}

/**
 * Example: List events using the service class
 */
async function demonstrateCalendarAccess() {
    console.log("🔐 Authenticating with Google Calendar...\n");

    try {
        // Get authenticated client using saved tokens
        const auth = await authorize();

        if (!auth) {
            console.log(
                "❌ No authentication available. To set up authentication:",
            );
            console.log("");
            console.log("1. Create a GoogleCalendarService instance:");
            console.log(
                "   const service = new GoogleCalendarService(clientId, clientSecret, redirectUri);",
            );
            console.log("");
            console.log("2. Get authorization URL:");
            console.log("   const authUrl = service.getAuthUrl();");
            console.log("   // Redirect user to authUrl");
            console.log("");
            console.log("3. Exchange code for tokens:");
            console.log(
                "   const tokens = await service.getTokensFromCode(authorizationCode);",
            );
            console.log("   // Store tokens.refresh_token in your database");
            console.log("");
            console.log("4. Use stored refresh token:");
            console.log(
                "   const auth = service.createAuthenticatedClient({ refresh_token: refreshToken });",
            );
            console.log("   const events = await service.listEvents(auth);");
            console.log(
                "   const calendars = await service.listCalendars(auth);",
            );
            console.log("");
            console.log(
                "For testing purposes, you can also check if there's a saved token.json file.",
            );
            return;
        }

        // Create calendar service
        const calendar = google.calendar({ version: "v3", auth });

        // List upcoming events
        console.log("📅 Upcoming Events:");
        const eventsRes = await calendar.events.list({
            calendarId: "primary",
            timeMin: new Date().toISOString(),
            maxResults: 10,
            singleEvents: true,
            orderBy: "startTime",
        });

        const events = eventsRes.data.items;
        if (!events || events.length === 0) {
            console.log("  No upcoming events found.\n");
        } else {
            events.forEach((event) => {
                const start =
                    event.start?.dateTime || event.start?.date || "No date";
                console.log(`  ${start} - ${event.summary}`);
            });
            console.log("");
        }

        // List calendars
        console.log("📋 Available Calendars:");
        const calendarsRes = await calendar.calendarList.list();
        const calendars = calendarsRes.data.items;

        if (!calendars || calendars.length === 0) {
            console.log("  No calendars found.\n");
        } else {
            calendars.forEach((cal) => {
                console.log(`  📅 ${cal.summary} (${cal.id})`);
            });
            console.log("");
        }

        console.log("✅ Demo completed successfully!");
    } catch (error) {
        console.error("❌ Error:", error);
    }
}

// Run the demonstration when executed directly (e.g. `bun run src/demo.ts`)
if (import.meta.main) {
    demonstrateCalendarAccess()
        .then(() => process.exit(0))
        .catch((error) => {
            console.error("Fatal error:", error);
            process.exit(1);
        });
}
/* v8 ignore stop */
