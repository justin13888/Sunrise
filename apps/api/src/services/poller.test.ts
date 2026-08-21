import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { CalendarPollingClient } from "./poller";

// Keep the test hermetic: never load googleapis or the real db.
vi.mock("@sunrise/gcal", () => ({
    GoogleCalendarService: class {},
}));

vi.mock("./pubsub", () => ({
    pubsub: {
        publish: vi.fn(),
    },
}));

vi.mock("./tokenStore", () => ({
    tokenStore: {
        getAllUserIds: vi.fn(),
        getTokens: vi.fn(),
        storeTokens: vi.fn(),
        removeTokens: vi.fn(),
    },
}));

// Mapper is owned by the resolvers layer; the poller only forwards events
// through it, so a passthrough stub is sufficient here.
vi.mock("../resolvers/mappers", () => ({
    mapGCalEvent: vi.fn(
        (event: { id?: string | null }, calendarId: string) => ({
            id: event.id,
            calendarId,
            mapped: true,
        }),
    ),
}));

import { mapGCalEvent } from "../resolvers/mappers";
import { PollingService } from "./poller";
import { pubsub } from "./pubsub";
import { tokenStore, type UserTokens } from "./tokenStore";

const mockedTokenStore = vi.mocked(tokenStore);
const mockedPublish = vi.mocked(pubsub.publish);

interface FakeEvent {
    id?: string | null;
    updated?: string | null;
    summary?: string;
    end?: { dateTime?: string | null; date?: string | null };
}

interface FakeCalendar {
    accessRole: string;
    events: FakeEvent[];
    /** When set, listEvents pages through these instead of `events`. */
    pages?: FakeEvent[][];
    /** When set, listEvents reports these credentials back to the poller. */
    credentials?: OAuthTokensArg;
}

/** userId → Error (whole user fails) or calendarId → calendar state */
type FakeState = Record<string, Error | Record<string, FakeCalendar>>;

function createFakeCalendarService(state: FakeState) {
    const getUserState = (auth: unknown) => {
        const { user } = auth as { user: string };
        const userState = state[user];
        if (!userState) throw new Error(`no fake state for user ${user}`);
        if (userState instanceof Error) throw userState;
        return userState;
    };

    return {
        createAuthenticatedClient: vi.fn((tokens: OAuthTokensArg) => ({
            user: (tokens.refresh_token ?? "").replace("refresh-", ""),
        })),
        listCalendars: vi.fn(async (auth: unknown) => {
            const userState = getUserState(auth);
            return {
                items: Object.entries(userState).map(([id, cal]) => ({
                    id,
                    accessRole: cal.accessRole,
                })),
            };
        }),
        listEvents: vi.fn(
            async (
                auth: unknown,
                calendarId?: string,
                _maxResults?: number,
                pageToken?: string,
            ) => {
                const userState = getUserState(auth);
                const calendar = userState[calendarId ?? "primary"];
                if (!calendar) {
                    throw new Error(`unknown calendar ${calendarId}`);
                }
                if (calendar.pages) {
                    const index = pageToken ? Number(pageToken) : 0;
                    const items = (calendar.pages[index] ?? []).map(
                        (event) => ({ ...event }),
                    );
                    const nextPageToken =
                        index + 1 < calendar.pages.length
                            ? String(index + 1)
                            : undefined;
                    return {
                        items,
                        nextPageToken,
                        credentials: calendar.credentials,
                    };
                }
                return {
                    items: calendar.events.map((event) => ({ ...event })),
                    credentials: calendar.credentials,
                };
            },
        ),
    } satisfies CalendarPollingClient;
}

interface OAuthTokensArg {
    access_token?: string | null;
    refresh_token?: string | null;
    expiry_date?: number | null;
}

function makeTokens(userId: string): UserTokens {
    return {
        userId,
        accessToken: `access-${userId}`,
        refreshToken: `refresh-${userId}`,
        expiresAt: new Date(Date.now() + 3600 * 1000),
        email: `${userId}@example.com`,
    };
}

function createService(state: FakeState, intervalMs = 1000) {
    const fake = createFakeCalendarService(state);
    const service = new PollingService(
        "client-id",
        "client-secret",
        undefined,
        {
            calendarService: fake,
            intervalMs,
        },
    );
    return { service, fake };
}

describe("PollingService", () => {
    beforeEach(() => {
        vi.clearAllMocks();
        vi.spyOn(console, "log").mockImplementation(() => {});
        vi.spyOn(console, "error").mockImplementation(() => {});
        mockedTokenStore.getTokens.mockImplementation(async (userId) =>
            makeTokens(userId),
        );
    });

    afterEach(() => {
        vi.restoreAllMocks();
        vi.useRealTimers();
    });

    it("publishes nothing on the first (seed) cycle", async () => {
        mockedTokenStore.getAllUserIds.mockResolvedValue(["alice"]);
        const { service } = createService({
            alice: {
                "cal-1": {
                    accessRole: "owner",
                    events: [
                        { id: "e1", updated: "2026-08-15T10:00:00Z" },
                        { id: "e2", updated: "2026-08-15T11:00:00Z" },
                    ],
                },
            },
        });

        await service.pollOnce();

        expect(mockedPublish).not.toHaveBeenCalled();
    });

    it("publishes eventCreated for events added after the seed cycle", async () => {
        mockedTokenStore.getAllUserIds.mockResolvedValue(["alice"]);
        const state: FakeState = {
            alice: {
                "cal-1": {
                    accessRole: "owner",
                    events: [{ id: "e1", updated: "2026-08-15T10:00:00Z" }],
                },
            },
        };
        const { service } = createService(state);

        await service.pollOnce();
        (state.alice as Record<string, FakeCalendar>)["cal-1"].events.push({
            id: "e2",
            updated: "2026-08-15T12:00:00Z",
            summary: "New event",
        });
        await service.pollOnce();

        expect(mockedPublish).toHaveBeenCalledTimes(1);
        expect(mockedPublish).toHaveBeenCalledWith("eventCreated", {
            userId: "alice",
            calendarId: "cal-1",
            event: { id: "e2", calendarId: "cal-1", mapped: true },
        });
        expect(mapGCalEvent).toHaveBeenCalledWith(
            expect.objectContaining({ id: "e2", summary: "New event" }),
            "cal-1",
        );
    });

    it("publishes eventUpdated when an event's updated timestamp changes", async () => {
        mockedTokenStore.getAllUserIds.mockResolvedValue(["alice"]);
        const state: FakeState = {
            alice: {
                "cal-1": {
                    accessRole: "writer",
                    events: [{ id: "e1", updated: "2026-08-15T10:00:00Z" }],
                },
            },
        };
        const { service } = createService(state);

        await service.pollOnce();
        (state.alice as Record<string, FakeCalendar>)[
            "cal-1"
        ].events[0].updated = "2026-08-15T13:37:00Z";
        await service.pollOnce();

        expect(mockedPublish).toHaveBeenCalledTimes(1);
        expect(mockedPublish).toHaveBeenCalledWith("eventUpdated", {
            userId: "alice",
            calendarId: "cal-1",
            event: { id: "e1", calendarId: "cal-1", mapped: true },
        });
    });

    it("publishes eventDeleted when an event disappears", async () => {
        mockedTokenStore.getAllUserIds.mockResolvedValue(["alice"]);
        const state: FakeState = {
            alice: {
                "cal-1": {
                    accessRole: "owner",
                    events: [
                        { id: "e1", updated: "2026-08-15T10:00:00Z" },
                        { id: "e2", updated: "2026-08-15T11:00:00Z" },
                    ],
                },
            },
        };
        const { service } = createService(state);

        await service.pollOnce();
        (state.alice as Record<string, FakeCalendar>)["cal-1"].events = [
            { id: "e2", updated: "2026-08-15T11:00:00Z" },
        ];
        await service.pollOnce();

        expect(mockedPublish).toHaveBeenCalledTimes(1);
        expect(mockedPublish).toHaveBeenCalledWith("eventDeleted", {
            userId: "alice",
            calendarId: "cal-1",
            payload: { id: "e1", calendarId: "cal-1" },
        });
    });

    it("publishes no duplicate events for unchanged cycles", async () => {
        mockedTokenStore.getAllUserIds.mockResolvedValue(["alice"]);
        const { service } = createService({
            alice: {
                "cal-1": {
                    accessRole: "owner",
                    events: [{ id: "e1", updated: "2026-08-15T10:00:00Z" }],
                },
            },
        });

        await service.pollOnce();
        await service.pollOnce();
        await service.pollOnce();

        expect(mockedPublish).not.toHaveBeenCalled();
    });

    it("only polls calendars where the user is owner or writer", async () => {
        mockedTokenStore.getAllUserIds.mockResolvedValue(["alice"]);
        const state: FakeState = {
            alice: {
                "cal-owned": {
                    accessRole: "owner",
                    events: [{ id: "e1", updated: "t1" }],
                },
                "cal-shared": {
                    accessRole: "reader",
                    events: [{ id: "r1", updated: "t1" }],
                },
                "cal-busy": {
                    accessRole: "freeBusyReader",
                    events: [{ id: "f1", updated: "t1" }],
                },
            },
        };
        const { service, fake } = createService(state);

        await service.pollOnce();

        const polledCalendars = fake.listEvents.mock.calls.map(
            (call) => call[1],
        );
        expect(polledCalendars).toEqual(["cal-owned"]);
    });

    it("skips users without stored tokens", async () => {
        mockedTokenStore.getAllUserIds.mockResolvedValue(["alice"]);
        mockedTokenStore.getTokens.mockResolvedValue(null);
        const { service, fake } = createService({});

        await service.pollOnce();

        expect(fake.listCalendars).not.toHaveBeenCalled();
        expect(mockedPublish).not.toHaveBeenCalled();
    });

    it("continues polling other users when one user errors", async () => {
        mockedTokenStore.getAllUserIds.mockResolvedValue(["broken", "alice"]);
        const state: FakeState = {
            broken: new Error("google is down for this user"),
            alice: {
                "cal-1": {
                    accessRole: "owner",
                    events: [{ id: "e1", updated: "t1" }],
                },
            },
        };
        const { service } = createService(state);

        await service.pollOnce(); // seed for alice; broken errors
        (state.alice as Record<string, FakeCalendar>)["cal-1"].events.push({
            id: "e2",
            updated: "t2",
        });
        await service.pollOnce();

        expect(mockedPublish).toHaveBeenCalledTimes(1);
        expect(mockedPublish).toHaveBeenCalledWith("eventCreated", {
            userId: "alice",
            calendarId: "cal-1",
            event: { id: "e2", calendarId: "cal-1", mapped: true },
        });
        expect(console.error).toHaveBeenCalled();
    });

    it("continues with other calendars when one calendar errors", async () => {
        mockedTokenStore.getAllUserIds.mockResolvedValue(["alice"]);
        const state: FakeState = {
            alice: {
                "cal-ok": {
                    accessRole: "owner",
                    events: [{ id: "e1", updated: "t1" }],
                },
            },
        };
        const { service, fake } = createService(state);
        // First listEvents call (cal-broken) fails, later calls use the fake.
        fake.listCalendars.mockResolvedValue({
            items: [
                { id: "cal-broken", accessRole: "owner" },
                { id: "cal-ok", accessRole: "owner" },
            ],
        });

        await service.pollOnce(); // seeds cal-ok; cal-broken errors each cycle
        (state.alice as Record<string, FakeCalendar>)["cal-ok"].events.push({
            id: "e2",
            updated: "t2",
        });
        await service.pollOnce();

        expect(mockedPublish).toHaveBeenCalledTimes(1);
        expect(mockedPublish).toHaveBeenCalledWith("eventCreated", {
            userId: "alice",
            calendarId: "cal-ok",
            event: { id: "e2", calendarId: "cal-ok", mapped: true },
        });
    });

    it("reseeds (stays silent) after a user logs out and back in", async () => {
        mockedTokenStore.getAllUserIds.mockResolvedValue(["alice"]);
        const state: FakeState = {
            alice: {
                "cal-1": {
                    accessRole: "owner",
                    events: [{ id: "e1", updated: "t1" }],
                },
            },
        };
        const { service } = createService(state);

        await service.pollOnce(); // seed
        mockedTokenStore.getAllUserIds.mockResolvedValue([]); // logged out
        await service.pollOnce();
        (state.alice as Record<string, FakeCalendar>)["cal-1"].events = [
            { id: "e2", updated: "t2" },
        ];
        mockedTokenStore.getAllUserIds.mockResolvedValue(["alice"]); // back in
        await service.pollOnce(); // must reseed silently

        expect(mockedPublish).not.toHaveBeenCalled();
    });

    it("follows nextPageToken across pages and diffs the combined listing", async () => {
        mockedTokenStore.getAllUserIds.mockResolvedValue(["alice"]);
        const state: FakeState = {
            alice: {
                "cal-1": {
                    accessRole: "owner",
                    events: [],
                    pages: [
                        [{ id: "e1", updated: "t1" }],
                        [{ id: "e2", updated: "t1" }],
                        [{ id: "e3", updated: "t1" }],
                    ],
                },
            },
        };
        const { service, fake } = createService(state);

        await service.pollOnce(); // seed

        expect(fake.listEvents).toHaveBeenCalledTimes(3);
        expect(fake.listEvents.mock.calls.map((call) => call[3])).toEqual([
            undefined,
            "1",
            "2",
        ]);

        // An event on a later page disappears: detected across all pages.
        (state.alice as Record<string, FakeCalendar>)["cal-1"].pages = [
            [{ id: "e1", updated: "t1" }],
            [{ id: "e3", updated: "t1" }],
        ];
        await service.pollOnce();

        expect(mockedPublish).toHaveBeenCalledTimes(1);
        expect(mockedPublish).toHaveBeenCalledWith("eventDeleted", {
            userId: "alice",
            calendarId: "cal-1",
            payload: { id: "e2", calendarId: "cal-1" },
        });
    });

    it("skips the deletion diff when the page cap is hit", async () => {
        mockedTokenStore.getAllUserIds.mockResolvedValue(["alice"]);
        // 11 pages of one event each: the poller caps at 10 pages, so the
        // listing is incomplete and deletions must not be diffed.
        const makePages = () =>
            Array.from({ length: 11 }, (_, i) => [
                { id: `e${i}`, updated: "t1" },
            ]);
        const state: FakeState = {
            alice: {
                "cal-1": {
                    accessRole: "owner",
                    events: [],
                    pages: makePages(),
                },
            },
        };
        const { service } = createService(state);

        await service.pollOnce(); // seed (capped)

        // e5 disappears while the listing is still capped.
        const pages = makePages();
        pages[5] = [];
        (state.alice as Record<string, FakeCalendar>)["cal-1"].pages = pages;
        await service.pollOnce();

        expect(mockedPublish).not.toHaveBeenCalledWith(
            "eventDeleted",
            expect.anything(),
        );
    });

    it("does not publish eventDeleted for events that slid out of the window, but does for removed future events", async () => {
        mockedTokenStore.getAllUserIds.mockResolvedValue(["alice"]);
        const pastEnd = new Date(Date.now() - 60 * 60 * 1000).toISOString();
        const futureEnd = new Date(Date.now() + 60 * 60 * 1000).toISOString();
        const state: FakeState = {
            alice: {
                "cal-1": {
                    accessRole: "owner",
                    events: [
                        {
                            id: "e-past",
                            updated: "t1",
                            end: { dateTime: pastEnd },
                        },
                        {
                            id: "e-future",
                            updated: "t1",
                            end: { dateTime: futureEnd },
                        },
                    ],
                },
            },
        };
        const { service } = createService(state);

        await service.pollOnce(); // seed
        (state.alice as Record<string, FakeCalendar>)["cal-1"].events = [];
        await service.pollOnce();

        expect(mockedPublish).toHaveBeenCalledTimes(1);
        expect(mockedPublish).toHaveBeenCalledWith("eventDeleted", {
            userId: "alice",
            calendarId: "cal-1",
            payload: { id: "e-future", calendarId: "cal-1" },
        });
    });

    it("persists refreshed credentials when the access token changed", async () => {
        mockedTokenStore.getAllUserIds.mockResolvedValue(["alice"]);
        const { service } = createService({
            alice: {
                "cal-1": {
                    accessRole: "owner",
                    events: [{ id: "e1", updated: "t1" }],
                    credentials: { access_token: "new-access-token" },
                },
            },
        });

        await service.pollOnce();

        expect(mockedTokenStore.storeTokens).toHaveBeenCalledWith(
            "alice",
            expect.objectContaining({ accessToken: "new-access-token" }),
        );
    });

    it("does not persist credentials when they are unchanged", async () => {
        mockedTokenStore.getAllUserIds.mockResolvedValue(["alice"]);
        const { service } = createService({
            alice: {
                "cal-1": {
                    accessRole: "owner",
                    events: [{ id: "e1", updated: "t1" }],
                    credentials: { access_token: "access-alice" },
                },
            },
        });

        await service.pollOnce();

        expect(mockedTokenStore.storeTokens).not.toHaveBeenCalled();
    });

    it("skips interval ticks while a previous cycle is still in flight", async () => {
        vi.useFakeTimers();
        let release!: (userIds: string[]) => void;
        mockedTokenStore.getAllUserIds.mockReturnValue(
            new Promise<string[]>((resolve) => {
                release = resolve;
            }),
        );
        const { service } = createService({}, 1000);

        service.start(); // first cycle starts and hangs on getAllUserIds
        vi.advanceTimersByTime(3000); // several interval ticks fire meanwhile

        expect(mockedTokenStore.getAllUserIds).toHaveBeenCalledTimes(1);

        release([]);
        await vi.advanceTimersByTimeAsync(0); // let the hung cycle finish
        mockedTokenStore.getAllUserIds.mockResolvedValue([]);
        await vi.advanceTimersByTimeAsync(1000); // next tick runs again

        expect(mockedTokenStore.getAllUserIds).toHaveBeenCalledTimes(2);
        service.stop();
    });

    it("start() polls immediately and on the interval; stop() halts it", async () => {
        vi.useFakeTimers();
        mockedTokenStore.getAllUserIds.mockResolvedValue([]);
        const { service } = createService({}, 1000);
        const pollSpy = vi
            .spyOn(service, "pollOnce")
            .mockResolvedValue(undefined);

        service.start();
        expect(pollSpy).toHaveBeenCalledTimes(1);

        vi.advanceTimersByTime(2000);
        expect(pollSpy).toHaveBeenCalledTimes(3);

        service.stop();
        vi.advanceTimersByTime(5000);
        expect(pollSpy).toHaveBeenCalledTimes(3);
    });
});
