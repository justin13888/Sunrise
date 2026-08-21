import type { OAuth2Client } from "google-auth-library";
import { type calendar_v3, google } from "googleapis";

export type { OAuth2Client } from "google-auth-library";
export type { calendar_v3 } from "googleapis";

// If modifying these scopes, users must re-consent.
const SCOPES = [
    "https://www.googleapis.com/auth/calendar.readonly",
    "https://www.googleapis.com/auth/calendar.events",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
];

/**
 * Basic Google account profile information.
 */
export interface GoogleUserInfo {
    email: string;
    name?: string;
    picture?: string;
}

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
     * Fetch the authenticated user's Google account profile.
     * Throws if Google does not return an email address.
     */
    async getUserInfo(auth: OAuth2Client): Promise<GoogleUserInfo> {
        const oauth2 = google.oauth2({ version: "v2", auth });
        const res = await oauth2.userinfo.get();
        const email = res.data?.email;
        if (!email) {
            throw new Error(
                "Failed to get user info from Google - no email in response",
            );
        }
        return {
            email,
            name: res.data.name ?? undefined,
            picture: res.data.picture ?? undefined,
        };
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
            // No default: callers decide the window. The API's "upcoming by
            // default" policy lives in the resolvers, not here.
            timeMin,
            timeMax,
            maxResults,
            singleEvents: true,
            orderBy,
            pageToken,
        });

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

    /**
     * Fetches a single event from the specified calendar
     */
    async getEvent(
        auth: OAuth2Client,
        calendarId: string,
        eventId: string,
    ): Promise<calendar_v3.Schema$Event> {
        const calendar = this.getCalendarService(auth);
        const res = await calendar.events.get({ calendarId, eventId });
        return res.data;
    }

    /**
     * Creates a new event on the specified calendar
     */
    async createEvent(
        auth: OAuth2Client,
        calendarId: string,
        event: calendar_v3.Schema$Event,
    ): Promise<calendar_v3.Schema$Event> {
        const calendar = this.getCalendarService(auth);
        const res = await calendar.events.insert({
            calendarId,
            requestBody: event,
        });
        return res.data;
    }

    /**
     * Partially updates an existing event on the specified calendar.
     * Uses PATCH semantics: only the provided fields are modified.
     */
    async updateEvent(
        auth: OAuth2Client,
        calendarId: string,
        eventId: string,
        event: calendar_v3.Schema$Event,
    ): Promise<calendar_v3.Schema$Event> {
        const calendar = this.getCalendarService(auth);
        const res = await calendar.events.patch({
            calendarId,
            eventId,
            requestBody: event,
        });
        return res.data;
    }

    /**
     * Deletes an event from the specified calendar
     */
    async deleteEvent(
        auth: OAuth2Client,
        calendarId: string,
        eventId: string,
    ): Promise<void> {
        const calendar = this.getCalendarService(auth);
        await calendar.events.delete({ calendarId, eventId });
    }
}
