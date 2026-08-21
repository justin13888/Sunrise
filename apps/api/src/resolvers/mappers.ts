import type { calendar_v3 } from "@sunrise/gcal";
import {
    GqlAttendeeResponseStatus,
    GqlCalendarAccessRole,
    type GqlCalendarEvent,
    type GqlEventDateTime,
    type GqlEventParticipant,
    GqlEventStatus,
    GqlEventVisibility,
    type GqlReminderInput,
    GqlReminderMethod,
} from "../generated/resolvers-types";
import type { Calendar } from "../types/calendar";

/**
 * Shared mapping helpers between Google Calendar API payloads and the GraphQL
 * schema types. Used by the resolvers and the background poller so events are
 * shaped identically everywhere.
 */

export function mapEventStatus(status?: string | null): GqlEventStatus {
    switch (status) {
        case "tentative":
            return GqlEventStatus.Tentative;
        case "cancelled":
            return GqlEventStatus.Cancelled;
        default:
            return GqlEventStatus.Confirmed;
    }
}

export function mapEventVisibility(
    visibility?: string | null,
): GqlEventVisibility {
    switch (visibility) {
        case "public":
            return GqlEventVisibility.Public;
        case "private":
            return GqlEventVisibility.Private;
        case "confidential":
            return GqlEventVisibility.Confidential;
        default:
            return GqlEventVisibility.Default;
    }
}

export function mapAttendeeResponse(
    response?: string | null,
): GqlAttendeeResponseStatus {
    switch (response) {
        case "declined":
            return GqlAttendeeResponseStatus.Declined;
        case "tentative":
            return GqlAttendeeResponseStatus.Tentative;
        case "accepted":
            return GqlAttendeeResponseStatus.Accepted;
        default:
            return GqlAttendeeResponseStatus.NeedsAction;
    }
}

export function mapAccessRole(role?: string | null): GqlCalendarAccessRole {
    switch (role) {
        case "freeBusyReader":
            return GqlCalendarAccessRole.FreeBusyReader;
        case "reader":
            return GqlCalendarAccessRole.Reader;
        case "writer":
            return GqlCalendarAccessRole.Writer;
        case "owner":
            return GqlCalendarAccessRole.Owner;
        default:
            return GqlCalendarAccessRole.None;
    }
}

function mapEventDateTime(
    dt?: calendar_v3.Schema$EventDateTime | null,
): GqlEventDateTime {
    return {
        dateTime: dt?.dateTime ? new Date(dt.dateTime) : null,
        date: dt?.date ?? null,
        timeZone: dt?.timeZone ?? null,
    };
}

function mapParticipant(
    participant: calendar_v3.Schema$EventAttendee,
    withResponseStatus: boolean,
): GqlEventParticipant {
    return {
        email: participant.email || "",
        displayName: participant.displayName ?? null,
        self: participant.self ?? null,
        ...(withResponseStatus && {
            responseStatus: mapAttendeeResponse(participant.responseStatus),
        }),
    };
}

/**
 * Map a Google Calendar event to the GraphQL CalendarEvent shape.
 *
 * `calendarId` must be the calendar the event was fetched from (Google event
 * payloads do not carry it).
 */
export function mapGCalEvent(
    event: calendar_v3.Schema$Event,
    calendarId: string,
): GqlCalendarEvent {
    return {
        id: event.id || "",
        calendarId,
        summary: event.summary || "",
        description: event.description ?? null,
        location: event.location ?? null,
        start: mapEventDateTime(event.start),
        end: mapEventDateTime(event.end),
        status: mapEventStatus(event.status),
        visibility: mapEventVisibility(event.visibility),
        creator: event.creator ? mapParticipant(event.creator, false) : null,
        organizer: event.organizer
            ? mapParticipant(event.organizer, false)
            : null,
        attendees:
            event.attendees?.map((attendee) =>
                mapParticipant(attendee, true),
            ) ?? null,
        recurringEventId: event.recurringEventId ?? null,
        originalStartTime: event.originalStartTime
            ? mapEventDateTime(event.originalStartTime)
            : null,
        htmlLink: event.htmlLink ?? null,
        created: new Date(event.created || Date.now()),
        updated: new Date(event.updated || Date.now()),
    };
}

/**
 * Map a Google calendar-list entry to the GraphQL Calendar shape
 * (sans the `events` connection, which has its own field resolver).
 */
export function mapGCalCalendar(
    calendar: calendar_v3.Schema$CalendarListEntry,
): Calendar {
    return {
        id: calendar.id || "",
        summary: calendar.summary || "",
        description: calendar.description ?? null,
        primary: calendar.primary || false,
        accessRole: mapAccessRole(calendar.accessRole),
        backgroundColor: calendar.backgroundColor ?? null,
        foregroundColor: calendar.foregroundColor ?? null,
        timeZone: calendar.timeZone ?? null,
    };
}

/**
 * Map GraphQL reminder input to the Google Calendar `reminders` request shape,
 * converting enum methods (EMAIL/POPUP) to Google's lowercase strings.
 */
export function mapReminderInput(
    reminders?: GqlReminderInput | null,
): calendar_v3.Schema$Event["reminders"] | undefined {
    if (!reminders) return undefined;
    const overrides = reminders.overrides?.map((override) => ({
        method: override.method === GqlReminderMethod.Email ? "email" : "popup",
        minutes: override.minutes,
    }));
    return {
        // Overrides only take effect when useDefault is false; if overrides
        // are present and useDefault was not explicitly set, disable defaults.
        useDefault:
            reminders.useDefault ??
            (overrides && overrides.length > 0 ? false : undefined),
        overrides,
    };
}
