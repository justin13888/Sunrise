/**
 * Represents a calendar event with all its properties and metadata
 */
export interface CalendarEvent {
    /** Unique identifier for the event */
    id: string;
    /** ID of the calendar that contains this event */
    calendarId: string;
    /** Title/name of the event */
    summary: string;
    /** Detailed description of the event content */
    description?: string | null;
    /** Physical or virtual location where the event takes place */
    location?: string | null;
    /** Start date and time of the event */
    start: EventDateTime;
    /** End date and time of the event */
    end: EventDateTime;
    /** Current status of the event */
    status: "CONFIRMED" | "TENTATIVE" | "CANCELLED";
    /** Visibility level determining who can see the event */
    visibility: "DEFAULT" | "PUBLIC" | "PRIVATE" | "CONFIDENTIAL";
    /** Person who created the event */
    creator?: EventParticipant;
    /** Person who organized/owns the event */
    organizer?: EventParticipant;
    /** List of people invited to the event */
    attendees?: EventParticipant[];
    /** ID of the recurring event series this event belongs to */
    recurringEventId?: string | null;
    /** Original start time for moved recurring event instances */
    originalStartTime?: EventDateTime;
    /** Direct link to view the event in the calendar application */
    htmlLink: string;
    /** Timestamp when the event was first created */
    created: Date;
    /** Timestamp when the event was last modified */
    updated: Date;
}

/**
 * Represents a calendar container that holds events
 */
export interface Calendar {
    /** Unique identifier for the calendar */
    id: string;
    /** Display name/title of the calendar */
    summary: string;
    /** Optional description explaining the calendar's purpose */
    description?: string | null;
    /** Whether this is the user's primary/default calendar */
    primary: boolean;
    /** User's permission level for this calendar */
    accessRole: "NONE" | "FREE_BUSY_READER" | "READER" | "WRITER" | "OWNER";
    /** Hex color code for calendar background display */
    backgroundColor?: string | null;
    /** Hex color code for calendar text/foreground display */
    foregroundColor?: string | null;
    /** IANA timezone identifier for the calendar */
    timeZone?: string | null;
}

/**
 * Represents date and time information for events, supporting both all-day and timed events
 */
export interface EventDateTime {
    /** Specific date and time for timed events */
    dateTime?: Date | null;
    /** Date in YYYY-MM-DD format for all-day events */
    date?: string | null;
    /** IANA timezone identifier for the date/time */
    timeZone?: string | null; // TODO: Add some business logic to parse the string into a timezone object
}

/**
 * Represents a person involved in a calendar event (creator, organizer, or attendee)
 */
export interface EventParticipant {
    /** Email address of the participant */
    email: string;
    /** Human-readable name of the participant */
    displayName?: string | null;
    /** Whether this participant is the authenticated user */
    self?: boolean | null;
    /** Participant's response to the event invitation */
    responseStatus?: "NEEDS_ACTION" | "DECLINED" | "TENTATIVE" | "ACCEPTED";
}
