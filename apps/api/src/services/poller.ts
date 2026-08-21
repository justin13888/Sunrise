import { GoogleCalendarService } from "@sunrise/gcal";
import { mapGCalEvent } from "../resolvers/mappers";
import { pubsub } from "./pubsub";
import { tokenStore, type UserTokens } from "./tokenStore";

const DEFAULT_INTERVAL_MS = 60 * 1000;
const POLL_WINDOW_DAYS = 30;
const MAX_EVENTS_PER_PAGE = 250;
const MAX_PAGES_PER_CALENDAR = 10;

/** Calendar roles that grant write access — only these calendars are polled. */
const POLLED_ACCESS_ROLES = new Set(["owner", "writer"]);

interface OAuthCredentials {
    access_token?: string | null;
    refresh_token?: string | null;
    expiry_date?: number | null;
}

interface PolledCalendar {
    id?: string | null;
    accessRole?: string | null;
}

interface PolledEvent {
    id?: string | null;
    updated?: string | null;
    end?: {
        dateTime?: string | null;
        date?: string | null;
    };
}

/** Snapshot entry: last seen `updated` stamp plus the event's end time
 *  (ISO-ish string), used to tell real deletions from window slide. */
interface SnapshotEntry {
    updated: string;
    end?: string;
}

/**
 * Minimal structural view of GoogleCalendarService used by the poller.
 * Tests inject a lightweight fake implementing this interface.
 */
export interface CalendarPollingClient {
    createAuthenticatedClient(tokens: OAuthCredentials): unknown;
    listCalendars(auth: unknown): Promise<{
        items: PolledCalendar[];
        credentials?: OAuthCredentials;
    }>;
    listEvents(
        auth: unknown,
        calendarId?: string,
        maxResults?: number,
        pageToken?: string,
        timeMin?: string,
        timeMax?: string,
    ): Promise<{
        items: PolledEvent[];
        nextPageToken?: string | null;
        credentials?: OAuthCredentials;
    }>;
}

export interface PollingServiceOptions {
    /** Injectable calendar client (tests); defaults to GoogleCalendarService. */
    calendarService?: CalendarPollingClient;
    /** Poll interval in milliseconds (default 60s). */
    intervalMs?: number;
}

/**
 * Real-time calendar sync: periodically polls each connected user's writable
 * Google calendars, diffs the upcoming-events window against an in-memory
 * snapshot, and publishes eventCreated / eventUpdated / eventDeleted on the
 * pubsub bus. The first observation of a user/calendar seeds the snapshot
 * silently (no publishes).
 */
export class PollingService {
    private isRunning = false;
    /** True while a pollOnce cycle is executing (overlap guard). */
    private polling = false;
    private timer: ReturnType<typeof setInterval> | null = null;
    private gcalService: CalendarPollingClient;
    private intervalMs: number;
    /** `${userId}::${calendarId}` → (eventId → snapshot entry) */
    private snapshots = new Map<string, Map<string, SnapshotEntry>>();

    constructor(
        clientId: string,
        clientSecret: string,
        redirectUri?: string,
        options: PollingServiceOptions = {},
    ) {
        this.gcalService =
            options.calendarService ??
            new GoogleCalendarService(clientId, clientSecret, redirectUri);
        this.intervalMs = options.intervalMs ?? DEFAULT_INTERVAL_MS;
    }

    start() {
        if (this.isRunning) return;
        this.isRunning = true;
        console.log("⏰ Starting calendar polling service...");

        // Poll immediately, then on an interval
        void this.pollOnce();
        this.timer = setInterval(() => {
            void this.pollOnce();
        }, this.intervalMs);
    }

    stop() {
        if (this.timer) {
            clearInterval(this.timer);
            this.timer = null;
        }
        this.isRunning = false;
        console.log("🛑 Stopping calendar polling service...");
    }

    /**
     * Runs a single poll cycle across all connected users.
     * Errors are contained per user so one failure never breaks the loop.
     * If a previous cycle is still in flight, the call is skipped.
     */
    async pollOnce(): Promise<void> {
        if (this.polling) return;
        this.polling = true;
        try {
            let userIds: string[];
            try {
                userIds = await tokenStore.getAllUserIds();
            } catch (error) {
                console.error("❌ Polling loop error:", error);
                return;
            }

            this.pruneSnapshots(userIds);

            for (const userId of userIds) {
                try {
                    await this.pollUser(userId);
                } catch (error) {
                    console.error(
                        `❌ Error polling for user ${userId}:`,
                        error,
                    );
                }
            }
        } finally {
            this.polling = false;
        }
    }

    /** Drops snapshots of users that are no longer connected, so a user who
     *  logs back in later reseeds instead of flooding stale diffs. */
    private pruneSnapshots(userIds: string[]) {
        const active = new Set(userIds);
        for (const key of this.snapshots.keys()) {
            const userId = key.slice(0, key.indexOf("::"));
            if (!active.has(userId)) {
                this.snapshots.delete(key);
            }
        }
    }

    private async pollUser(userId: string) {
        const tokens = await tokenStore.getTokens(userId);
        if (!tokens) return;

        const auth = this.gcalService.createAuthenticatedClient({
            access_token: tokens.accessToken,
            refresh_token: tokens.refreshToken,
            expiry_date: tokens.expiresAt.getTime(),
        });

        const calendarList = await this.gcalService.listCalendars(auth);
        let latestCredentials = calendarList.credentials;

        for (const calendar of calendarList.items) {
            const calendarId = calendar.id;
            if (!calendarId) continue;
            if (!POLLED_ACCESS_ROLES.has(calendar.accessRole ?? "")) continue;

            try {
                const credentials = await this.pollCalendar(
                    userId,
                    auth,
                    calendarId,
                );
                latestCredentials = credentials ?? latestCredentials;
            } catch (error) {
                console.error(
                    `❌ Error polling calendar ${calendarId} for user ${userId}:`,
                    error,
                );
            }
        }

        if (latestCredentials) {
            await this.maybePersistRefreshedTokens(
                userId,
                tokens,
                latestCredentials,
            );
        }
    }

    /**
     * Lists the upcoming-events window for one calendar, diffs it against the
     * previous snapshot, and publishes the resulting change events.
     */
    private async pollCalendar(
        userId: string,
        auth: unknown,
        calendarId: string,
    ): Promise<OAuthCredentials | undefined> {
        const timeMin = new Date();
        const timeMax = new Date(
            timeMin.getTime() + POLL_WINDOW_DAYS * 24 * 60 * 60 * 1000,
        );

        const events: PolledEvent[] = [];
        let credentials: OAuthCredentials | undefined;
        let pageToken: string | undefined;
        // True when the page cap was hit with more pages remaining: the
        // listing is incomplete, so the deletion diff is skipped this cycle.
        let pageCapHit = false;
        for (let page = 0; page < MAX_PAGES_PER_CALENDAR; page++) {
            const result = await this.gcalService.listEvents(
                auth,
                calendarId,
                MAX_EVENTS_PER_PAGE,
                pageToken,
                timeMin.toISOString(),
                timeMax.toISOString(),
            );
            events.push(...result.items);
            credentials = result.credentials ?? credentials;
            if (!result.nextPageToken) break;
            pageToken = result.nextPageToken;
            if (page === MAX_PAGES_PER_CALENDAR - 1) pageCapHit = true;
        }

        const current = new Map<string, PolledEvent>();
        for (const event of events) {
            if (event.id) current.set(event.id, event);
        }

        const key = `${userId}::${calendarId}`;
        const previous = this.snapshots.get(key);

        const snapshot = new Map<string, SnapshotEntry>();
        for (const [id, event] of current) {
            snapshot.set(id, {
                updated: event.updated ?? "",
                end: event.end?.dateTime ?? event.end?.date ?? undefined,
            });
        }
        this.snapshots.set(key, snapshot);

        // First observation of this user/calendar: seed silently.
        if (!previous) return credentials;

        for (const [id, event] of current) {
            if (!previous.has(id)) {
                pubsub.publish("eventCreated", {
                    userId,
                    calendarId,
                    event: mapGCalEvent(event, calendarId),
                });
            } else if (previous.get(id)?.updated !== (event.updated ?? "")) {
                pubsub.publish("eventUpdated", {
                    userId,
                    calendarId,
                    event: mapGCalEvent(event, calendarId),
                });
            }
        }

        // With an incomplete listing we cannot tell "deleted" from "not
        // fetched"; skip the deletion diff entirely for this cycle.
        if (pageCapHit) return credentials;

        const timeMinIso = timeMin.toISOString();
        for (const [id, entry] of previous) {
            if (current.has(id)) continue;
            // An event whose recorded end time is at or before the current
            // window start simply slid out of the window — not a deletion.
            if (entry.end !== undefined && entry.end <= timeMinIso) continue;
            pubsub.publish("eventDeleted", {
                userId,
                calendarId,
                payload: { id, calendarId },
            });
        }

        return credentials;
    }

    /** Persists refreshed Google credentials if they changed during polling. */
    private async maybePersistRefreshedTokens(
        userId: string,
        tokens: UserTokens,
        credentials: OAuthCredentials,
    ) {
        const changed =
            (credentials.access_token &&
                credentials.access_token !== tokens.accessToken) ||
            (credentials.expiry_date &&
                credentials.expiry_date !== tokens.expiresAt.getTime());
        if (!changed) return;

        console.log(`🔄 Refreshing tokens for user ${userId}`);
        await tokenStore.storeTokens(userId, {
            ...tokens,
            accessToken: credentials.access_token || tokens.accessToken,
            refreshToken: credentials.refresh_token || tokens.refreshToken,
            expiresAt: new Date(
                credentials.expiry_date || tokens.expiresAt.getTime(),
            ),
        });
    }
}
