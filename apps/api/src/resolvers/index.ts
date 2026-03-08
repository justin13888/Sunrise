/** biome-ignore-all lint/suspicious/noExplicitAny: Resolve generally don't know about a specific type */
import { GraphQLError } from "graphql";
import { filter, pipe } from "graphql-yoga";
import type { GraphQLContext } from "../context";
import {
    GqlAttendeeResponseStatus,
    GqlConflictResolution,
    GqlDependencyRelationship,
    GqlEnergyLevel,
    GqlEventStatus,
    GqlEventVisibility,
    GqlFrequency,
    GqlPriorityLevel,
    type GqlResolvers,
    GqlTimeOfDay,
} from "../generated/resolvers-types";
import { signJWT } from "../services/jwt";
import { pubsub } from "../services/pubsub";
import { routineService } from "../services/routineService";
import { tokenStore } from "../services/tokenStore";
import { ensureAuth, ensureUser } from "./auth";
import { DateTimeScalar } from "./date-time";
import { URLScalar } from "./url";

// TODO: Finish implementing these resolvers
export const resolvers: GqlResolvers = {
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
            const { refreshToken, calendarService } = ensureAuth(context);

            try {
                const auth = calendarService.createAuthenticatedClient({
                    refresh_token: refreshToken,
                });
                const calendars = await calendarService.listCalendars(auth);
                return calendars.items.map((cal) => ({
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
            const { refreshToken, calendarService } = ensureAuth(context);

            try {
                const auth = calendarService.createAuthenticatedClient({
                    refresh_token: refreshToken,
                });
                const calendars = await calendarService.listCalendars(auth);
                const calendar = calendars.items.find((cal) => cal.id === id);

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
            const { refreshToken, calendarService } = ensureAuth(context);

            try {
                const auth = calendarService.createAuthenticatedClient({
                    refresh_token: refreshToken,
                });
                const result = await calendarService.listEvents(
                    auth,
                    args.calendarId || "primary",
                    args.first || 20,
                    args.after || undefined,
                    args.timeMin || undefined,
                    args.timeMax || undefined,
                    args.orderBy === "UPDATED" ? "updated" : "startTime",
                );

                const edges = result.items.map((event) => ({
                    cursor: event.id || "",
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
                            responseStatus: mapAttendeeResponse(
                                attendee.responseStatus,
                            ),
                        })),
                        htmlLink: event.htmlLink || "",
                        created: new Date(event.created || Date.now()),
                        updated: new Date(event.updated || Date.now()),
                    },
                }));

                return {
                    edges,
                    pageInfo: {
                        hasNextPage: !!result.nextPageToken,
                        hasPreviousPage: false, // Google Calendar API doesn't support backward pagination
                        startCursor: edges.length > 0 ? edges[0].cursor : null,
                        endCursor:
                            result.nextPageToken ||
                            (edges.length > 0
                                ? edges[edges.length - 1].cursor
                                : null),
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
            const { refreshToken, calendarService } = ensureAuth(context);

            // For now, we'll get all events and find the specific one
            // In production, you'd want to use the Calendar API's get method
            try {
                const auth = calendarService.createAuthenticatedClient({
                    refresh_token: refreshToken,
                });
                const result = await calendarService.listEvents(
                    auth,
                    "primary",
                    100,
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
                        responseStatus: mapAttendeeResponse(
                            attendee.responseStatus,
                        ),
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

        routines: async (_, __, context: GraphQLContext) => {
            const { user } = ensureUser(context);
            const list = await routineService.listRoutines(user.id);
            return list.map(mapRoutine);
        },

        routine: async (_, { id }, context: GraphQLContext) => {
            const { user } = ensureUser(context);
            const routine = await routineService.getRoutine(id, user.id);
            if (!routine) return null;
            return mapRoutine(routine);
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
                    const { OAuth2Client } = await import(
                        "google-auth-library"
                    );
                    const oauth2Client = new OAuth2Client();
                    oauth2Client.setCredentials({
                        access_token: tokens.access_token,
                        refresh_token: tokens.refresh_token,
                    });

                    console.log(
                        "🔍 Attempting to fetch user info from Google...",
                    );
                    const { google } = await import("googleapis");
                    const oauth2 = google.oauth2({
                        version: "v2",
                        auth: oauth2Client,
                    });
                    const userInfoResponse = await oauth2.userinfo.get();

                    console.log(
                        "🔍 User info response:",
                        JSON.stringify(userInfoResponse.data, null, 2),
                    );

                    if (
                        !userInfoResponse.data ||
                        !userInfoResponse.data.email
                    ) {
                        throw new Error(
                            "Failed to get user info from Google - no email in response",
                        );
                    }

                    userInfo = {
                        email: userInfoResponse.data.email,
                        name:
                            userInfoResponse.data.name ||
                            userInfoResponse.data.email, // Use email as name if name not provided
                        picture: userInfoResponse.data.picture || undefined,
                    };

                    console.log(
                        "✅ Fetched user info from Google:",
                        userInfo.email,
                    );
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
                                originalError:
                                    userInfoError instanceof Error
                                        ? userInfoError.message
                                        : String(userInfoError),
                            },
                        },
                    );
                }

                // Use the email as a stable user ID (in production, this would be a database ID)
                // We hash or encode it to make it URL-safe and consistent
                const userId = `google-${Buffer.from(userInfo.email).toString("base64").replace(/[/+=]/g, "")}`;

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

                const jwtToken = await signJWT(userId);

                return {
                    accessToken: jwtToken,
                    refreshToken: null,
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
                const client = calendarService.createAuthenticatedClient({
                    refresh_token: refreshToken,
                });
                // Force a token refresh to get fresh credentials
                await client.getAccessToken();
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

                const jwtToken = await signJWT(user.id);

                return {
                    accessToken: jwtToken,
                    refreshToken: null,
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
            const { refreshToken, calendarService } = ensureAuth(context);

            try {
                const auth = calendarService.createAuthenticatedClient({
                    refresh_token: refreshToken,
                });
                const event = await calendarService.createEvent(
                    auth,
                    input.calendarId,
                    {
                        summary: input.summary,
                        description: input.description || undefined,
                        location: input.location || undefined,
                        start: {
                            dateTime: input.start.dateTime?.toISOString(),
                            date: input.start.date || undefined,
                            timeZone: input.start.timeZone || undefined,
                        },
                        end: {
                            dateTime: input.end.dateTime?.toISOString(),
                            date: input.end.date || undefined,
                            timeZone: input.end.timeZone || undefined,
                        },
                        attendees: input.attendees?.map((email: string) => ({
                            email,
                        })),
                    },
                );

                const gqlEvent = mapGCalEvent(event, input.calendarId);
                pubsub.publish("eventCreated", {
                    calendarId: input.calendarId,
                    event: gqlEvent,
                });
                return gqlEvent;
            } catch (error) {
                throw new GraphQLError(`Failed to create event: ${error}`, {
                    extensions: { code: "CALENDAR_ERROR" },
                });
            }
        },

        updateEvent: async (_, { id, input }, context: GraphQLContext) => {
            const { refreshToken, calendarService } = ensureAuth(context);

            // Default to primary calendar; calendarId can be added to UpdateEventInput in the future
            const calendarId = "primary";

            try {
                const auth = calendarService.createAuthenticatedClient({
                    refresh_token: refreshToken,
                });

                const eventData: Record<string, unknown> = {};
                if (input.summary !== undefined && input.summary !== null)
                    eventData.summary = input.summary;
                if (input.description !== undefined)
                    eventData.description = input.description;
                if (input.location !== undefined)
                    eventData.location = input.location;
                if (input.start)
                    eventData.start = {
                        dateTime: input.start.dateTime?.toISOString(),
                        date: input.start.date || undefined,
                        timeZone: input.start.timeZone || undefined,
                    };
                if (input.end)
                    eventData.end = {
                        dateTime: input.end.dateTime?.toISOString(),
                        date: input.end.date || undefined,
                        timeZone: input.end.timeZone || undefined,
                    };
                if (input.attendees)
                    eventData.attendees = input.attendees.map(
                        (email: string) => ({ email }),
                    );

                const event = await calendarService.updateEvent(
                    auth,
                    calendarId,
                    id,
                    eventData,
                );

                const gqlEvent = mapGCalEvent(event, calendarId);
                pubsub.publish("eventUpdated", {
                    calendarId,
                    event: gqlEvent,
                });
                return gqlEvent;
            } catch (error) {
                throw new GraphQLError(`Failed to update event: ${error}`, {
                    extensions: { code: "CALENDAR_ERROR" },
                });
            }
        },

        deleteEvent: async (_, { id }, context: GraphQLContext) => {
            const { refreshToken, calendarService } = ensureAuth(context);
            const calendarId = "primary";

            try {
                const auth = calendarService.createAuthenticatedClient({
                    refresh_token: refreshToken,
                });
                await calendarService.deleteEvent(auth, calendarId, id);
                pubsub.publish("eventDeleted", {
                    calendarId,
                    payload: { id, calendarId },
                });
                return true;
            } catch (error) {
                throw new GraphQLError(`Failed to delete event: ${error}`, {
                    extensions: { code: "CALENDAR_ERROR" },
                });
            }
        },

        createRoutine: async (_, { input }, context: GraphQLContext) => {
            const { user } = ensureUser(context);
            const routine = await routineService.createRoutine(user.id, {
                name: input.name,
                description: input.description ?? undefined,
                duration: {
                    minutes: input.duration.minutes,
                    flexible: input.duration.flexible,
                    min_duration: input.duration.minDuration ?? undefined,
                    max_duration: input.duration.maxDuration ?? undefined,
                },
                priority: mapGqlPriority(input.priority),
                flexibility: input.flexibility,
                energy_level_required: mapGqlEnergyLevel(
                    input.energyLevelRequired,
                ),
                category: input.category,
                frequency: mapGqlFrequency(input.frequency),
                time_preferences: input.timePreferences.map(mapGqlTimeOfDay),
                availability_windows: input.availabilityWindows.map((w) => ({
                    start_hour: w.startHour,
                    start_minute: w.startMinute,
                    end_hour: w.endHour,
                    end_minute: w.endMinute,
                })),
                dependencies: input.dependencies.map((d) => ({
                    routine_id: d.routineId,
                    relationship: mapGqlDependencyRelationship(d.relationship),
                    buffer_minutes: d.bufferMinutes ?? undefined,
                })),
                minimum_gap_minutes: input.minimumGapMinutes,
                buffer_time_minutes: input.bufferTimeMinutes,
                conflict_resolution: mapGqlConflictResolution(
                    input.conflictResolution,
                ),
                can_be_grouped: input.canBeGrouped,
                preferred_batch_size: input.preferredBatchSize ?? undefined,
                enabled: input.enabled,
                tags: input.tags,
            });
            const gqlRoutine = mapRoutine(routine);
            pubsub.publish("routineCreated", { routine: gqlRoutine });
            return gqlRoutine;
        },

        updateRoutine: async (_, { id, input }, context: GraphQLContext) => {
            const { user } = ensureUser(context);
            const updated = await routineService.updateRoutine(id, user.id, {
                ...(input.name != null && { name: input.name }),
                ...(input.description !== undefined && {
                    description: input.description ?? undefined,
                }),
                ...(input.duration != null && {
                    duration: {
                        minutes: input.duration.minutes,
                        flexible: input.duration.flexible,
                        min_duration: input.duration.minDuration ?? undefined,
                        max_duration: input.duration.maxDuration ?? undefined,
                    },
                }),
                ...(input.priority != null && {
                    priority: mapGqlPriority(input.priority),
                }),
                ...(input.flexibility != null && {
                    flexibility: input.flexibility,
                }),
                ...(input.energyLevelRequired != null && {
                    energy_level_required: mapGqlEnergyLevel(
                        input.energyLevelRequired,
                    ),
                }),
                ...(input.category != null && {
                    category: input.category,
                }),
                ...(input.frequency != null && {
                    frequency: mapGqlFrequency(input.frequency),
                }),
                ...(input.timePreferences != null && {
                    time_preferences:
                        input.timePreferences.map(mapGqlTimeOfDay),
                }),
                ...(input.availabilityWindows != null && {
                    availability_windows: input.availabilityWindows.map(
                        (w) => ({
                            start_hour: w.startHour,
                            start_minute: w.startMinute,
                            end_hour: w.endHour,
                            end_minute: w.endMinute,
                        }),
                    ),
                }),
                ...(input.dependencies != null && {
                    dependencies: input.dependencies.map((d) => ({
                        routine_id: d.routineId,
                        relationship: mapGqlDependencyRelationship(
                            d.relationship,
                        ),
                        buffer_minutes: d.bufferMinutes ?? undefined,
                    })),
                }),
                ...(input.minimumGapMinutes != null && {
                    minimum_gap_minutes: input.minimumGapMinutes,
                }),
                ...(input.bufferTimeMinutes != null && {
                    buffer_time_minutes: input.bufferTimeMinutes,
                }),
                ...(input.conflictResolution != null && {
                    conflict_resolution: mapGqlConflictResolution(
                        input.conflictResolution,
                    ),
                }),
                ...(input.canBeGrouped != null && {
                    can_be_grouped: input.canBeGrouped,
                }),
                ...(input.preferredBatchSize !== undefined && {
                    preferred_batch_size: input.preferredBatchSize ?? undefined,
                }),
                ...(input.enabled != null && { enabled: input.enabled }),
                ...(input.tags != null && { tags: input.tags }),
            });
            if (!updated) {
                throw new GraphQLError("Routine not found", {
                    extensions: { code: "NOT_FOUND" },
                });
            }
            const gqlRoutine = mapRoutine(updated);
            pubsub.publish("routineUpdated", { routine: gqlRoutine });
            return gqlRoutine;
        },

        deleteRoutine: async (_, { id }, context: GraphQLContext) => {
            const { user } = ensureUser(context);
            const deleted = await routineService.deleteRoutine(id, user.id);
            if (!deleted) {
                throw new GraphQLError("Routine not found", {
                    extensions: { code: "NOT_FOUND" },
                });
            }
            pubsub.publish("routineDeleted", { payload: { id } });
            return true;
        },
    },

    Calendar: {
        events: async (parent, args, context: GraphQLContext) => {
            const { refreshToken, calendarService } = ensureAuth(context);

            try {
                const auth = calendarService.createAuthenticatedClient({
                    refresh_token: refreshToken,
                });
                const result = await calendarService.listEvents(
                    auth,
                    parent.id,
                    args.first || 20,
                    args.after || undefined,
                    args.timeMin || undefined,
                    args.timeMax || undefined,
                );

                const edges = result.items.map((event) => ({
                    cursor: event.id || "",
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
                            responseStatus: mapAttendeeResponse(
                                attendee.responseStatus,
                            ),
                        })),
                        htmlLink: event.htmlLink || "",
                        created: new Date(event.created || Date.now()),
                        updated: new Date(event.updated || Date.now()),
                    },
                }));

                return {
                    edges,
                    pageInfo: {
                        hasNextPage: !!result.nextPageToken,
                        hasPreviousPage: false,
                        startCursor: edges.length > 0 ? edges[0].cursor : null,
                        endCursor:
                            result.nextPageToken ||
                            (edges.length > 0
                                ? edges[edges.length - 1].cursor
                                : null),
                    },
                    totalCount: null,
                };
            } catch (error) {
                throw new GraphQLError(
                    `Failed to fetch calendar events: ${error}`,
                    {
                        extensions: { code: "CALENDAR_ERROR" },
                    },
                );
            }
        },
    },

    Subscription: {
        eventCreated: {
            subscribe: (_, { calendarId }, context: GraphQLContext) => {
                ensureAuth(context);

                return pipe(
                    pubsub.subscribe("eventCreated"),
                    filter(
                        ({ calendarId: eventCalendarId }) =>
                            !calendarId || eventCalendarId === calendarId,
                    ),
                ) as AsyncIterable<{ eventCreated: any }>;
            },
            resolve: (payload: { event: any }) => payload.event,
        },

        eventUpdated: {
            subscribe: (_, { calendarId }, context: GraphQLContext) => {
                ensureAuth(context);

                return pipe(
                    pubsub.subscribe("eventUpdated"),
                    filter(
                        ({ calendarId: eventCalendarId }) =>
                            !calendarId || eventCalendarId === calendarId,
                    ),
                ) as AsyncIterable<{ eventUpdated: any }>;
            },
            resolve: (payload: { event: any }) => payload.event,
        },

        eventDeleted: {
            subscribe: (_, { calendarId }, context: GraphQLContext) => {
                ensureAuth(context);

                return pipe(
                    pubsub.subscribe("eventDeleted"),
                    filter(
                        ({ calendarId: eventCalendarId }) =>
                            !calendarId || eventCalendarId === calendarId,
                    ),
                ) as AsyncIterable<{ eventDeleted: any }>;
            },
            resolve: (payload: { payload: any }) => payload.payload,
        },

        routineCreated: {
            subscribe: (_, __, context: GraphQLContext) => {
                ensureUser(context);
                return pubsub.subscribe("routineCreated") as AsyncIterable<{
                    routine: any;
                }>;
            },
            resolve: (payload: { routine: any }) => payload.routine,
        },

        routineUpdated: {
            subscribe: (_, __, context: GraphQLContext) => {
                ensureUser(context);
                return pubsub.subscribe("routineUpdated") as AsyncIterable<{
                    routine: any;
                }>;
            },
            resolve: (payload: { routine: any }) => payload.routine,
        },

        routineDeleted: {
            subscribe: (_, __, context: GraphQLContext) => {
                ensureUser(context);
                return pubsub.subscribe("routineDeleted") as AsyncIterable<{
                    payload: any;
                }>;
            },
            resolve: (payload: { payload: any }) => payload.payload,
        },
    },
};

// Map a Google Calendar event to a GQL CalendarEvent object
// biome-ignore lint/suspicious/noExplicitAny: gcal schema types are any
function mapGCalEvent(event: any, calendarId: string) {
    return {
        id: event.id || "",
        calendarId,
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
        status: mapEventStatusEnum(event.status),
        visibility: mapEventVisibilityEnum(event.visibility),
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
        attendees: event.attendees?.map(
            // biome-ignore lint/suspicious/noExplicitAny: attendee type
            (attendee: any) => ({
                email: attendee.email || "",
                displayName: attendee.displayName,
                self: attendee.self,
                responseStatus: mapAttendeeResponseEnum(
                    attendee.responseStatus,
                ),
            }),
        ),
        htmlLink: event.htmlLink || "",
        created: new Date(event.created || Date.now()),
        updated: new Date(event.updated || Date.now()),
    };
}

function mapEventStatusEnum(status?: string | null): GqlEventStatus {
    switch (status) {
        case "confirmed":
            return GqlEventStatus.Confirmed;
        case "tentative":
            return GqlEventStatus.Tentative;
        case "cancelled":
            return GqlEventStatus.Cancelled;
        default:
            return GqlEventStatus.Confirmed;
    }
}

function mapEventVisibilityEnum(
    visibility?: string | null,
): GqlEventVisibility {
    switch (visibility) {
        case "default":
            return GqlEventVisibility.Default;
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

function mapAttendeeResponseEnum(
    response?: string | null,
): GqlAttendeeResponseStatus {
    switch (response) {
        case "needsAction":
            return GqlAttendeeResponseStatus.NeedsAction;
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

function mapEventStatus(
    status?: string | null,
): "CONFIRMED" | "TENTATIVE" | "CANCELLED" {
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

function mapEventVisibility(
    visibility?: string | null,
): "DEFAULT" | "PUBLIC" | "PRIVATE" | "CONFIDENTIAL" {
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

function mapAttendeeResponse(
    response?: string | null,
): "NEEDS_ACTION" | "DECLINED" | "TENTATIVE" | "ACCEPTED" {
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

import type { Routine } from "@sunrise/models";

function mapRoutine(routine: Routine) {
    return {
        id: routine.id,
        name: routine.name,
        description: routine.description ?? null,
        duration: {
            minutes: routine.duration.minutes,
            flexible: routine.duration.flexible,
            minDuration: routine.duration.min_duration ?? null,
            maxDuration: routine.duration.max_duration ?? null,
        },
        priority: mapPriorityToGql(routine.priority),
        flexibility: routine.flexibility,
        energyLevelRequired: mapEnergyLevelToGql(routine.energy_level_required),
        category: routine.category,
        frequency: mapFrequencyToGql(routine.frequency),
        timePreferences: routine.time_preferences.map(mapTimeOfDayToGql),
        availabilityWindows: routine.availability_windows.map((w) => ({
            startHour: w.start_hour,
            startMinute: w.start_minute,
            endHour: w.end_hour,
            endMinute: w.end_minute,
        })),
        dependencies: routine.dependencies.map((d) => ({
            routineId: d.routine_id,
            relationship: mapDependencyRelationshipToGql(d.relationship),
            bufferMinutes: d.buffer_minutes ?? null,
        })),
        minimumGapMinutes: routine.minimum_gap_minutes,
        bufferTimeMinutes: routine.buffer_time_minutes,
        conflictResolution: mapConflictResolutionToGql(
            routine.conflict_resolution,
        ),
        canBeGrouped: routine.can_be_grouped,
        preferredBatchSize: routine.preferred_batch_size ?? null,
        enabled: routine.enabled,
        tags: routine.tags,
    };
}

function mapPriorityToGql(p: string): GqlPriorityLevel {
    switch (p) {
        case "high":
            return GqlPriorityLevel.High;
        case "low":
            return GqlPriorityLevel.Low;
        default:
            return GqlPriorityLevel.Medium;
    }
}

function mapGqlPriority(p: GqlPriorityLevel): "high" | "medium" | "low" {
    switch (p) {
        case GqlPriorityLevel.High:
            return "high";
        case GqlPriorityLevel.Low:
            return "low";
        default:
            return "medium";
    }
}

function mapEnergyLevelToGql(e: string): GqlEnergyLevel {
    switch (e) {
        case "high":
            return GqlEnergyLevel.High;
        case "low":
            return GqlEnergyLevel.Low;
        default:
            return GqlEnergyLevel.Medium;
    }
}

function mapGqlEnergyLevel(e: GqlEnergyLevel): "high" | "medium" | "low" {
    switch (e) {
        case GqlEnergyLevel.High:
            return "high";
        case GqlEnergyLevel.Low:
            return "low";
        default:
            return "medium";
    }
}

function mapFrequencyToGql(f: string): GqlFrequency {
    switch (f) {
        case "daily":
            return GqlFrequency.Daily;
        case "weekly":
            return GqlFrequency.Weekly;
        case "weekdays":
            return GqlFrequency.Weekdays;
        case "weekends":
            return GqlFrequency.Weekends;
        default:
            return GqlFrequency.Custom;
    }
}

function mapGqlFrequency(
    f: GqlFrequency,
): "daily" | "weekly" | "weekdays" | "weekends" | "custom" {
    switch (f) {
        case GqlFrequency.Daily:
            return "daily";
        case GqlFrequency.Weekly:
            return "weekly";
        case GqlFrequency.Weekdays:
            return "weekdays";
        case GqlFrequency.Weekends:
            return "weekends";
        default:
            return "custom";
    }
}

function mapTimeOfDayToGql(t: string): GqlTimeOfDay {
    switch (t) {
        case "early_morning":
            return GqlTimeOfDay.EarlyMorning;
        case "morning":
            return GqlTimeOfDay.Morning;
        case "late_morning":
            return GqlTimeOfDay.LateMorning;
        case "midday":
            return GqlTimeOfDay.Midday;
        case "afternoon":
            return GqlTimeOfDay.Afternoon;
        case "evening":
            return GqlTimeOfDay.Evening;
        case "night":
            return GqlTimeOfDay.Night;
        default:
            return GqlTimeOfDay.Flexible;
    }
}

function mapGqlTimeOfDay(
    t: GqlTimeOfDay,
):
    | "early_morning"
    | "morning"
    | "late_morning"
    | "midday"
    | "afternoon"
    | "evening"
    | "night"
    | "flexible" {
    switch (t) {
        case GqlTimeOfDay.EarlyMorning:
            return "early_morning";
        case GqlTimeOfDay.Morning:
            return "morning";
        case GqlTimeOfDay.LateMorning:
            return "late_morning";
        case GqlTimeOfDay.Midday:
            return "midday";
        case GqlTimeOfDay.Afternoon:
            return "afternoon";
        case GqlTimeOfDay.Evening:
            return "evening";
        case GqlTimeOfDay.Night:
            return "night";
        default:
            return "flexible";
    }
}

function mapConflictResolutionToGql(c: string): GqlConflictResolution {
    switch (c) {
        case "skip":
            return GqlConflictResolution.Skip;
        case "compress":
            return GqlConflictResolution.Compress;
        case "override":
            return GqlConflictResolution.Override;
        default:
            return GqlConflictResolution.Reschedule;
    }
}

function mapGqlConflictResolution(
    c: GqlConflictResolution,
): "skip" | "reschedule" | "compress" | "override" {
    switch (c) {
        case GqlConflictResolution.Skip:
            return "skip";
        case GqlConflictResolution.Compress:
            return "compress";
        case GqlConflictResolution.Override:
            return "override";
        default:
            return "reschedule";
    }
}

function mapDependencyRelationshipToGql(r: string): GqlDependencyRelationship {
    switch (r) {
        case "before":
            return GqlDependencyRelationship.Before;
        case "same_time_block":
            return GqlDependencyRelationship.SameTimeBlock;
        default:
            return GqlDependencyRelationship.After;
    }
}

function mapGqlDependencyRelationship(
    r: GqlDependencyRelationship,
): "before" | "after" | "same_time_block" {
    switch (r) {
        case GqlDependencyRelationship.Before:
            return "before";
        case GqlDependencyRelationship.SameTimeBlock:
            return "same_time_block";
        default:
            return "after";
    }
}
