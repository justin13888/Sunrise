import { createFileRoute, useRouter } from '@tanstack/react-router'
import { useEffect, useState } from 'react'
import { useGetEventsQuery, useGetCalendarsQuery, useGetMeQuery } from '../generated/graphql'
import { Button } from '../components/ui/button'

export const Route = createFileRoute('/schedule')({
    component: ScheduleComponent,
})

function ScheduleComponent() {
    const router = useRouter()
    const [selectedCalendarId, setSelectedCalendarId] = useState<string>('primary')
    const [isRedirecting, setIsRedirecting] = useState(false)

    // Memoize time range to prevent refetches on every render
    const [timeRange] = useState(() => ({
        timeMin: new Date().toISOString(),
        timeMax: new Date(Date.now() + 30 * 24 * 60 * 60 * 1000).toISOString() // 30 days
    }))

    // Check if user is authenticated
    const { data: userData, loading: userLoading, error: userError } = useGetMeQuery({
        errorPolicy: 'all',
        // Use cache-first to avoid unnecessary network requests
        // The query will still refetch if needed, but won't spam the API
        fetchPolicy: 'cache-first',
        // Stop polling/refetching if redirecting
        skip: isRedirecting
    })

    // Get user's calendars
    const { data: calendarsData, loading: calendarsLoading } = useGetCalendarsQuery({
        skip: !userData?.me || isRedirecting,
        errorPolicy: 'all'
    })

    // Get events for the selected calendar with pagination
    const { data: eventsData, loading: eventsLoading, refetch: refetchEvents, fetchMore } = useGetEventsQuery({
        variables: {
            first: 20,
            calendarId: selectedCalendarId,
            timeMin: timeRange.timeMin,
            timeMax: timeRange.timeMax
        },
        skip: !userData?.me || isRedirecting,
        errorPolicy: 'all',
        notifyOnNetworkStatusChange: true,
    })

    // Redirect to auth if not authenticated
    useEffect(() => {
        if (userError && !userLoading && !isRedirecting) {
            // Check if it's an authentication error
            const isUnauthenticated = userError.graphQLErrors.some(
                error => error.extensions?.code === 'UNAUTHENTICATED'
            )

            if (isUnauthenticated) {
                console.log('🔒 Unauthenticated - redirecting to /auth')
                setIsRedirecting(true)
                router.navigate({ to: '/auth' })
            }
        }
    }, [userError, userLoading, router, isRedirecting])

    const handleLogout = () => {
        localStorage.removeItem('access_token')
        localStorage.removeItem('refresh_token')
        localStorage.removeItem('user_id')
        router.navigate({ to: '/auth' })
    }

    const loadMore = () => {
        if (!eventsData?.events?.pageInfo?.hasNextPage || eventsLoading) {
            return
        }

        fetchMore({
            variables: {
                after: eventsData.events.pageInfo.endCursor,
            },
            updateQuery: (prev, { fetchMoreResult }) => {
                if (!fetchMoreResult) return prev

                return {
                    events: {
                        ...fetchMoreResult.events,
                        edges: [
                            ...prev.events.edges,
                            ...fetchMoreResult.events.edges,
                        ],
                    },
                }
            },
        })
    }

    const formatDateTime = (dateTime: string | null | undefined, date?: string | null | undefined) => {
        if (dateTime) {
            return new Date(dateTime).toLocaleString()
        }
        if (date) {
            return new Date(date).toLocaleDateString()
        }
        return 'No date'
    }

    if (userLoading || isRedirecting) {
        return (
            <div className="min-h-screen flex items-center justify-center">
                <div className="text-center">
                    <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-gray-900 mx-auto mb-4"></div>
                    <p>{isRedirecting ? 'Redirecting to login...' : 'Loading...'}</p>
                </div>
            </div>
        )
    }

    if (!userData?.me) {
        return (
            <div className="min-h-screen flex items-center justify-center">
                <div className="text-center">
                    <p>Please authenticate to view your calendar.</p>
                    <Button onClick={() => router.navigate({ to: '/auth' })} className="mt-4">
                        Sign In
                    </Button>
                </div>
            </div>
        )
    }

    return (
        <div className="min-h-screen bg-gray-50">
            {/* Header */}
            <header className="bg-white shadow-sm border-b">
                <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8">
                    <div className="flex justify-between items-center py-6">
                        <div>
                            <h1 className="text-3xl font-bold text-gray-900">Schedule</h1>
                            <p className="text-gray-600">Welcome back, {userData.me.name || userData.me.email}!</p>
                        </div>
                        <div className="flex items-center space-x-4">
                            <Button
                                onClick={() => refetchEvents()}
                                variant="outline"
                                disabled={eventsLoading}
                            >
                                {eventsLoading ? 'Refreshing...' : 'Refresh'}
                            </Button>
                            <Button onClick={handleLogout} variant="outline">
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
                            <h2 className="text-lg font-semibold text-gray-900 mb-4">Calendars</h2>
                            {calendarsLoading ? (
                                <div className="space-y-2">
                                    <div className="animate-pulse h-4 bg-gray-200 rounded"></div>
                                    <div className="animate-pulse h-4 bg-gray-200 rounded"></div>
                                </div>
                            ) : (
                                <div className="space-y-2">
                                    {calendarsData?.calendars?.map((calendar) => (
                                        <button
                                            key={calendar.id}
                                            type="button"
                                            onClick={() => setSelectedCalendarId(calendar.id)}
                                            className={`w-full text-left p-3 rounded-md transition-colors ${selectedCalendarId === calendar.id
                                                ? 'bg-blue-50 text-blue-700 border border-blue-200'
                                                : 'hover:bg-gray-50'
                                                }`}
                                        >
                                            <div className="flex items-center">
                                                <div
                                                    className="w-3 h-3 rounded-full mr-3"
                                                    style={{ backgroundColor: calendar.backgroundColor || '#3B82F6' }}
                                                ></div>
                                                <div>
                                                    <p className="font-medium">{calendar.summary}</p>
                                                    {calendar.primary && (
                                                        <p className="text-xs text-gray-500">Primary</p>
                                                    )}
                                                </div>
                                            </div>
                                        </button>
                                    ))}
                                </div>
                            )}
                        </div>
                    </div>

                    {/* Main Content - Events */}
                    <div className="lg:col-span-3">
                        <div className="bg-white rounded-lg shadow">
                            <div className="p-6 border-b">
                                <h2 className="text-lg font-semibold text-gray-900">
                                    Upcoming Events
                                    {calendarsData?.calendars && (
                                        <span className="text-gray-500 font-normal ml-2">
                                            - {calendarsData.calendars.find(c => c.id === selectedCalendarId)?.summary || 'Unknown Calendar'}
                                        </span>
                                    )}
                                </h2>
                            </div>

                            <div className="p-6">
                                {eventsLoading && !eventsData ? (
                                    <div className="space-y-4">
                                        {[1, 2, 3].map((i) => (
                                            <div key={i} className="animate-pulse">
                                                <div className="h-4 bg-gray-200 rounded w-3/4 mb-2"></div>
                                                <div className="h-3 bg-gray-200 rounded w-1/2"></div>
                                            </div>
                                        ))}
                                    </div>
                                ) : eventsData?.events?.edges?.length ? (
                                    <>
                                        <div className="space-y-4">
                                            {eventsData.events.edges.map((edge) => {
                                                const event = edge.node
                                                return (
                                                    <div key={event.id} className="border border-gray-200 rounded-lg p-4 hover:shadow-md transition-shadow">
                                                        <div className="flex justify-between items-start">
                                                            <div className="flex-1">
                                                                <h3 className="font-semibold text-gray-900 text-lg">{event.summary}</h3>
                                                                {event.description && (
                                                                    <p className="text-gray-600 mt-1 text-sm">{event.description}</p>
                                                                )}
                                                                {event.location && (
                                                                    <p className="text-gray-500 text-sm mt-1">📍 {event.location}</p>
                                                                )}
                                                                <div className="flex items-center space-x-4 mt-2 text-sm text-gray-500">
                                                                    <span>🗓️ {formatDateTime(event.start.dateTime, event.start.date)}</span>
                                                                    {event.start.dateTime && event.end.dateTime && (
                                                                        <span>→ {formatDateTime(event.end.dateTime, event.end.date)}</span>
                                                                    )}
                                                                </div>
                                                                {event.attendees && event.attendees.length > 0 && (
                                                                    <div className="mt-2">
                                                                        <p className="text-sm text-gray-500">
                                                                            👥 {event.attendees.length} attendee{event.attendees.length > 1 ? 's' : ''}
                                                                        </p>
                                                                    </div>
                                                                )}
                                                            </div>
                                                            <div className="ml-4">
                                                                <span className={`px-2 py-1 text-xs rounded-full ${event.status === 'CONFIRMED' ? 'bg-green-100 text-green-800' :
                                                                    event.status === 'TENTATIVE' ? 'bg-yellow-100 text-yellow-800' :
                                                                        'bg-red-100 text-red-800'
                                                                    }`}>
                                                                    {event.status.toLowerCase()}
                                                                </span>
                                                            </div>
                                                        </div>
                                                        {event.htmlLink && (
                                                            <div className="mt-3 pt-3 border-t">
                                                                <a
                                                                    href={event.htmlLink}
                                                                    target="_blank"
                                                                    rel="noopener noreferrer"
                                                                    className="text-blue-600 hover:text-blue-800 text-sm"
                                                                >
                                                                    View in Google Calendar →
                                                                </a>
                                                            </div>
                                                        )}
                                                    </div>
                                                )
                                            })}
                                        </div>

                                        {/* Load More Button */}
                                        {eventsData.events.pageInfo.hasNextPage && (
                                            <div className="mt-6 text-center">
                                                <Button
                                                    onClick={loadMore}
                                                    disabled={eventsLoading}
                                                    variant="outline"
                                                    className="w-full"
                                                >
                                                    {eventsLoading ? 'Loading more...' : 'Load More Events'}
                                                </Button>
                                            </div>
                                        )}
                                    </>
                                ) : (
                                    <div className="text-center py-8">
                                        <div className="text-gray-400 text-6xl mb-4">📅</div>
                                        <h3 className="text-gray-900 text-lg font-medium">No upcoming events</h3>
                                        <p className="text-gray-500 mt-2">
                                            You don't have any events scheduled in this calendar.
                                        </p>
                                    </div>
                                )}
                            </div>
                        </div>
                    </div>
                </div>
            </div>
        </div>
    )
}
