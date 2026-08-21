/**
 * Represents a calendar container that holds events.
 *
 * Used as the codegen mapper type for the GraphQL `Calendar` type so that
 * resolvers can return calendars without the `events` connection (which is
 * resolved by its own field resolver).
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
