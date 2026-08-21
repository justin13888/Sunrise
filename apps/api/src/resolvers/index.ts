/** biome-ignore-all lint/suspicious/noExplicitAny: Resolve generally don't know about a specific type */
import type { calendar_v3 } from "@sunrise/gcal";
import type { Routine } from "@sunrise/models";
import { GraphQLError } from "graphql";
import { filter, pipe } from "graphql-yoga";
import type { GraphQLContext } from "../context";
import {
    GqlConflictResolution,
    GqlDependencyRelationship,
    GqlEnergyLevel,
    GqlFrequency,
    GqlPriorityLevel,
    type GqlResolvers,
    GqlTimeOfDay,
} from "../generated/resolvers-types";
import { signJWT } from "../services/jwt";
import { pubsub } from "../services/pubsub";
import {
    RoutineValidationError,
    routineService,
} from "../services/routineService";
import { tokenStore } from "../services/tokenStore";
import { ensureAuth, ensureUser } from "./auth";
import { DateTimeScalar } from "./date-time";
import { mapGCalCalendar, mapGCalEvent, mapReminderInput } from "./mappers";
import { URLScalar } from "./url";

/** Opaque per-edge cursor. Pagination itself uses Google page tokens: pass
 * `pageInfo.endCursor` as `after` to fetch the next page. */
function toCursor(eventId: string): string {
    return Buffer.from(`gcal:${eventId}`).toString("base64");
}

/** Google caps events.list maxResults at 250. */
const MAX_PAGE_SIZE = 250;

/** Clamp the requested page size to Google's supported 1..250 range. */
function clampFirst(first: number | null | undefined): number {
    return Math.min(Math.max(first ?? 20, 1), MAX_PAGE_SIZE);
}

/**
 * Guard against edge cursors being passed as `after`: pagination uses Google
 * page tokens (pageInfo.endCursor), not the per-edge cursors, and forwarding
 * an edge cursor would send junk to Google.
 */
function assertNotEdgeCursor(after: string): void {
    let decoded: string | undefined;
    try {
        decoded = Buffer.from(after, "base64").toString("utf8");
    } catch {
        // Not base64-decodable: cannot be one of our edge cursors.
        return;
    }
    if (decoded?.startsWith("gcal:")) {
        throw new GraphQLError(
            "The `after` argument must be pageInfo.endCursor, not an edge cursor",
            { extensions: { code: "BAD_USER_INPUT" } },
        );
    }
}

/** Map RoutineValidationError to BAD_USER_INPUT; rethrow everything else. */
function rethrowRoutineError(error: unknown): never {
    if (error instanceof RoutineValidationError) {
        throw new GraphQLError(error.message, {
            extensions: { code: "BAD_USER_INPUT" },
        });
    }
    throw error;
}

/** Convert a DateTime scalar value (Date or ISO string) for the Google API. */
function toIso(value?: Date | string | null): string | undefined {
    return value ? new Date(value).toISOString() : undefined;
}

function isNotFoundError(error: unknown): boolean {
    if (typeof error !== "object" || error === null) return false;
    const e = error as {
        code?: unknown;
        status?: unknown;
        response?: { status?: unknown };
    };
    return e.code === 404 || e.status === 404 || e.response?.status === 404;
}

function toEventConnection(
    items: calendar_v3.Schema$Event[],
    nextPageToken: string | null | undefined,
    calendarId: string,
) {
    const edges = items.map((event) => ({
        cursor: toCursor(event.id || ""),
        node: mapGCalEvent(event, calendarId),
    }));

    return {
        edges,
        pageInfo: {
            hasNextPage: !!nextPageToken,
            hasPreviousPage: false, // Google Calendar API doesn't support backward pagination
            startCursor: edges.length > 0 ? edges[0].cursor : null,
            endCursor: nextPageToken ?? null,
        },
        totalCount: null, // Google Calendar API doesn't provide total count
    };
}

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
                return calendars.items.map(mapGCalCalendar);
            } catch (error) {
                if (error instanceof GraphQLError) throw error;
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

                return mapGCalCalendar(calendar);
            } catch (error) {
                if (error instanceof GraphQLError) throw error;
                throw new GraphQLError(`Failed to fetch calendar: ${error}`, {
                    extensions: { code: "CALENDAR_ERROR" },
                });
            }
        },

        events: async (_, args, context: GraphQLContext) => {
            const { refreshToken, calendarService } = ensureAuth(context);
            const calendarId = args.calendarId || "primary";
            if (args.after) assertNotEdgeCursor(args.after);

            // "Upcoming by default": when no window is given at all, start
            // the listing at now. An explicit timeMin/timeMax wins.
            const timeMin =
                args.timeMin == null && args.timeMax == null
                    ? new Date().toISOString()
                    : toIso(args.timeMin);

            try {
                const auth = calendarService.createAuthenticatedClient({
                    refresh_token: refreshToken,
                });
                const result = await calendarService.listEvents(
                    auth,
                    calendarId,
                    clampFirst(args.first),
                    args.after || undefined,
                    timeMin,
                    toIso(args.timeMax),
                    args.orderBy === "UPDATED" ? "updated" : "startTime",
                );

                return toEventConnection(
                    result.items,
                    result.nextPageToken,
                    calendarId,
                );
            } catch (error) {
                if (error instanceof GraphQLError) throw error;
                throw new GraphQLError(`Failed to fetch events: ${error}`, {
                    extensions: { code: "CALENDAR_ERROR" },
                });
            }
        },

        event: async (_, { id, calendarId }, context: GraphQLContext) => {
            const { refreshToken, calendarService } = ensureAuth(context);
            const targetCalendarId = calendarId || "primary";

            try {
                const auth = calendarService.createAuthenticatedClient({
                    refresh_token: refreshToken,
                });
                const event = await calendarService.getEvent(
                    auth,
                    targetCalendarId,
                    id,
                );

                if (!event) {
                    throw new GraphQLError("Event not found", {
                        extensions: { code: "NOT_FOUND" },
                    });
                }

                return mapGCalEvent(event, targetCalendarId);
            } catch (error) {
                if (error instanceof GraphQLError) throw error;
                if (isNotFoundError(error)) {
                    throw new GraphQLError("Event not found", {
                        extensions: { code: "NOT_FOUND" },
                    });
                }
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
                        { extensions: { code: "AUTH_ERROR" } },
                    );
                }

                // Get user info from Google using the freshly issued tokens.
                // IMPORTANT: This must succeed - we don't use fallback values.
                let userInfo: {
                    email: string;
                    name: string;
                    picture?: string;
                };

                try {
                    const auth = calendarService.createAuthenticatedClient({
                        access_token: tokens.access_token,
                        refresh_token: tokens.refresh_token,
                    });
                    const info = await calendarService.getUserInfo(auth);

                    userInfo = {
                        email: info.email,
                        // Use email as name if name not provided
                        name: info.name || info.email,
                        picture: info.picture,
                    };

                    console.log(
                        "✅ Fetched user info from Google:",
                        userInfo.email,
                    );
                } catch (userInfoError) {
                    console.error(
                        "❌ Could not fetch user info from Google:",
                        userInfoError,
                    );
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

                // Use the email as a stable user ID (in production, this would be a database ID).
                // base64url is URL-safe and collision-free (no characters stripped).
                const userId = `google-${Buffer.from(userInfo.email).toString("base64url")}`;

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
                if (error instanceof GraphQLError) throw error;
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
                if (error instanceof GraphQLError) throw error;
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
            const { user, refreshToken, calendarService } = ensureAuth(context);

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
                            dateTime: toIso(input.start.dateTime),
                            date: input.start.date || undefined,
                            timeZone: input.start.timeZone || undefined,
                        },
                        end: {
                            dateTime: toIso(input.end.dateTime),
                            date: input.end.date || undefined,
                            timeZone: input.end.timeZone || undefined,
                        },
                        attendees: input.attendees?.map((email: string) => ({
                            email,
                        })),
                        reminders: mapReminderInput(input.reminders),
                    },
                );

                const gqlEvent = mapGCalEvent(event, input.calendarId);
                pubsub.publish("eventCreated", {
                    userId: user.id,
                    calendarId: input.calendarId,
                    event: gqlEvent,
                });
                return gqlEvent;
            } catch (error) {
                if (error instanceof GraphQLError) throw error;
                throw new GraphQLError(`Failed to create event: ${error}`, {
                    extensions: { code: "CALENDAR_ERROR" },
                });
            }
        },

        updateEvent: async (_, { id, input }, context: GraphQLContext) => {
            const { user, refreshToken, calendarService } = ensureAuth(context);
            const calendarId = input.calendarId || "primary";

            try {
                const auth = calendarService.createAuthenticatedClient({
                    refresh_token: refreshToken,
                });

                const eventData: calendar_v3.Schema$Event = {};
                if (input.summary !== undefined && input.summary !== null)
                    eventData.summary = input.summary;
                if (input.description !== undefined)
                    eventData.description = input.description;
                if (input.location !== undefined)
                    eventData.location = input.location;
                if (input.start)
                    eventData.start = {
                        dateTime: toIso(input.start.dateTime),
                        date: input.start.date || undefined,
                        timeZone: input.start.timeZone || undefined,
                    };
                if (input.end)
                    eventData.end = {
                        dateTime: toIso(input.end.dateTime),
                        date: input.end.date || undefined,
                        timeZone: input.end.timeZone || undefined,
                    };
                if (input.attendees)
                    eventData.attendees = input.attendees.map(
                        (email: string) => ({ email }),
                    );
                if (input.reminders)
                    eventData.reminders = mapReminderInput(input.reminders);

                const event = await calendarService.updateEvent(
                    auth,
                    calendarId,
                    id,
                    eventData,
                );

                const gqlEvent = mapGCalEvent(event, calendarId);
                pubsub.publish("eventUpdated", {
                    userId: user.id,
                    calendarId,
                    event: gqlEvent,
                });
                return gqlEvent;
            } catch (error) {
                if (error instanceof GraphQLError) throw error;
                throw new GraphQLError(`Failed to update event: ${error}`, {
                    extensions: { code: "CALENDAR_ERROR" },
                });
            }
        },

        deleteEvent: async (_, { id, calendarId }, context: GraphQLContext) => {
            const { user, refreshToken, calendarService } = ensureAuth(context);
            const targetCalendarId = calendarId || "primary";

            try {
                const auth = calendarService.createAuthenticatedClient({
                    refresh_token: refreshToken,
                });
                await calendarService.deleteEvent(auth, targetCalendarId, id);
                pubsub.publish("eventDeleted", {
                    userId: user.id,
                    calendarId: targetCalendarId,
                    payload: { id, calendarId: targetCalendarId },
                });
                return true;
            } catch (error) {
                if (error instanceof GraphQLError) throw error;
                throw new GraphQLError(`Failed to delete event: ${error}`, {
                    extensions: { code: "CALENDAR_ERROR" },
                });
            }
        },

        createRoutine: async (_, { input }, context: GraphQLContext) => {
            const { user } = ensureUser(context);
            let routine: Routine;
            try {
                routine = await routineService.createRoutine(user.id, {
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
                    time_preferences:
                        input.timePreferences.map(mapGqlTimeOfDay),
                    availability_windows: input.availabilityWindows.map(
                        (w) => ({
                            start_hour: w.startHour,
                            start_minute: w.startMinute,
                            end_hour: w.endHour,
                            end_minute: w.endMinute,
                        }),
                    ),
                    dependencies: input.dependencies.map((d) => ({
                        routine_id: d.routineId,
                        relationship: mapGqlDependencyRelationship(
                            d.relationship,
                        ),
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
            } catch (error) {
                rethrowRoutineError(error);
            }
            const gqlRoutine = mapRoutine(routine);
            pubsub.publish("routineCreated", {
                userId: user.id,
                routine: gqlRoutine,
            });
            return gqlRoutine;
        },

        updateRoutine: async (_, { id, input }, context: GraphQLContext) => {
            const { user } = ensureUser(context);
            let updated: Routine | null;
            try {
                updated = await routineService.updateRoutine(id, user.id, {
                    ...(input.name != null && { name: input.name }),
                    // Explicit null clears the description; undefined leaves it.
                    ...(input.description !== undefined && {
                        description: input.description,
                    }),
                    ...(input.duration != null && {
                        duration: {
                            minutes: input.duration.minutes,
                            flexible: input.duration.flexible,
                            min_duration:
                                input.duration.minDuration ?? undefined,
                            max_duration:
                                input.duration.maxDuration ?? undefined,
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
                    // Explicit null clears the batch size; undefined leaves it.
                    ...(input.preferredBatchSize !== undefined && {
                        preferred_batch_size: input.preferredBatchSize,
                    }),
                    ...(input.enabled != null && { enabled: input.enabled }),
                    ...(input.tags != null && { tags: input.tags }),
                });
            } catch (error) {
                rethrowRoutineError(error);
            }
            if (!updated) {
                throw new GraphQLError("Routine not found", {
                    extensions: { code: "NOT_FOUND" },
                });
            }
            const gqlRoutine = mapRoutine(updated);
            pubsub.publish("routineUpdated", {
                userId: user.id,
                routine: gqlRoutine,
            });
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
            pubsub.publish("routineDeleted", {
                userId: user.id,
                payload: { id },
            });
            return true;
        },
    },

    Calendar: {
        events: async (parent, args, context: GraphQLContext) => {
            const { refreshToken, calendarService } = ensureAuth(context);
            if (args.after) assertNotEdgeCursor(args.after);

            const timeMin =
                args.timeMin == null && args.timeMax == null
                    ? new Date().toISOString()
                    : toIso(args.timeMin);

            try {
                const auth = calendarService.createAuthenticatedClient({
                    refresh_token: refreshToken,
                });
                const result = await calendarService.listEvents(
                    auth,
                    parent.id,
                    clampFirst(args.first),
                    args.after || undefined,
                    timeMin,
                    toIso(args.timeMax),
                );

                return toEventConnection(
                    result.items,
                    result.nextPageToken,
                    parent.id,
                );
            } catch (error) {
                if (error instanceof GraphQLError) throw error;
                throw new GraphQLError(
                    `Failed to fetch calendar events: ${error}`,
                    {
                        extensions: { code: "CALENDAR_ERROR" },
                    },
                );
            }
        },
    },

    // All subscriptions are user-scoped: every published payload carries the
    // owning userId and each subscriber only receives their own events.
    Subscription: {
        eventCreated: {
            subscribe: (_, { calendarId }, context: GraphQLContext) => {
                const { user } = ensureAuth(context);

                return pipe(
                    pubsub.subscribe("eventCreated"),
                    filter(
                        (payload) =>
                            payload.userId === user.id &&
                            (!calendarId || payload.calendarId === calendarId),
                    ),
                ) as AsyncIterable<{ eventCreated: any }>;
            },
            resolve: (payload: { event: any }) => payload.event,
        },

        eventUpdated: {
            subscribe: (_, { calendarId }, context: GraphQLContext) => {
                const { user } = ensureAuth(context);

                return pipe(
                    pubsub.subscribe("eventUpdated"),
                    filter(
                        (payload) =>
                            payload.userId === user.id &&
                            (!calendarId || payload.calendarId === calendarId),
                    ),
                ) as AsyncIterable<{ eventUpdated: any }>;
            },
            resolve: (payload: { event: any }) => payload.event,
        },

        eventDeleted: {
            subscribe: (_, { calendarId }, context: GraphQLContext) => {
                const { user } = ensureAuth(context);

                return pipe(
                    pubsub.subscribe("eventDeleted"),
                    filter(
                        (payload) =>
                            payload.userId === user.id &&
                            (!calendarId || payload.calendarId === calendarId),
                    ),
                ) as AsyncIterable<{ eventDeleted: any }>;
            },
            resolve: (payload: { payload: any }) => payload.payload,
        },

        routineCreated: {
            subscribe: (_, __, context: GraphQLContext) => {
                const { user } = ensureUser(context);
                return pipe(
                    pubsub.subscribe("routineCreated"),
                    filter((payload) => payload.userId === user.id),
                ) as AsyncIterable<{ routine: any }>;
            },
            resolve: (payload: { routine: any }) => payload.routine,
        },

        routineUpdated: {
            subscribe: (_, __, context: GraphQLContext) => {
                const { user } = ensureUser(context);
                return pipe(
                    pubsub.subscribe("routineUpdated"),
                    filter((payload) => payload.userId === user.id),
                ) as AsyncIterable<{ routine: any }>;
            },
            resolve: (payload: { routine: any }) => payload.routine,
        },

        routineDeleted: {
            subscribe: (_, __, context: GraphQLContext) => {
                const { user } = ensureUser(context);
                return pipe(
                    pubsub.subscribe("routineDeleted"),
                    filter((payload) => payload.userId === user.id),
                ) as AsyncIterable<{ payload: any }>;
            },
            resolve: (payload: { payload: any }) => payload.payload,
        },
    },
};

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
