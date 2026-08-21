import type { CalendarEvent } from "../generated/graphql";
import {
    useOnEventCreatedSubscription,
    useOnEventDeletedSubscription,
    useOnEventUpdatedSubscription,
} from "../generated/graphql";

interface EventSubscriptionHookResult {
    loading: boolean;
    error?: Error;
}

/**
 * Subscribes to event created/updated/deleted for the given calendar.
 * Subscriptions are skipped until a calendarId is provided (the server
 * publishes real calendar ids, so pass the resolved id, not "primary").
 */
export function useEventSubscriptions(
    calendarId?: string,
    options?: {
        onEventCreated?: (event: CalendarEvent) => void;
        onEventUpdated?: (event: CalendarEvent) => void;
        onEventDeleted?: (payload: { id: string; calendarId: string }) => void;
    },
): EventSubscriptionHookResult {
    const { loading: createdLoading, error: createdError } =
        useOnEventCreatedSubscription({
            variables: { calendarId },
            skip: !calendarId,
            onData: ({ data }) => {
                if (data.data?.eventCreated && options?.onEventCreated) {
                    options.onEventCreated(
                        data.data.eventCreated as CalendarEvent,
                    );
                }
            },
        });

    const { loading: updatedLoading, error: updatedError } =
        useOnEventUpdatedSubscription({
            variables: { calendarId },
            skip: !calendarId,
            onData: ({ data }) => {
                if (data.data?.eventUpdated && options?.onEventUpdated) {
                    options.onEventUpdated(
                        data.data.eventUpdated as CalendarEvent,
                    );
                }
            },
        });

    const { loading: deletedLoading, error: deletedError } =
        useOnEventDeletedSubscription({
            variables: { calendarId },
            skip: !calendarId,
            onData: ({ data }) => {
                if (data.data?.eventDeleted && options?.onEventDeleted) {
                    options.onEventDeleted(data.data.eventDeleted);
                }
            },
        });

    return {
        loading: createdLoading || updatedLoading || deletedLoading,
        error: createdError || updatedError || deletedError,
    };
}
