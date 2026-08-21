/** biome-ignore-all lint/suspicious/noExplicitAny: test stubs */
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { makeExecutableSchema } from "@graphql-tools/schema";
import { beforeEach, describe, expect, it, vi } from "vitest";

// Load graphql through CJS so the executor shares a realm with the schema
// built by @graphql-tools/schema (which vitest externalizes to the CJS build);
// the vite-resolved ESM copy would trip graphql's duplicate-module guard.
const cjsRequire = createRequire(import.meta.url);
const {
    graphql,
    parse,
    subscribe: gqlSubscribe,
} = cjsRequire("graphql") as typeof import("graphql");

// Mock DB-backed services BEFORE importing the resolvers so nothing touches
// bun:sqlite / the database when this suite runs under Node.
// The mock re-exports its own RoutineValidationError class; since the
// resolvers import the class from the same (mocked) module, instanceof
// checks line up.
vi.mock("../services/routineService", () => {
    class RoutineValidationError extends Error {
        constructor(message: string) {
            super(message);
            this.name = "RoutineValidationError";
        }
    }
    return {
        RoutineValidationError,
        routineService: {
            listRoutines: vi.fn(),
            getRoutine: vi.fn(),
            createRoutine: vi.fn(),
            updateRoutine: vi.fn(),
            deleteRoutine: vi.fn(),
        },
    };
});

vi.mock("../services/tokenStore", () => ({
    tokenStore: {
        storeTokens: vi.fn().mockResolvedValue(undefined),
        getTokens: vi.fn(),
        removeTokens: vi.fn().mockResolvedValue(undefined),
    },
}));

// Real pubsub semantics behind spy-able fns, so subscription resolvers can
// be exercised end-to-end while mutations' publish calls stay assertable.
vi.mock("../services/pubsub", async () => {
    const { createPubSub } = await import("graphql-yoga");
    const real = createPubSub();
    return {
        pubsub: {
            publish: vi.fn((topic: string, payload: unknown) =>
                real.publish(topic, payload),
            ),
            subscribe: vi.fn((topic: string) => real.subscribe(topic)),
        },
    };
});

import { pubsub } from "../services/pubsub";
import {
    RoutineValidationError,
    routineService,
} from "../services/routineService";
import { tokenStore } from "../services/tokenStore";
import { resolvers } from "./index";

const typeDefs = readFileSync(
    fileURLToPath(new URL("../schema.graphql", import.meta.url)),
    "utf8",
);

const schema = makeExecutableSchema({ typeDefs, resolvers: resolvers as any });

const sampleEvent = {
    id: "ev-1",
    summary: "Team Sync",
    description: "Weekly sync",
    start: { dateTime: "2026-01-05T10:00:00Z" },
    end: { dateTime: "2026-01-05T11:00:00Z" },
    status: "confirmed",
    recurringEventId: "recurring-1",
    originalStartTime: { dateTime: "2026-01-05T09:00:00Z" },
    htmlLink: "https://calendar.google.com/event?eid=ev-1",
    created: "2025-12-01T00:00:00Z",
    updated: "2025-12-02T00:00:00Z",
};

function makeCalendarService(overrides: Record<string, any> = {}) {
    return {
        getAuthUrl: vi.fn().mockReturnValue("https://accounts.google.com/auth"),
        createAuthenticatedClient: vi.fn().mockReturnValue({ fake: true }),
        getTokensFromCode: vi.fn(),
        getUserInfo: vi.fn(),
        listCalendars: vi.fn().mockResolvedValue({ items: [] }),
        listEvents: vi
            .fn()
            .mockResolvedValue({ items: [], nextPageToken: undefined }),
        getEvent: vi.fn().mockResolvedValue(sampleEvent),
        createEvent: vi.fn().mockResolvedValue(sampleEvent),
        updateEvent: vi.fn().mockResolvedValue(sampleEvent),
        deleteEvent: vi.fn().mockResolvedValue(undefined),
        ...overrides,
    };
}

function makeContext(overrides: Record<string, any> = {}) {
    return {
        user: {
            id: "user-1",
            email: "user@example.com",
            name: "Test User",
            verified: true,
        },
        refreshToken: "refresh-token",
        calendarService: makeCalendarService(),
        ...overrides,
    };
}

async function execute(
    source: string,
    contextValue: Record<string, any>,
    variableValues?: Record<string, unknown>,
) {
    return graphql({ schema, source, contextValue, variableValues });
}

beforeEach(() => {
    vi.clearAllMocks();
});

describe("Query.calendars", () => {
    it("rejects unauthenticated requests with UNAUTHENTICATED", async () => {
        const result = await execute("query { calendars { id summary } }", {
            calendarService: makeCalendarService(),
        });

        expect(result.data).toBeNull();
        expect(result.errors?.[0].extensions?.code).toBe("UNAUTHENTICATED");
    });

    it("returns mapped calendars for authenticated users", async () => {
        const context = makeContext();
        context.calendarService.listCalendars.mockResolvedValue({
            items: [
                {
                    id: "cal-1",
                    summary: "Work",
                    primary: true,
                    accessRole: "owner",
                },
            ],
        });

        const result = await execute(
            "query { calendars { id summary primary accessRole } }",
            context,
        );

        expect(result.errors).toBeUndefined();
        expect(result.data?.calendars).toEqual([
            {
                id: "cal-1",
                summary: "Work",
                primary: true,
                accessRole: "OWNER",
            },
        ]);
    });
});

describe("Query.events", () => {
    const query = /* GraphQL */ `
        query Events($calendarId: ID, $timeMin: DateTime) {
            events(calendarId: $calendarId, first: 2, timeMin: $timeMin) {
                edges {
                    cursor
                    node {
                        id
                        calendarId
                        summary
                        recurringEventId
                        originalStartTime {
                            dateTime
                        }
                    }
                }
                pageInfo {
                    hasNextPage
                    hasPreviousPage
                    startCursor
                    endCursor
                }
                totalCount
            }
        }
    `;

    it("returns edges, opaque cursors, and page-token endCursor", async () => {
        const context = makeContext();
        context.calendarService.listEvents.mockResolvedValue({
            items: [sampleEvent, { ...sampleEvent, id: "ev-2" }],
            nextPageToken: "next-page-token",
        });

        const result = await execute(query, context, {
            calendarId: "work-calendar",
            timeMin: "2026-01-01T00:00:00.000Z",
        });

        expect(result.errors).toBeUndefined();
        const events = result.data?.events as any;

        expect(events.edges).toHaveLength(2);
        expect(events.edges[0].cursor).toBe(
            Buffer.from("gcal:ev-1").toString("base64"),
        );
        expect(events.edges[0].node).toMatchObject({
            id: "ev-1",
            calendarId: "work-calendar",
            summary: "Team Sync",
            recurringEventId: "recurring-1",
            originalStartTime: { dateTime: "2026-01-05T09:00:00.000Z" },
        });
        expect(events.pageInfo).toEqual({
            hasNextPage: true,
            hasPreviousPage: false,
            startCursor: Buffer.from("gcal:ev-1").toString("base64"),
            endCursor: "next-page-token",
        });
        expect(events.totalCount).toBeNull();

        // DateTime arg is converted to an ISO string before hitting gcal
        expect(context.calendarService.listEvents).toHaveBeenCalledWith(
            { fake: true },
            "work-calendar",
            2,
            undefined,
            "2026-01-01T00:00:00.000Z",
            undefined,
            "startTime",
        );
    });

    it("reports no next page when Google returns no page token", async () => {
        const context = makeContext();
        context.calendarService.listEvents.mockResolvedValue({
            items: [sampleEvent],
            nextPageToken: undefined,
        });

        const result = await execute(query, context, {});

        expect(result.errors).toBeUndefined();
        const events = result.data?.events as any;
        expect(events.pageInfo.hasNextPage).toBe(false);
        expect(events.pageInfo.endCursor).toBeNull();
    });

    it("wraps upstream failures as CALENDAR_ERROR", async () => {
        const context = makeContext();
        context.calendarService.listEvents.mockRejectedValue(new Error("boom"));

        const result = await execute(query, context, {});
        expect(result.errors?.[0].extensions?.code).toBe("CALENDAR_ERROR");
    });
});

describe("Query.event", () => {
    const query = /* GraphQL */ `
        query Event($id: ID!, $calendarId: ID) {
            event(id: $id, calendarId: $calendarId) {
                id
                calendarId
                summary
            }
        }
    `;

    it("fetches a single event from the requested calendar", async () => {
        const context = makeContext();

        const result = await execute(query, context, {
            id: "ev-1",
            calendarId: "work-calendar",
        });

        expect(result.errors).toBeUndefined();
        expect(result.data?.event).toEqual({
            id: "ev-1",
            calendarId: "work-calendar",
            summary: "Team Sync",
        });
        expect(context.calendarService.getEvent).toHaveBeenCalledWith(
            { fake: true },
            "work-calendar",
            "ev-1",
        );
    });

    it("defaults to the primary calendar", async () => {
        const context = makeContext();
        await execute(query, context, { id: "ev-1" });
        expect(context.calendarService.getEvent).toHaveBeenCalledWith(
            { fake: true },
            "primary",
            "ev-1",
        );
    });

    it("surfaces Google 404s as NOT_FOUND (not CALENDAR_ERROR)", async () => {
        const context = makeContext();
        context.calendarService.getEvent.mockRejectedValue(
            Object.assign(new Error("Not Found"), { code: 404 }),
        );

        const result = await execute(query, context, { id: "missing" });

        expect(result.errors?.[0].message).toBe("Event not found");
        expect(result.errors?.[0].extensions?.code).toBe("NOT_FOUND");
    });

    it("wraps other upstream failures as CALENDAR_ERROR", async () => {
        const context = makeContext();
        context.calendarService.getEvent.mockRejectedValue(
            Object.assign(new Error("Server error"), { code: 500 }),
        );

        const result = await execute(query, context, { id: "ev-1" });
        expect(result.errors?.[0].extensions?.code).toBe("CALENDAR_ERROR");
    });
});

describe("Mutation.createEvent", () => {
    const mutation = /* GraphQL */ `
        mutation CreateEvent($input: CreateEventInput!) {
            createEvent(input: $input) {
                id
                calendarId
                summary
            }
        }
    `;

    const input = {
        calendarId: "work-calendar",
        summary: "New Event",
        start: { dateTime: "2026-01-05T10:00:00.000Z" },
        end: { dateTime: "2026-01-05T11:00:00.000Z" },
        reminders: {
            useDefault: false,
            overrides: [
                { method: "EMAIL", minutes: 30 },
                { method: "POPUP", minutes: 10 },
            ],
        },
    };

    it("creates the event on the input calendar with mapped reminders", async () => {
        const context = makeContext();

        const result = await execute(mutation, context, { input });

        expect(result.errors).toBeUndefined();
        expect((result.data?.createEvent as any).calendarId).toBe(
            "work-calendar",
        );

        expect(context.calendarService.createEvent).toHaveBeenCalledWith(
            { fake: true },
            "work-calendar",
            expect.objectContaining({
                summary: "New Event",
                start: expect.objectContaining({
                    dateTime: "2026-01-05T10:00:00.000Z",
                }),
                reminders: {
                    useDefault: false,
                    overrides: [
                        { method: "email", minutes: 30 },
                        { method: "popup", minutes: 10 },
                    ],
                },
            }),
        );
    });

    it("publishes eventCreated with the input calendarId", async () => {
        const context = makeContext();
        await execute(mutation, context, { input });

        expect(pubsub.publish).toHaveBeenCalledWith(
            "eventCreated",
            expect.objectContaining({
                userId: "user-1",
                calendarId: "work-calendar",
                event: expect.objectContaining({
                    id: "ev-1",
                    calendarId: "work-calendar",
                }),
            }),
        );
    });
});

describe("Mutation.updateEvent", () => {
    const mutation = /* GraphQL */ `
        mutation UpdateEvent($id: ID!, $input: UpdateEventInput!) {
            updateEvent(id: $id, input: $input) {
                id
                calendarId
            }
        }
    `;

    it("honors input.calendarId", async () => {
        const context = makeContext();

        const result = await execute(mutation, context, {
            id: "ev-1",
            input: { calendarId: "work-calendar", summary: "Renamed" },
        });

        expect(result.errors).toBeUndefined();
        expect((result.data?.updateEvent as any).calendarId).toBe(
            "work-calendar",
        );
        expect(context.calendarService.updateEvent).toHaveBeenCalledWith(
            { fake: true },
            "work-calendar",
            "ev-1",
            expect.objectContaining({ summary: "Renamed" }),
        );
        expect(pubsub.publish).toHaveBeenCalledWith(
            "eventUpdated",
            expect.objectContaining({ calendarId: "work-calendar" }),
        );
    });

    it("defaults to the primary calendar when input.calendarId is absent", async () => {
        const context = makeContext();

        await execute(mutation, context, {
            id: "ev-1",
            input: { summary: "Renamed" },
        });

        expect(context.calendarService.updateEvent).toHaveBeenCalledWith(
            { fake: true },
            "primary",
            "ev-1",
            expect.objectContaining({ summary: "Renamed" }),
        );
    });
});

describe("Mutation.deleteEvent", () => {
    const mutation = /* GraphQL */ `
        mutation DeleteEvent($id: ID!, $calendarId: ID) {
            deleteEvent(id: $id, calendarId: $calendarId)
        }
    `;

    it("honors the calendarId argument and publishes it", async () => {
        const context = makeContext();

        const result = await execute(mutation, context, {
            id: "ev-1",
            calendarId: "work-calendar",
        });

        expect(result.errors).toBeUndefined();
        expect(result.data?.deleteEvent).toBe(true);
        expect(context.calendarService.deleteEvent).toHaveBeenCalledWith(
            { fake: true },
            "work-calendar",
            "ev-1",
        );
        expect(pubsub.publish).toHaveBeenCalledWith("eventDeleted", {
            userId: "user-1",
            calendarId: "work-calendar",
            payload: { id: "ev-1", calendarId: "work-calendar" },
        });
    });

    it("defaults to the primary calendar", async () => {
        const context = makeContext();

        await execute(mutation, context, { id: "ev-1" });

        expect(context.calendarService.deleteEvent).toHaveBeenCalledWith(
            { fake: true },
            "primary",
            "ev-1",
        );
    });
});

describe("Mutation.authenticateWithCode", () => {
    const mutation = /* GraphQL */ `
        mutation Auth($code: String!) {
            authenticateWithCode(code: $code) {
                accessToken
                user {
                    id
                    email
                    name
                    verified
                }
                expiresIn
            }
        }
    `;

    it("exchanges the code, fetches user info via gcal, and stores tokens", async () => {
        const calendarService = makeCalendarService({
            getTokensFromCode: vi.fn().mockResolvedValue({
                access_token: "access",
                refresh_token: "refresh",
                expiry_date: Date.now() + 3600_000,
            }),
            getUserInfo: vi.fn().mockResolvedValue({
                email: "user@example.com",
                name: "Test User",
                picture: "https://example.com/avatar.png",
            }),
        });

        const result = await execute(
            mutation,
            { calendarService },
            {
                code: "auth-code",
            },
        );

        expect(result.errors).toBeUndefined();
        const payload = result.data?.authenticateWithCode as any;
        expect(payload.user.email).toBe("user@example.com");
        expect(payload.user.name).toBe("Test User");
        expect(payload.user.verified).toBe(true);
        expect(typeof payload.accessToken).toBe("string");
        expect(payload.accessToken.length).toBeGreaterThan(0);
        expect(payload.expiresIn).toBeGreaterThan(0);

        expect(calendarService.getTokensFromCode).toHaveBeenCalledWith(
            "auth-code",
        );
        expect(calendarService.createAuthenticatedClient).toHaveBeenCalledWith({
            access_token: "access",
            refresh_token: "refresh",
        });
        expect(calendarService.getUserInfo).toHaveBeenCalledWith({
            fake: true,
        });
        expect(tokenStore.storeTokens).toHaveBeenCalledWith(
            payload.user.id,
            expect.objectContaining({
                email: "user@example.com",
                accessToken: "access",
                refreshToken: "refresh",
            }),
        );
    });

    it("surfaces user-info failures with USERINFO_FETCH_FAILED", async () => {
        const calendarService = makeCalendarService({
            getTokensFromCode: vi.fn().mockResolvedValue({
                access_token: "access",
                refresh_token: "refresh",
            }),
            getUserInfo: vi
                .fn()
                .mockRejectedValue(new Error("no email in response")),
        });

        const result = await execute(
            mutation,
            { calendarService },
            {
                code: "auth-code",
            },
        );

        expect(result.errors?.[0].extensions?.code).toBe(
            "USERINFO_FETCH_FAILED",
        );
        expect(tokenStore.storeTokens).not.toHaveBeenCalled();
    });
});

describe("CalendarEvent.htmlLink nullability", () => {
    it("serializes an event without htmlLink as null and does not error", async () => {
        const context = makeContext();
        const { htmlLink: _omitted, ...eventWithoutLink } = sampleEvent;
        context.calendarService.getEvent.mockResolvedValue(eventWithoutLink);

        const result = await execute(
            /* GraphQL */ `
                query {
                    event(id: "ev-1") {
                        id
                        htmlLink
                    }
                }
            `,
            context,
        );

        expect(result.errors).toBeUndefined();
        expect((result.data?.event as any).htmlLink).toBeNull();
    });
});

describe("Query.events argument handling", () => {
    const query = /* GraphQL */ `
        query Events($first: Int, $after: String, $timeMax: DateTime) {
            events(first: $first, after: $after, timeMax: $timeMax) {
                totalCount
            }
        }
    `;

    it("rejects an edge cursor passed as after with BAD_USER_INPUT", async () => {
        const context = makeContext();
        const edgeCursor = Buffer.from("gcal:ev-1").toString("base64");

        const result = await execute(query, context, { after: edgeCursor });

        expect(result.errors?.[0].extensions?.code).toBe("BAD_USER_INPUT");
        expect(result.errors?.[0].message).toMatch(/pageInfo\.endCursor/);
        expect(context.calendarService.listEvents).not.toHaveBeenCalled();
    });

    it("forwards a genuine page token untouched", async () => {
        const context = makeContext();
        await execute(query, context, { after: "next-page-token" });

        expect(context.calendarService.listEvents).toHaveBeenCalledWith(
            { fake: true },
            "primary",
            20,
            "next-page-token",
            expect.any(String),
            undefined,
            "startTime",
        );
    });

    it("clamps first to Google's 1..250 range", async () => {
        const context = makeContext();

        await execute(query, context, { first: 999 });
        expect(context.calendarService.listEvents.mock.calls[0][2]).toBe(250);

        context.calendarService.listEvents.mockClear();
        await execute(query, context, { first: 0 });
        expect(context.calendarService.listEvents.mock.calls[0][2]).toBe(1);
    });

    it("defaults timeMin to now when neither timeMin nor timeMax is given", async () => {
        const context = makeContext();
        const before = Date.now();

        await execute(query, context, {});

        const timeMinArg = context.calendarService.listEvents.mock.calls[0][4];
        expect(typeof timeMinArg).toBe("string");
        expect(new Date(timeMinArg).getTime()).toBeGreaterThanOrEqual(
            before - 1000,
        );
    });

    it("does not default timeMin when timeMax is provided", async () => {
        const context = makeContext();

        await execute(query, context, { timeMax: "2026-02-01T00:00:00.000Z" });

        expect(
            context.calendarService.listEvents.mock.calls[0][4],
        ).toBeUndefined();
    });
});

describe("Mutation.createRoutine validation errors", () => {
    const mutation = /* GraphQL */ `
        mutation CreateRoutine($input: CreateRoutineInput!) {
            createRoutine(input: $input) {
                id
                name
            }
        }
    `;

    const input = {
        name: "Stretch",
        duration: { minutes: 30, flexible: false },
        priority: "MEDIUM",
        flexibility: 999,
        energyLevelRequired: "MEDIUM",
        category: "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
        frequency: "DAILY",
        timePreferences: ["MORNING"],
        availabilityWindows: [],
        dependencies: [],
        minimumGapMinutes: 0,
        bufferTimeMinutes: 0,
        conflictResolution: "RESCHEDULE",
        canBeGrouped: false,
        enabled: true,
        tags: [],
    };

    it("maps RoutineValidationError to BAD_USER_INPUT with the validator message", async () => {
        const context = makeContext();
        vi.mocked(routineService.createRoutine).mockRejectedValue(
            new RoutineValidationError(
                "Invalid routine input: /flexibility: Expected number to be at most 100",
            ),
        );

        const result = await execute(mutation, context, { input });

        expect(result.data).toBeNull();
        expect(result.errors?.[0].extensions?.code).toBe("BAD_USER_INPUT");
        expect(result.errors?.[0].message).toMatch(/flexibility/);
        expect(pubsub.publish).not.toHaveBeenCalled();
    });

    it("leaves non-validation errors untouched (masked as internal)", async () => {
        const context = makeContext();
        vi.mocked(routineService.createRoutine).mockRejectedValue(
            new Error("db exploded"),
        );

        const result = await execute(mutation, context, { input });

        expect(result.errors?.[0].extensions?.code).not.toBe("BAD_USER_INPUT");
    });
});

describe("Subscription user scoping", () => {
    const subscriptionDoc = /* GraphQL */ `
        subscription {
            eventDeleted {
                id
                calendarId
            }
        }
    `;

    const deleteMutation = /* GraphQL */ `
        mutation DeleteEvent($id: ID!, $calendarId: ID) {
            deleteEvent(id: $id, calendarId: $calendarId)
        }
    `;

    it("delivers a user's own events but never another user's", async () => {
        const contextA = makeContext(); // user-1
        const contextB = makeContext({
            user: {
                id: "user-2",
                email: "other@example.com",
                name: "Other User",
                verified: true,
            },
        });

        const iterator = (await gqlSubscribe({
            schema,
            document: parse(subscriptionDoc),
            contextValue: contextA,
        })) as AsyncIterableIterator<any>;

        const nextPromise = iterator.next();
        // Let the subscription's pubsub listener attach before publishing.
        await new Promise((resolve) => setTimeout(resolve, 0));

        // User B deletes an event: must NOT reach user A's subscription.
        await execute(deleteMutation, contextB, {
            id: "ev-B",
            calendarId: "cal-shared",
        });
        // User A deletes an event: must arrive (and arrive first).
        await execute(deleteMutation, contextA, {
            id: "ev-A",
            calendarId: "cal-shared",
        });

        const { value } = await nextPromise;
        expect(value.errors).toBeUndefined();
        expect(value.data?.eventDeleted).toEqual({
            id: "ev-A",
            calendarId: "cal-shared",
        });

        await iterator.return?.();
    });
});
