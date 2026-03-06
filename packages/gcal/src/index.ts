import * as fs from "node:fs/promises";
import * as path from "node:path";
import * as process from "node:process";
import { OAuth2Client } from "google-auth-library";
import { type calendar_v3, google } from "googleapis";

// If modifying these scopes, delete token.json.
const SCOPES = [
    "https://www.googleapis.com/auth/calendar.readonly",
    "https://www.googleapis.com/auth/calendar.events",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
];

const CREDENTIALS_PATH = path.join(process.cwd(), "credentials.json");
const TOKEN_PATH = path.join(process.cwd(), "token.json");

/**
 * Google Calendar Service - API-ready implementation
 */
export class GoogleCalendarService {
    private oauth2Client: OAuth2Client;

    constructor(
        private clientId: string,
        private clientSecret: string,
        private redirectUri: string = "urn:ietf:wg:oauth:2.0:oob",
    ) {
        this.oauth2Client = new google.auth.OAuth2({
            clientId,
            clientSecret,
            redirectUri,
        });
    }

    /**
     * Create an authenticated OAuth2 client from token information
     */
    createAuthenticatedClient(tokens: {
        access_token?: string | null;
        refresh_token?: string | null;
        expiry_date?: number | null;
    }): OAuth2Client {
        const client = new google.auth.OAuth2(
            this.clientId,
            this.clientSecret,
            this.redirectUri,
        );

        client.setCredentials({
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expiry_date: tokens.expiry_date,
            token_type: "Bearer",
        });

        return client;
    }

    /**
     * Generate the authorization URL to redirect users to
     */
    getAuthUrl(state?: string): string {
        return this.oauth2Client.generateAuthUrl({
            access_type: "offline",
            prompt: "consent",
            scope: SCOPES,
            state: state || `timestamp_${Date.now()}`,
        });
    }

    /**
     * Exchange authorization code for tokens
     */
    async getTokensFromCode(code: string) {
        const { tokens } = await this.oauth2Client.getToken(code);
        return tokens;
    }

    /**
     * Get calendar service instance
     */
    private getCalendarService(auth: OAuth2Client): calendar_v3.Calendar {
        return google.calendar({ version: "v3", auth });
    }

    /**
     * Lists the next N events on the user's calendar with pagination support
     */
    async listEvents(
        auth: OAuth2Client,
        calendarId: string = "primary",
        maxResults: number = 20,
        pageToken?: string,
        timeMin?: string,
        timeMax?: string,
        orderBy: "startTime" | "updated" = "startTime",
    ) {
        const calendar = this.getCalendarService(auth);

        const res = await calendar.events.list({
            calendarId,
            timeMin: timeMin || new Date().toISOString(),
            timeMax,
            maxResults,
            singleEvents: true,
            orderBy,
            pageToken,
        });

        // Check if tokens were refreshed during the request
        // The OAuth2Client automatically refreshes tokens if the refresh_token is present
        // We can inspect client.credentials to see if they changed, simplified here.
        // real persistence sync should happen if credentials change.

        return {
            items: res.data.items || [],
            nextPageToken: res.data.nextPageToken,
            // Return credentials so caller can persist updates if any
            credentials: auth.credentials,
        };
    }

    /**
     * Lists all calendars the user has access to
     */
    async listCalendars(auth: OAuth2Client) {
        const calendar = this.getCalendarService(auth);
        const res = await calendar.calendarList.list();
        return {
            items: res.data.items || [],
            credentials: auth.credentials,
        };
    }
}

/**
 * Legacy function for backward compatibility - Deprecated
 */
export async function getClientFromRefreshToken(
    refreshToken: string,
): Promise<OAuth2Client> {
    const client = new google.auth.OAuth2(
        process.env.GOOGLE_CLIENT_ID,
        process.env.GOOGLE_CLIENT_SECRET,
    );
    client.setCredentials({ refresh_token: refreshToken });
    return client;
}

/* v8 ignore start */
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
async function _saveCredentials(
    client: OAuth2Client,
    // client: Omit<Omit<OAuth2Client, 'fetch'>, 'addUserProjectAndAuthHeaders'>
): Promise<void> {
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
                "   const events = await service.listEvents(refreshToken);",
            );
            console.log(
                "   const calendars = await service.listCalendars(refreshToken);",
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

// Run the demonstration
if (require.main === module) {
    demonstrateCalendarAccess()
        .then(() => process.exit(0))
        .catch((error) => {
            console.error("Fatal error:", error);
            process.exit(1);
        });
}
/* v8 ignore stop */
