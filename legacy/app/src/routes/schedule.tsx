import { createFileRoute, useRouter } from "@tanstack/react-router";
import { useCallback, useEffect, useState } from "react";
import { Button } from "../components/ui/button";
import type { CalendarEvent } from "../generated/graphql";
import {
    useCreateEventMutation,
    useDeleteEventMutation,
    useGetCalendarsQuery,
    useGetEventsQuery,
    useGetMeQuery,
    useUpdateEventMutation,
} from "../generated/graphql";
import { useEventSubscriptions } from "../hooks/useEventSubscriptions";

interface EventFormData {
    summary: string;
    description: string;
    location: string;
    startDate: string;
    startTime: string;
    endDate: string;
    endTime: string;
}

const emptyForm = (): EventFormData => {
    const now = new Date();
    const inOneHour = new Date(now.getTime() + 60 * 60 * 1000);
    const toDateStr = (d: Date) => d.toISOString().slice(0, 10);
    const toTimeStr = (d: Date) =>
        `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
    return {
        summary: "",
        description: "",
        location: "",
        startDate: toDateStr(now),
        startTime: toTimeStr(now),
        endDate: toDateStr(inOneHour),
        endTime: toTimeStr(inOneHour),
    };
};

function formToDateTime(date: string, time: string): string {
    return new Date(`${date}T${time}:00`).toISOString();
}

function eventToForm(event: CalendarEvent): EventFormData {
    const parseDateTime = (dt?: string | null, d?: string | null) => {
        if (dt) {
            const parsed = new Date(dt);
            return {
                date: parsed.toISOString().slice(0, 10),
                time: `${String(parsed.getHours()).padStart(2, "0")}:${String(parsed.getMinutes()).padStart(2, "0")}`,
            };
        }
        if (d) return { date: d, time: "00:00" };
        return { date: "", time: "" };
    };
    const start = parseDateTime(event.start.dateTime, event.start.date);
    const end = parseDateTime(event.end.dateTime, event.end.date);
    return {
        summary: event.summary,
        description: event.description || "",
        location: event.location || "",
        startDate: start.date,
        startTime: start.time,
        endDate: end.date,
        endTime: end.time,
    };
}

interface EventFormProps {
    initialData: EventFormData;
    onSave: (data: EventFormData) => void;
    onCancel: () => void;
    isSaving: boolean;
    title: string;
}

function EventForm({
    initialData,
    onSave,
    onCancel,
    isSaving,
    title,
}: EventFormProps) {
    const [form, setForm] = useState<EventFormData>(initialData);
    const set = (key: keyof EventFormData, value: string) =>
        setForm((f) => ({ ...f, [key]: value }));

    return (
        <div className="fixed inset-0 bg-black/50 flex items-center justify-center z-50">
            <div className="bg-white rounded-lg shadow-xl p-6 w-full max-w-md mx-4">
                <h2 className="text-lg font-semibold mb-4">{title}</h2>
                <div className="space-y-3">
                    <div>
                        <label
                            htmlFor="ef-summary"
                            className="block text-sm font-medium text-gray-700 mb-1"
                        >
                            Title *
                        </label>
                        <input
                            id="ef-summary"
                            type="text"
                            value={form.summary}
                            onChange={(e) => set("summary", e.target.value)}
                            className="w-full border border-gray-300 rounded-md px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500"
                            placeholder="Event title"
                            required
                        />
                    </div>
                    <div className="grid grid-cols-2 gap-3">
                        <div>
                            <label
                                htmlFor="ef-start-date"
                                className="block text-sm font-medium text-gray-700 mb-1"
                            >
                                Start date
                            </label>
                            <input
                                id="ef-start-date"
                                type="date"
                                value={form.startDate}
                                onChange={(e) =>
                                    set("startDate", e.target.value)
                                }
                                className="w-full border border-gray-300 rounded-md px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500"
                            />
                        </div>
                        <div>
                            <label
                                htmlFor="ef-start-time"
                                className="block text-sm font-medium text-gray-700 mb-1"
                            >
                                Start time
                            </label>
                            <input
                                id="ef-start-time"
                                type="time"
                                value={form.startTime}
                                onChange={(e) =>
                                    set("startTime", e.target.value)
                                }
                                className="w-full border border-gray-300 rounded-md px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500"
                            />
                        </div>
                    </div>
                    <div className="grid grid-cols-2 gap-3">
                        <div>
                            <label
                                htmlFor="ef-end-date"
                                className="block text-sm font-medium text-gray-700 mb-1"
                            >
                                End date
                            </label>
                            <input
                                id="ef-end-date"
                                type="date"
                                value={form.endDate}
                                onChange={(e) => set("endDate", e.target.value)}
                                className="w-full border border-gray-300 rounded-md px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500"
                            />
                        </div>
                        <div>
                            <label
                                htmlFor="ef-end-time"
                                className="block text-sm font-medium text-gray-700 mb-1"
                            >
                                End time
                            </label>
                            <input
                                id="ef-end-time"
                                type="time"
                                value={form.endTime}
                                onChange={(e) => set("endTime", e.target.value)}
                                className="w-full border border-gray-300 rounded-md px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500"
                            />
                        </div>
                    </div>
                    <div>
                        <label
                            htmlFor="ef-location"
                            className="block text-sm font-medium text-gray-700 mb-1"
                        >
                            Location
                        </label>
                        <input
                            id="ef-location"
                            type="text"
                            value={form.location}
                            onChange={(e) => set("location", e.target.value)}
                            className="w-full border border-gray-300 rounded-md px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500"
                            placeholder="Optional location"
                        />
                    </div>
                    <div>
                        <label
                            htmlFor="ef-description"
                            className="block text-sm font-medium text-gray-700 mb-1"
                        >
                            Description
                        </label>
                        <textarea
                            id="ef-description"
                            value={form.description}
                            onChange={(e) => set("description", e.target.value)}
                            rows={3}
                            className="w-full border border-gray-300 rounded-md px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500"
                            placeholder="Optional description"
                        />
                    </div>
                </div>
                <div className="flex gap-3 mt-4 justify-end">
                    <Button
                        variant="outline"
                        onClick={onCancel}
                        disabled={isSaving}
                    >
                        Cancel
                    </Button>
                    <Button
                        onClick={() => onSave(form)}
                        disabled={isSaving || !form.summary.trim()}
                    >
                        {isSaving ? "Saving..." : "Save"}
                    </Button>
                </div>
            </div>
        </div>
    );
}

export const Route = createFileRoute("/schedule")({
    component: ScheduleComponent,
});

function ScheduleComponent() {
    const router = useRouter();
    const [selectedCalendarId, setSelectedCalendarId] =
        useState<string>("primary");
    const [isRedirecting, setIsRedirecting] = useState(false);
    const [showCreateForm, setShowCreateForm] = useState(false);
    const [editingEvent, setEditingEvent] = useState<CalendarEvent | null>(
        null,
    );
    const [deletingEventId, setDeletingEventId] = useState<string | null>(null);

    const [createEvent, { loading: creating }] = useCreateEventMutation();
    const [updateEvent, { loading: updating }] = useUpdateEventMutation();
    const [deleteEvent, { loading: deleting }] = useDeleteEventMutation();

    // Memoize time range to prevent refetches on every render
    const [timeRange] = useState(() => ({
        timeMin: new Date().toISOString(),
        timeMax: new Date(Date.now() + 30 * 24 * 60 * 60 * 1000).toISOString(), // 30 days
    }));

    // Check if user is authenticated
    const {
        data: userData,
        loading: userLoading,
        error: userError,
    } = useGetMeQuery({
        errorPolicy: "all",
        // Use cache-first to avoid unnecessary network requests
        // The query will still refetch if needed, but won't spam the API
        fetchPolicy: "cache-first",
        // Stop polling/refetching if redirecting
        skip: isRedirecting,
    });

    // Get user's calendars
    const { data: calendarsData, loading: calendarsLoading } =
        useGetCalendarsQuery({
            skip: !userData?.me || isRedirecting,
            errorPolicy: "all",
        });

    // Get events for the selected calendar with pagination
    const {
        data: eventsData,
        loading: eventsLoading,
        refetch: refetchEvents,
        fetchMore,
        client,
    } = useGetEventsQuery({
        variables: {
            first: 20,
            calendarId: selectedCalendarId,
            timeMin: timeRange.timeMin,
            timeMax: timeRange.timeMax,
        },
        skip: !userData?.me || isRedirecting,
        errorPolicy: "all",
        notifyOnNetworkStatusChange: true,
    });

    // Subscribe to real-time event updates
    useEventSubscriptions(selectedCalendarId, {
        onEventCreated: useCallback(
            (event: CalendarEvent) => {
                console.log("📅 New event created:", event);
                // Apollo cache will automatically update due to matching ID
                // But we can also manually refetch if needed
                refetchEvents();
            },
            [refetchEvents],
        ),

        onEventUpdated: useCallback(
            (event: CalendarEvent) => {
                console.log("📝 Event updated:", event);
                // Apollo cache will automatically update
                refetchEvents();
            },
            [refetchEvents],
        ),

        onEventDeleted: useCallback(
            (payload: { id: string; calendarId: string }) => {
                console.log("🗑️ Event deleted:", payload);
                // Remove from cache
                client.cache.evict({
                    id: client.cache.identify({
                        __typename: "CalendarEvent",
                        id: payload.id,
                    }),
                });
                client.cache.gc();
            },
            [client],
        ),
    });

    // Redirect to auth if not authenticated
    useEffect(() => {
        if (userError && !userLoading && !isRedirecting) {
            // Check if it's an authentication error
            const isUnauthenticated = userError.graphQLErrors.some(
                (error) => error.extensions?.code === "UNAUTHENTICATED",
            );

            if (isUnauthenticated) {
                console.log("🔒 Unauthenticated - redirecting to /auth");
                setIsRedirecting(true);
                router.navigate({ to: "/auth" });
            }
        }
    }, [userError, userLoading, router, isRedirecting]);

    const handleLogout = () => {
        localStorage.removeItem("access_token");
        router.navigate({ to: "/auth" });
    };

    const loadMore = () => {
        if (!eventsData?.events?.pageInfo?.hasNextPage || eventsLoading) {
            return;
        }

        fetchMore({
            variables: {
                after: eventsData.events.pageInfo.endCursor,
            },
            updateQuery: (prev, { fetchMoreResult }) => {
                if (!fetchMoreResult) return prev;

                return {
                    events: {
                        ...fetchMoreResult.events,
                        edges: [
                            ...prev.events.edges,
                            ...fetchMoreResult.events.edges,
                        ],
                    },
                };
            },
        });
    };

    const handleCreateEvent = async (form: EventFormData) => {
        if (!form.summary.trim()) return;
        await createEvent({
            variables: {
                input: {
                    calendarId: selectedCalendarId,
                    summary: form.summary,
                    description: form.description || undefined,
                    location: form.location || undefined,
                    start: {
                        dateTime: formToDateTime(
                            form.startDate,
                            form.startTime,
                        ),
                    },
                    end: {
                        dateTime: formToDateTime(form.endDate, form.endTime),
                    },
                },
            },
        });
        setShowCreateForm(false);
        refetchEvents();
    };

    const handleUpdateEvent = async (form: EventFormData) => {
        if (!editingEvent || !form.summary.trim()) return;
        await updateEvent({
            variables: {
                id: editingEvent.id,
                input: {
                    summary: form.summary,
                    description: form.description || undefined,
                    location: form.location || undefined,
                    start: {
                        dateTime: formToDateTime(
                            form.startDate,
                            form.startTime,
                        ),
                    },
                    end: {
                        dateTime: formToDateTime(form.endDate, form.endTime),
                    },
                },
            },
        });
        setEditingEvent(null);
        refetchEvents();
    };

    const handleDeleteEvent = async (eventId: string) => {
        await deleteEvent({ variables: { id: eventId } });
        setDeletingEventId(null);
        refetchEvents();
    };

    const formatDateTime = (
        dateTime: string | null | undefined,
        date?: string | null | undefined,
    ) => {
        if (dateTime) {
            return new Date(dateTime).toLocaleString();
        }
        if (date) {
            return new Date(date).toLocaleDateString();
        }
        return "No date";
    };

    // Separate calendars into "My Calendars" and "Other Calendars"
    const myCalendars =
        calendarsData?.calendars
            ?.filter((cal) => cal.accessRole === "OWNER")
            .sort((a, b) => {
                // Primary calendar always comes first
                if (a.primary && !b.primary) return -1;
                if (!a.primary && b.primary) return 1;
                // Then sort alphabetically by summary
                return a.summary.localeCompare(b.summary);
            }) || [];
    const otherCalendars =
        calendarsData?.calendars
            ?.filter((cal) => cal.accessRole !== "OWNER")
            .sort((a, b) => a.summary.localeCompare(b.summary)) || [];

    if (userLoading || isRedirecting) {
        return (
            <div className="min-h-screen flex items-center justify-center">
                <div className="text-center">
                    <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-gray-900 mx-auto mb-4"></div>
                    <p>
                        {isRedirecting
                            ? "Redirecting to login..."
                            : "Loading..."}
                    </p>
                </div>
            </div>
        );
    }

    if (!userData?.me) {
        return (
            <div className="min-h-screen flex items-center justify-center">
                <div className="text-center">
                    <p>Please authenticate to view your calendar.</p>
                    <Button
                        onClick={() => router.navigate({ to: "/auth" })}
                        className="mt-4"
                    >
                        Sign In
                    </Button>
                </div>
            </div>
        );
    }

    return (
        <>
            <div className="min-h-screen bg-gray-50">
                {/* Header */}
                <header className="bg-white shadow-sm border-b">
                    <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8">
                        <div className="flex justify-between items-center py-6">
                            <div>
                                <h1 className="text-3xl font-bold text-gray-900">
                                    Schedule
                                </h1>
                                <p className="text-gray-600">
                                    Welcome back,{" "}
                                    {userData.me.name || userData.me.email}!
                                </p>
                            </div>
                            <div className="flex items-center space-x-4">
                                <Button
                                    onClick={() => refetchEvents()}
                                    variant="outline"
                                    disabled={eventsLoading}
                                >
                                    {eventsLoading
                                        ? "Refreshing..."
                                        : "Refresh"}
                                </Button>
                                <Button
                                    onClick={handleLogout}
                                    variant="outline"
                                >
                                    Sign Out
                                </Button>
                            </div>
                        </div>
                    </div>
                </header>

                <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8 py-8">
                    <div className="grid grid-cols-1 lg:grid-cols-4 gap-8">
                        {/* Sidebar - Calendars */}
                        <div className="lg:col-span-1">
                            <div className="bg-white rounded-lg shadow p-6">
                                <h2 className="text-lg font-semibold text-gray-900 mb-4">
                                    Calendars
                                </h2>
                                {calendarsLoading ? (
                                    <div className="space-y-2">
                                        <div className="animate-pulse h-4 bg-gray-200 rounded"></div>
                                        <div className="animate-pulse h-4 bg-gray-200 rounded"></div>
                                    </div>
                                ) : (
                                    <div className="space-y-6">
                                        {/* My Calendars Section */}
                                        {myCalendars.length > 0 && (
                                            <div>
                                                <h3 className="text-xs font-semibold text-gray-500 uppercase tracking-wider mb-2">
                                                    My Calendars
                                                </h3>
                                                <div className="space-y-1">
                                                    {myCalendars.map(
                                                        (calendar) => (
                                                            <button
                                                                key={
                                                                    calendar.id
                                                                }
                                                                type="button"
                                                                onClick={() =>
                                                                    setSelectedCalendarId(
                                                                        calendar.id,
                                                                    )
                                                                }
                                                                className={`w-full text-left p-3 rounded-md transition-colors ${
                                                                    selectedCalendarId ===
                                                                    calendar.id
                                                                        ? "bg-blue-50 text-blue-700 border border-blue-200"
                                                                        : "hover:bg-gray-50"
                                                                }`}
                                                            >
                                                                <div className="flex items-center">
                                                                    <div
                                                                        className="w-3 h-3 rounded-full mr-3 flex-shrink-0"
                                                                        style={{
                                                                            backgroundColor:
                                                                                calendar.backgroundColor ||
                                                                                "#3B82F6",
                                                                        }}
                                                                    ></div>
                                                                    <div className="flex-1 min-w-0">
                                                                        <p className="font-medium truncate">
                                                                            {
                                                                                calendar.summary
                                                                            }
                                                                        </p>
                                                                        {calendar.primary && (
                                                                            <p className="text-xs text-gray-500">
                                                                                Primary
                                                                            </p>
                                                                        )}
                                                                    </div>
                                                                </div>
                                                            </button>
                                                        ),
                                                    )}
                                                </div>
                                            </div>
                                        )}

                                        {/* Other Calendars Section */}
                                        {otherCalendars.length > 0 && (
                                            <div>
                                                <h3 className="text-xs font-semibold text-gray-500 uppercase tracking-wider mb-2">
                                                    Other Calendars
                                                </h3>
                                                <div className="space-y-1">
                                                    {otherCalendars.map(
                                                        (calendar) => (
                                                            <button
                                                                key={
                                                                    calendar.id
                                                                }
                                                                type="button"
                                                                onClick={() =>
                                                                    setSelectedCalendarId(
                                                                        calendar.id,
                                                                    )
                                                                }
                                                                className={`w-full text-left p-3 rounded-md transition-colors ${
                                                                    selectedCalendarId ===
                                                                    calendar.id
                                                                        ? "bg-blue-50 text-blue-700 border border-blue-200"
                                                                        : "hover:bg-gray-50"
                                                                }`}
                                                            >
                                                                <div className="flex items-center">
                                                                    <div
                                                                        className="w-3 h-3 rounded-full mr-3 flex-shrink-0"
                                                                        style={{
                                                                            backgroundColor:
                                                                                calendar.backgroundColor ||
                                                                                "#9CA3AF",
                                                                        }}
                                                                    ></div>
                                                                    <div className="flex-1 min-w-0">
                                                                        <p className="font-medium truncate">
                                                                            {
                                                                                calendar.summary
                                                                            }
                                                                        </p>
                                                                        <p className="text-xs text-gray-500 capitalize">
                                                                            {calendar.accessRole
                                                                                .toLowerCase()
                                                                                .replace(
                                                                                    "_",
                                                                                    " ",
                                                                                )}
                                                                        </p>
                                                                    </div>
                                                                </div>
                                                            </button>
                                                        ),
                                                    )}
                                                </div>
                                            </div>
                                        )}

                                        {/* Empty State */}
                                        {myCalendars.length === 0 &&
                                            otherCalendars.length === 0 && (
                                                <div className="text-center py-4 text-gray-500 text-sm">
                                                    No calendars found
                                                </div>
                                            )}
                                    </div>
                                )}
                            </div>
                        </div>

                        {/* Main Content - Events */}
                        <div className="lg:col-span-3">
                            <div className="bg-white rounded-lg shadow">
                                <div className="p-6 border-b flex justify-between items-center">
                                    <h2 className="text-lg font-semibold text-gray-900">
                                        Upcoming Events
                                        {calendarsData?.calendars && (
                                            <span className="text-gray-500 font-normal ml-2">
                                                -{" "}
                                                {calendarsData.calendars.find(
                                                    (c) =>
                                                        c.id ===
                                                        selectedCalendarId,
                                                )?.summary ||
                                                    "Unknown Calendar"}
                                            </span>
                                        )}
                                    </h2>
                                    <Button
                                        onClick={() => setShowCreateForm(true)}
                                        disabled={!userData?.me}
                                    >
                                        + New Event
                                    </Button>
                                </div>

                                <div className="p-6">
                                    {eventsLoading && !eventsData ? (
                                        <div className="space-y-4">
                                            {[1, 2, 3].map((i) => (
                                                <div
                                                    key={i}
                                                    className="animate-pulse"
                                                >
                                                    <div className="h-4 bg-gray-200 rounded w-3/4 mb-2"></div>
                                                    <div className="h-3 bg-gray-200 rounded w-1/2"></div>
                                                </div>
                                            ))}
                                        </div>
                                    ) : eventsData?.events?.edges?.length ? (
                                        <>
                                            <div className="space-y-4">
                                                {eventsData.events.edges.map(
                                                    (edge) => {
                                                        const event = edge.node;
                                                        return (
                                                            <div
                                                                key={event.id}
                                                                className="border border-gray-200 rounded-lg p-4 hover:shadow-md transition-shadow"
                                                            >
                                                                <div className="flex justify-between items-start">
                                                                    <div className="flex-1">
                                                                        <h3 className="font-semibold text-gray-900 text-lg">
                                                                            {
                                                                                event.summary
                                                                            }
                                                                        </h3>
                                                                        {event.description && (
                                                                            <p className="text-gray-600 mt-1 text-sm">
                                                                                {
                                                                                    event.description
                                                                                }
                                                                            </p>
                                                                        )}
                                                                        {event.location && (
                                                                            <p className="text-gray-500 text-sm mt-1">
                                                                                📍{" "}
                                                                                {
                                                                                    event.location
                                                                                }
                                                                            </p>
                                                                        )}
                                                                        <div className="flex items-center space-x-4 mt-2 text-sm text-gray-500">
                                                                            <span>
                                                                                🗓️{" "}
                                                                                {formatDateTime(
                                                                                    event
                                                                                        .start
                                                                                        .dateTime,
                                                                                    event
                                                                                        .start
                                                                                        .date,
                                                                                )}
                                                                            </span>
                                                                            {event
                                                                                .start
                                                                                .dateTime &&
                                                                                event
                                                                                    .end
                                                                                    .dateTime && (
                                                                                    <span>
                                                                                        →{" "}
                                                                                        {formatDateTime(
                                                                                            event
                                                                                                .end
                                                                                                .dateTime,
                                                                                            event
                                                                                                .end
                                                                                                .date,
                                                                                        )}
                                                                                    </span>
                                                                                )}
                                                                        </div>
                                                                        {event.attendees &&
                                                                            event
                                                                                .attendees
                                                                                .length >
                                                                                0 && (
                                                                                <div className="mt-2">
                                                                                    <p className="text-sm text-gray-500">
                                                                                        👥{" "}
                                                                                        {
                                                                                            event
                                                                                                .attendees
                                                                                                .length
                                                                                        }{" "}
                                                                                        attendee
                                                                                        {event
                                                                                            .attendees
                                                                                            .length >
                                                                                        1
                                                                                            ? "s"
                                                                                            : ""}
                                                                                    </p>
                                                                                </div>
                                                                            )}
                                                                    </div>
                                                                    <div className="ml-4 flex flex-col gap-2 items-end">
                                                                        <span
                                                                            className={`px-2 py-1 text-xs rounded-full ${
                                                                                event.status ===
                                                                                "CONFIRMED"
                                                                                    ? "bg-green-100 text-green-800"
                                                                                    : event.status ===
                                                                                        "TENTATIVE"
                                                                                      ? "bg-yellow-100 text-yellow-800"
                                                                                      : "bg-red-100 text-red-800"
                                                                            }`}
                                                                        >
                                                                            {event.status.toLowerCase()}
                                                                        </span>
                                                                        <div className="flex gap-1">
                                                                            <button
                                                                                type="button"
                                                                                onClick={() =>
                                                                                    setEditingEvent(
                                                                                        event,
                                                                                    )
                                                                                }
                                                                                className="px-2 py-1 text-xs text-blue-600 hover:bg-blue-50 rounded"
                                                                            >
                                                                                Edit
                                                                            </button>
                                                                            <button
                                                                                type="button"
                                                                                onClick={() =>
                                                                                    setDeletingEventId(
                                                                                        event.id,
                                                                                    )
                                                                                }
                                                                                className="px-2 py-1 text-xs text-red-600 hover:bg-red-50 rounded"
                                                                            >
                                                                                Delete
                                                                            </button>
                                                                        </div>
                                                                    </div>
                                                                </div>
                                                                {event.htmlLink && (
                                                                    <div className="mt-3 pt-3 border-t">
                                                                        <a
                                                                            href={
                                                                                event.htmlLink
                                                                            }
                                                                            target="_blank"
                                                                            rel="noopener noreferrer"
                                                                            className="text-blue-600 hover:text-blue-800 text-sm"
                                                                        >
                                                                            View
                                                                            in
                                                                            Google
                                                                            Calendar
                                                                            →
                                                                        </a>
                                                                    </div>
                                                                )}
                                                            </div>
                                                        );
                                                    },
                                                )}
                                            </div>

                                            {/* Load More Button */}
                                            {eventsData.events.pageInfo
                                                .hasNextPage && (
                                                <div className="mt-6 text-center">
                                                    <Button
                                                        onClick={loadMore}
                                                        disabled={eventsLoading}
                                                        variant="outline"
                                                        className="w-full"
                                                    >
                                                        {eventsLoading
                                                            ? "Loading more..."
                                                            : "Load More Events"}
                                                    </Button>
                                                </div>
                                            )}
                                        </>
                                    ) : (
                                        <div className="text-center py-8">
                                            <div className="text-gray-400 text-6xl mb-4">
                                                📅
                                            </div>
                                            <h3 className="text-gray-900 text-lg font-medium">
                                                No upcoming events
                                            </h3>
                                            <p className="text-gray-500 mt-2">
                                                You don't have any events
                                                scheduled in this calendar.
                                            </p>
                                        </div>
                                    )}
                                </div>
                            </div>
                        </div>
                    </div>
                </div>
            </div>

            {/* Create event modal */}
            {showCreateForm && (
                <EventForm
                    title="New Event"
                    initialData={emptyForm()}
                    onSave={handleCreateEvent}
                    onCancel={() => setShowCreateForm(false)}
                    isSaving={creating}
                />
            )}

            {/* Edit event modal */}
            {editingEvent && (
                <EventForm
                    title="Edit Event"
                    initialData={eventToForm(editingEvent)}
                    onSave={handleUpdateEvent}
                    onCancel={() => setEditingEvent(null)}
                    isSaving={updating}
                />
            )}

            {/* Delete confirmation */}
            {deletingEventId && (
                <div className="fixed inset-0 bg-black/50 flex items-center justify-center z-50">
                    <div className="bg-white rounded-lg shadow-xl p-6 w-full max-w-sm mx-4">
                        <h2 className="text-lg font-semibold mb-2">
                            Delete Event
                        </h2>
                        <p className="text-gray-600 text-sm mb-4">
                            Are you sure you want to delete this event? This
                            cannot be undone.
                        </p>
                        <div className="flex gap-3 justify-end">
                            <Button
                                variant="outline"
                                onClick={() => setDeletingEventId(null)}
                                disabled={deleting}
                            >
                                Cancel
                            </Button>
                            <Button
                                onClick={() =>
                                    handleDeleteEvent(deletingEventId)
                                }
                                disabled={deleting}
                                className="bg-red-600 hover:bg-red-700 text-white"
                            >
                                {deleting ? "Deleting..." : "Delete"}
                            </Button>
                        </div>
                    </div>
                </div>
            )}
        </>
    );
}
