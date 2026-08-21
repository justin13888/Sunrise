import { describe, expect, it } from "vitest";
import {
    GqlAttendeeResponseStatus,
    GqlCalendarAccessRole,
    GqlEventStatus,
    GqlEventVisibility,
    GqlReminderMethod,
} from "../generated/resolvers-types";
import {
    mapAccessRole,
    mapAttendeeResponse,
    mapEventStatus,
    mapEventVisibility,
    mapGCalCalendar,
    mapGCalEvent,
    mapReminderInput,
} from "./mappers";

describe("mapGCalEvent", () => {
    const fullEvent = {
        id: "event-1",
        summary: "Team Sync",
        description: "Weekly sync",
        location: "Room 4",
        start: {
            dateTime: "2026-01-05T10:00:00Z",
            timeZone: "America/Toronto",
        },
        end: { dateTime: "2026-01-05T11:00:00Z", timeZone: "America/Toronto" },
        status: "tentative",
        visibility: "private",
        creator: { email: "creator@example.com", displayName: "Creator" },
        organizer: {
            email: "organizer@example.com",
            displayName: "Organizer",
            self: true,
        },
        attendees: [
            {
                email: "a@example.com",
                displayName: "A",
                self: false,
                responseStatus: "accepted",
            },
            { email: "b@example.com" },
        ],
        recurringEventId: "recurring-123",
        originalStartTime: { dateTime: "2026-01-05T09:00:00Z" },
        htmlLink: "https://calendar.google.com/event?eid=abc",
        created: "2025-12-01T00:00:00Z",
        updated: "2025-12-02T00:00:00Z",
    };

    it("maps all fields including recurringEventId and originalStartTime", () => {
        const result = mapGCalEvent(fullEvent, "work-calendar");

        expect(result.id).toBe("event-1");
        expect(result.calendarId).toBe("work-calendar");
        expect(result.summary).toBe("Team Sync");
        expect(result.description).toBe("Weekly sync");
        expect(result.location).toBe("Room 4");
        expect(result.start.dateTime).toEqual(new Date("2026-01-05T10:00:00Z"));
        expect(result.start.timeZone).toBe("America/Toronto");
        expect(result.end.dateTime).toEqual(new Date("2026-01-05T11:00:00Z"));
        expect(result.status).toBe(GqlEventStatus.Tentative);
        expect(result.visibility).toBe(GqlEventVisibility.Private);
        expect(result.creator).toEqual({
            email: "creator@example.com",
            displayName: "Creator",
            self: null,
        });
        expect(result.organizer?.self).toBe(true);
        expect(result.recurringEventId).toBe("recurring-123");
        expect(result.originalStartTime?.dateTime).toEqual(
            new Date("2026-01-05T09:00:00Z"),
        );
        expect(result.htmlLink).toBe(
            "https://calendar.google.com/event?eid=abc",
        );
        expect(result.created).toEqual(new Date("2025-12-01T00:00:00Z"));
        expect(result.updated).toEqual(new Date("2025-12-02T00:00:00Z"));
    });

    it("maps attendees with response status defaults", () => {
        const result = mapGCalEvent(fullEvent, "primary");

        expect(result.attendees).toHaveLength(2);
        expect(result.attendees?.[0]).toEqual({
            email: "a@example.com",
            displayName: "A",
            self: false,
            responseStatus: GqlAttendeeResponseStatus.Accepted,
        });
        // Missing responseStatus defaults to NEEDS_ACTION
        expect(result.attendees?.[1].responseStatus).toBe(
            GqlAttendeeResponseStatus.NeedsAction,
        );
    });

    it("maps all-day events (date, no dateTime)", () => {
        const result = mapGCalEvent(
            {
                id: "all-day",
                summary: "Holiday",
                start: { date: "2026-01-01" },
                end: { date: "2026-01-02" },
            },
            "primary",
        );

        expect(result.start).toEqual({
            dateTime: null,
            date: "2026-01-01",
            timeZone: null,
        });
        expect(result.end.date).toBe("2026-01-02");
    });

    it("applies defaults for sparse events", () => {
        const result = mapGCalEvent({}, "primary");

        expect(result.id).toBe("");
        expect(result.calendarId).toBe("primary");
        expect(result.summary).toBe("");
        expect(result.description).toBeNull();
        expect(result.location).toBeNull();
        expect(result.status).toBe(GqlEventStatus.Confirmed);
        expect(result.visibility).toBe(GqlEventVisibility.Default);
        expect(result.creator).toBeNull();
        expect(result.organizer).toBeNull();
        expect(result.attendees).toBeNull();
        expect(result.recurringEventId).toBeNull();
        expect(result.originalStartTime).toBeNull();
        // Missing htmlLink maps to null (an empty string would crash the
        // URL scalar during serialization).
        expect(result.htmlLink).toBeNull();
        expect(result.created).toBeInstanceOf(Date);
        expect(result.updated).toBeInstanceOf(Date);
    });
});

describe("mapGCalCalendar", () => {
    it("maps a calendar-list entry", () => {
        const result = mapGCalCalendar({
            id: "cal-1",
            summary: "Work",
            description: "Work calendar",
            primary: true,
            accessRole: "owner",
            backgroundColor: "#ffffff",
            foregroundColor: "#000000",
            timeZone: "America/Toronto",
        });

        expect(result).toEqual({
            id: "cal-1",
            summary: "Work",
            description: "Work calendar",
            primary: true,
            accessRole: GqlCalendarAccessRole.Owner,
            backgroundColor: "#ffffff",
            foregroundColor: "#000000",
            timeZone: "America/Toronto",
        });
    });

    it("applies defaults for sparse entries", () => {
        const result = mapGCalCalendar({});
        expect(result.id).toBe("");
        expect(result.summary).toBe("");
        expect(result.primary).toBe(false);
        expect(result.accessRole).toBe(GqlCalendarAccessRole.None);
        expect(result.description).toBeNull();
    });
});

describe("enum mappers", () => {
    it("maps event statuses with CONFIRMED default", () => {
        expect(mapEventStatus("confirmed")).toBe(GqlEventStatus.Confirmed);
        expect(mapEventStatus("tentative")).toBe(GqlEventStatus.Tentative);
        expect(mapEventStatus("cancelled")).toBe(GqlEventStatus.Cancelled);
        expect(mapEventStatus(undefined)).toBe(GqlEventStatus.Confirmed);
        expect(mapEventStatus("bogus")).toBe(GqlEventStatus.Confirmed);
    });

    it("maps visibilities with DEFAULT default", () => {
        expect(mapEventVisibility("default")).toBe(GqlEventVisibility.Default);
        expect(mapEventVisibility("public")).toBe(GqlEventVisibility.Public);
        expect(mapEventVisibility("private")).toBe(GqlEventVisibility.Private);
        expect(mapEventVisibility("confidential")).toBe(
            GqlEventVisibility.Confidential,
        );
        expect(mapEventVisibility(null)).toBe(GqlEventVisibility.Default);
    });

    it("maps attendee responses with NEEDS_ACTION default", () => {
        expect(mapAttendeeResponse("needsAction")).toBe(
            GqlAttendeeResponseStatus.NeedsAction,
        );
        expect(mapAttendeeResponse("declined")).toBe(
            GqlAttendeeResponseStatus.Declined,
        );
        expect(mapAttendeeResponse("tentative")).toBe(
            GqlAttendeeResponseStatus.Tentative,
        );
        expect(mapAttendeeResponse("accepted")).toBe(
            GqlAttendeeResponseStatus.Accepted,
        );
        expect(mapAttendeeResponse(undefined)).toBe(
            GqlAttendeeResponseStatus.NeedsAction,
        );
    });

    it("maps access roles with NONE default", () => {
        expect(mapAccessRole("owner")).toBe(GqlCalendarAccessRole.Owner);
        expect(mapAccessRole("writer")).toBe(GqlCalendarAccessRole.Writer);
        expect(mapAccessRole("reader")).toBe(GqlCalendarAccessRole.Reader);
        expect(mapAccessRole("freeBusyReader")).toBe(
            GqlCalendarAccessRole.FreeBusyReader,
        );
        expect(mapAccessRole("none")).toBe(GqlCalendarAccessRole.None);
        expect(mapAccessRole(undefined)).toBe(GqlCalendarAccessRole.None);
    });
});

describe("mapReminderInput", () => {
    it("returns undefined when no reminders provided", () => {
        expect(mapReminderInput(undefined)).toBeUndefined();
        expect(mapReminderInput(null)).toBeUndefined();
    });

    it("maps EMAIL and POPUP methods to Google's lowercase strings", () => {
        const result = mapReminderInput({
            useDefault: false,
            overrides: [
                { method: GqlReminderMethod.Email, minutes: 30 },
                { method: GqlReminderMethod.Popup, minutes: 10 },
            ],
        });

        expect(result).toEqual({
            useDefault: false,
            overrides: [
                { method: "email", minutes: 30 },
                { method: "popup", minutes: 10 },
            ],
        });
    });

    it("handles useDefault-only input", () => {
        const result = mapReminderInput({ useDefault: true });
        expect(result).toEqual({ useDefault: true, overrides: undefined });
    });

    it("sets useDefault false when overrides are present without an explicit useDefault", () => {
        const result = mapReminderInput({
            overrides: [{ method: GqlReminderMethod.Popup, minutes: 5 }],
        });
        expect(result).toEqual({
            useDefault: false,
            overrides: [{ method: "popup", minutes: 5 }],
        });
    });

    it("keeps an explicit useDefault true even with overrides", () => {
        const result = mapReminderInput({
            useDefault: true,
            overrides: [{ method: GqlReminderMethod.Popup, minutes: 5 }],
        });
        expect(result?.useDefault).toBe(true);
    });
});
