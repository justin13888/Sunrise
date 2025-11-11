import type { GraphQLContext } from "../context";
import type { Resolvers } from "../generated/resolvers-types";
import { GraphQLError } from "graphql";
import { tokenStore } from "../services/tokenStore";
import { DateTimeScalar } from "./date-time";
import { URLScalar } from "./url";
import { withAuth, withUser, ensureAuth, ensureUser } from "./auth";

// TODO: Finish implementing these resolvers
export const resolvers: Resolvers = {
    DateTime: DateTimeScalar,
    URL: URLScalar,

    Query: {
        me: async (_, __, context: GraphQLContext) => {
            const { user } = ensureUser(context);
            return user;
        },

        authUrl: async (_, __, { calendarService }: GraphQLContext) => {
            return calendarService.getAuthUrl();
        },

        calendars: async (_, __, context: GraphQLContext) => {
            const { user, refreshToken, calendarService } = ensureAuth(context);

            try {
                const calendars = await calendarService.listCalendars(refreshToken);
                return calendars.map((cal) => ({
                    id: cal.id || "",
                    summary: cal.summary || "",
                    description: cal.description,
                    primary: cal.primary || false,
                    accessRole: mapAccessRole(cal.accessRole),
                    backgroundColor: cal.backgroundColor,
                    foregroundColor: cal.foregroundColor,
                    timeZone: cal.timeZone,
                }));
            } catch (error) {
                throw new GraphQLError(`Failed to fetch calendars: ${error}`, {
                    extensions: { code: "CALENDAR_ERROR" },
                });
            }
        },

        calendar: async (_, { id }, context: GraphQLContext) => {
            const { user, refreshToken, calendarService } = ensureAuth(context);

            try {
                const calendars = await calendarService.listCalendars(refreshToken);
                const calendar = calendars.find((cal) => cal.id === id);

                if (!calendar) {
                    throw new GraphQLError("Calendar not found", {
                        extensions: { code: "NOT_FOUND" },
                    });
                }

                return {
                    id: calendar.id || "",
                    summary: calendar.summary || "",
                    description: calendar.description,
                    primary: calendar.primary || false,
                    accessRole: mapAccessRole(calendar.accessRole),
                    backgroundColor: calendar.backgroundColor,
                    foregroundColor: calendar.foregroundColor,
                    timeZone: calendar.timeZone,
                };
            } catch (error) {
                throw new GraphQLError(`Failed to fetch calendar: ${error}`, {
                    extensions: { code: "CALENDAR_ERROR" },
                });
            }
        },

        events: async (_, args, context: GraphQLContext) => {
            const { user, refreshToken, calendarService } = ensureAuth(context);

            try {
                const result = await calendarService.listEvents(
                    refreshToken,
                    args.calendarId || 'primary',
                    args.first || 20,
                    args.after || undefined,
                    args.timeMin || undefined,
                    args.timeMax || undefined,
                    args.orderBy === 'UPDATED' ? 'updated' : 'startTime'
                );

                const edges = result.items.map((event) => ({
                    cursor: event.id || '',
                    node: {
                        id: event.id || "",
                        calendarId: args.calendarId || "primary",
                        summary: event.summary || "",
                        description: event.description,
                        location: event.location,
                        start: {
                            dateTime: event.start?.dateTime
                                ? new Date(event.start.dateTime)
                                : undefined,
                            date: event.start?.date,
                            timeZone: event.start?.timeZone,
                        },
                        end: {
                            dateTime: event.end?.dateTime
                                ? new Date(event.end.dateTime)
                                : undefined,
                            date: event.end?.date,
                            timeZone: event.end?.timeZone,
                        },
                        status: mapEventStatus(event.status),
                        visibility: mapEventVisibility(event.visibility),
                        creator: event.creator
                            ? {
                                email: event.creator.email || "",
                                displayName: event.creator.displayName,
                                self: event.creator.self,
                            }
                            : undefined,
                        organizer: event.organizer
                            ? {
                                email: event.organizer.email || "",
                                displayName: event.organizer.displayName,
                                self: event.organizer.self,
                            }
                            : undefined,
                        attendees: event.attendees?.map((attendee) => ({
                            email: attendee.email || "",
                            displayName: attendee.displayName,
                            self: attendee.self,
                            responseStatus: mapAttendeeResponse(attendee.responseStatus),
                        })),
                        htmlLink: event.htmlLink || "",
                        created: new Date(event.created || Date.now()),
                        updated: new Date(event.updated || Date.now()),
                    }
                }));

                return {
                    edges,
                    pageInfo: {
                        hasNextPage: !!result.nextPageToken,
                        hasPreviousPage: false, // Google Calendar API doesn't support backward pagination
                        startCursor: edges.length > 0 ? edges[0].cursor : null,
                        endCursor: result.nextPageToken || (edges.length > 0 ? edges[edges.length - 1].cursor : null),
                    },
                    totalCount: null, // Google Calendar API doesn't provide total count
                };
            } catch (error) {
                throw new GraphQLError(`Failed to fetch events: ${error}`, {
                    extensions: { code: "CALENDAR_ERROR" },
                });
            }
        },

        event: async (_, { id }, context: GraphQLContext) => {
            const { user, refreshToken, calendarService } = ensureAuth(context);

            // For now, we'll get all events and find the specific one
            // In production, you'd want to use the Calendar API's get method
            try {
                const result = await calendarService.listEvents(
                    refreshToken,
                    'primary',
                    100
                );
                const event = result.items.find((e) => e.id === id);

                if (!event) {
                    throw new GraphQLError("Event not found", {
                        extensions: { code: "NOT_FOUND" },
                    });
                }

                return {
                    id: event.id || "",
                    calendarId: "primary", // Would need to be tracked properly
                    summary: event.summary || "",
                    description: event.description,
                    location: event.location,
                    start: {
                        dateTime: event.start?.dateTime
                            ? new Date(event.start.dateTime)
                            : undefined,
                        date: event.start?.date,
                        timeZone: event.start?.timeZone,
                    },
                    end: {
                        dateTime: event.end?.dateTime
                            ? new Date(event.end.dateTime)
                            : undefined,
                        date: event.end?.date,
                        timeZone: event.end?.timeZone,
                    },
                    status: mapEventStatus(event.status),
                    visibility: mapEventVisibility(event.visibility),
                    creator: event.creator
                        ? {
                            email: event.creator.email || "",
                            displayName: event.creator.displayName,
                            self: event.creator.self,
                        }
                        : undefined,
                    organizer: event.organizer
                        ? {
                            email: event.organizer.email || "",
                            displayName: event.organizer.displayName,
                            self: event.organizer.self,
                        }
                        : undefined,
                    attendees: event.attendees?.map((attendee) => ({
                        email: attendee.email || "",
                        displayName: attendee.displayName,
                        self: attendee.self,
                        responseStatus: mapAttendeeResponse(attendee.responseStatus),
                    })),
                    htmlLink: event.htmlLink || "",
                    created: new Date(event.created || Date.now()),
                    updated: new Date(event.updated || Date.now()),
                };
            } catch (error) {
                throw new GraphQLError(`Failed to fetch event: ${error}`, {
                    extensions: { code: "CALENDAR_ERROR" },
                });
            }
        },
    },

    Mutation: {
        authenticateWithCode: async (
            _,
            { code },
            { calendarService }: GraphQLContext,
        ) => {
            try {
                console.log("🔐 Exchanging authorization code for tokens...");
                const tokens = await calendarService.getTokensFromCode(code);

                if (!tokens.access_token || !tokens.refresh_token) {
                    throw new GraphQLError(
                        "Failed to get tokens from authorization code",
                    );
                }

                // TODO: In production, you'd:
                // 1. Extract and validate user info from JWT token
                // 2. Create/update user record in database
                // 3. Use proper user ID from token

                // Get user info from Google using the access token
                // IMPORTANT: This must succeed - we don't use fallback values
                let userInfo: {
                    email: string;
                    name: string;
                    picture?: string;
                };

                try {
                    // Create a temporary OAuth client to get user info
                    const { OAuth2Client } = await import("google-auth-library");
                    const oauth2Client = new OAuth2Client();
                    oauth2Client.setCredentials({
                        access_token: tokens.access_token,
                        refresh_token: tokens.refresh_token,
                    });

                    console.log("🔍 Attempting to fetch user info from Google...");
                    const { google } = await import("googleapis");
                    const oauth2 = google.oauth2({ version: "v2", auth: oauth2Client });
                    const userInfoResponse = await oauth2.userinfo.get();

                    console.log("🔍 User info response:", JSON.stringify(userInfoResponse.data, null, 2));

                    if (!userInfoResponse.data || !userInfoResponse.data.email) {
                        throw new Error("Failed to get user info from Google - no email in response");
                    }

                    userInfo = {
                        email: userInfoResponse.data.email,
                        name: userInfoResponse.data.name || userInfoResponse.data.email, // Use email as name if name not provided
                        picture: userInfoResponse.data.picture || undefined,
                    };

                    console.log("✅ Fetched user info from Google:", userInfo.email);
                } catch (userInfoError) {
                    console.error("❌ Could not fetch user info from Google:");
                    console.error("Error details:", userInfoError);
                    if (userInfoError instanceof Error) {
                        console.error("Error message:", userInfoError.message);
                        console.error("Error stack:", userInfoError.stack);
                    }
                    throw new GraphQLError(
                        "Failed to fetch user information from Google. " +
                        "This may be because the required OAuth scopes (userinfo.email, userinfo.profile) were not granted. " +
                        "Please try signing in again.",
                        {
                            extensions: {
                                code: "USERINFO_FETCH_FAILED",
                                originalError: userInfoError instanceof Error ? userInfoError.message : String(userInfoError)
                            },
                        }
                    );
                }

                // Use the email as a stable user ID (in production, this would be a database ID)
                // We hash or encode it to make it URL-safe and consistent
                const userId = `google-${Buffer.from(userInfo.email).toString('base64').replace(/[/+=]/g, '')}`;

                const user = {
                    id: userId,
                    email: userInfo.email,
                    name: userInfo.name,
                    picture: userInfo.picture,
                    verified: true,
                };

                // Store tokens for the user
                const expiresIn = tokens.expiry_date
                    ? Math.floor((tokens.expiry_date - Date.now()) / 1000)
                    : 3600;
                const expiresAt = new Date(Date.now() + expiresIn * 1000);

                await tokenStore.storeTokens(userId, {
                    userId,
                    accessToken: tokens.access_token,
                    refreshToken: tokens.refresh_token,
                    expiresAt,
                    email: userInfo.email,
                    name: userInfo.name,
                    picture: userInfo.picture,
                });

                console.log(
                    `✅ Successfully authenticated user: ${userId} (${userInfo.email})`,
                );

                return {
                    accessToken: tokens.access_token,
                    refreshToken: tokens.refresh_token,
                    user,
                    expiresIn,
                };
            } catch (error) {
                console.error("Authentication failed:", error);
                throw new GraphQLError(`Authentication failed: ${error}`, {
                    extensions: { code: "AUTH_ERROR" },
                });
            }
        },

        refreshToken: async (_, __, context: GraphQLContext) => {
            // User must be authenticated to refresh their token
            const { user, refreshToken, calendarService } = ensureAuth(context);

            try {
                const client =
                    await calendarService.getClientFromRefreshToken(refreshToken);
                const credentials = client.credentials;

                // Get updated token expiry
                const expiresIn = credentials.expiry_date
                    ? Math.floor((credentials.expiry_date - Date.now()) / 1000)
                    : 3600;
                const expiresAt = new Date(Date.now() + expiresIn * 1000);

                // Update stored tokens with new access token and expiry
                await tokenStore.storeTokens(user.id, {
                    userId: user.id,
                    accessToken: credentials.access_token || "",
                    refreshToken: credentials.refresh_token || refreshToken,
                    expiresAt,
                    email: user.email,
                    name: user.name || user.email,
                    picture: user.picture,
                });

                console.log(`🔄 Refreshed tokens for user: ${user.email}`);

                return {
                    accessToken: credentials.access_token || "",
                    refreshToken: credentials.refresh_token || refreshToken,
                    user,
                    expiresIn,
                };
            } catch (error) {
                console.error("Token refresh failed:", error);
                throw new GraphQLError(`Token refresh failed: ${error}`, {
                    extensions: { code: "AUTH_ERROR" },
                });
            }
        },

        logout: async (_, __, context: GraphQLContext) => {
            // Remove tokens from server storage if user is authenticated
            if (context.user) {
                try {
                    await tokenStore.removeTokens(context.user.id);
                    console.log(`🚪 User logged out: ${context.user.id}`);
                } catch (error) {
                    console.error("Error removing tokens:", error);
                }
            }
            return true;
        },

        createEvent: async (_, { input }, context: GraphQLContext) => {
            const { user, refreshToken } = ensureAuth(context);

            // Implementation would use Google Calendar API to create event
            throw new GraphQLError("Not implemented", {
                extensions: { code: "NOT_IMPLEMENTED" },
            });
        },

        updateEvent: async (_, { id, input }, context: GraphQLContext) => {
            const { user, refreshToken } = ensureAuth(context);

            // Implementation would use Google Calendar API to update event
            throw new GraphQLError("Not implemented", {
                extensions: { code: "NOT_IMPLEMENTED" },
            });
        },

        deleteEvent: async (_, { id }, context: GraphQLContext) => {
            const { user, refreshToken } = ensureAuth(context);

            // Implementation would use Google Calendar API to delete event
            throw new GraphQLError("Not implemented", {
                extensions: { code: "NOT_IMPLEMENTED" },
            });
        },
    },

    Calendar: {
        events: async (parent, args, context: GraphQLContext) => {
            const { user, refreshToken, calendarService } = ensureAuth(context);

            try {
                const result = await calendarService.listEvents(
                    refreshToken,
                    parent.id,
                    args.first || 20,
                    args.after || undefined,
                    args.timeMin || undefined,
                    args.timeMax || undefined,
                );

                const edges = result.items.map((event) => ({
                    cursor: event.id || '',
                    node: {
                        id: event.id || "",
                        calendarId: parent.id,
                        summary: event.summary || "",
                        description: event.description,
                        location: event.location,
                        start: {
                            dateTime: event.start?.dateTime
                                ? new Date(event.start.dateTime)
                                : undefined,
                            date: event.start?.date,
                            timeZone: event.start?.timeZone,
                        },
                        end: {
                            dateTime: event.end?.dateTime
                                ? new Date(event.end.dateTime)
                                : undefined,
                            date: event.end?.date,
                            timeZone: event.end?.timeZone,
                        },
                        status: mapEventStatus(event.status),
                        visibility: mapEventVisibility(event.visibility),
                        creator: event.creator
                            ? {
                                email: event.creator.email || "",
                                displayName: event.creator.displayName,
                                self: event.creator.self,
                            }
                            : undefined,
                        organizer: event.organizer
                            ? {
                                email: event.organizer.email || "",
                                displayName: event.organizer.displayName,
                                self: event.organizer.self,
                            }
                            : undefined,
                        attendees: event.attendees?.map((attendee) => ({
                            email: attendee.email || "",
                            displayName: attendee.displayName,
                            self: attendee.self,
                            responseStatus: mapAttendeeResponse(attendee.responseStatus),
                        })),
                        htmlLink: event.htmlLink || "",
                        created: new Date(event.created || Date.now()),
                        updated: new Date(event.updated || Date.now()),
                    }
                }));

                return {
                    edges,
                    pageInfo: {
                        hasNextPage: !!result.nextPageToken,
                        hasPreviousPage: false,
                        startCursor: edges.length > 0 ? edges[0].cursor : null,
                        endCursor: result.nextPageToken || (edges.length > 0 ? edges[edges.length - 1].cursor : null),
                    },
                    totalCount: null,
                };
            } catch (error) {
                throw new GraphQLError(`Failed to fetch calendar events: ${error}`, {
                    extensions: { code: "CALENDAR_ERROR" },
                });
            }
        },
    },
};

// Helper functions to map Google Calendar API values to GraphQL enum values
function mapAccessRole(role?: string | null) {
    switch (role) {
        case "none":
            return "NONE";
        case "freeBusyReader":
            return "FREE_BUSY_READER";
        case "reader":
            return "READER";
        case "writer":
            return "WRITER";
        case "owner":
            return "OWNER";
        default:
            return "NONE";
    }
}

function mapEventStatus(status?: string | null): "CONFIRMED" | "TENTATIVE" | "CANCELLED" {
    switch (status) {
        case "confirmed":
            return "CONFIRMED";
        case "tentative":
            return "TENTATIVE";
        case "cancelled":
            return "CANCELLED";
        default:
            return "CONFIRMED";
    }
}

function mapEventVisibility(visibility?: string | null): "DEFAULT" | "PUBLIC" | "PRIVATE" | "CONFIDENTIAL" {
    switch (visibility) {
        case "default":
            return "DEFAULT";
        case "public":
            return "PUBLIC";
        case "private":
            return "PRIVATE";
        case "confidential":
            return "CONFIDENTIAL";
        default:
            return "DEFAULT";
    }
}

function mapAttendeeResponse(response?: string | null): "NEEDS_ACTION" | "DECLINED" | "TENTATIVE" | "ACCEPTED" {
    switch (response) {
        case "needsAction":
            return "NEEDS_ACTION";
        case "declined":
            return "DECLINED";
        case "tentative":
            return "TENTATIVE";
        case "accepted":
            return "ACCEPTED";
        default:
            return "NEEDS_ACTION";
    }
}
