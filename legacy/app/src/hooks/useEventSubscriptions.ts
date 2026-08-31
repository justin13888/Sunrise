import { gql, useSubscription } from "@apollo/client";
import type { CalendarEvent } from "../generated/graphql";

const EVENT_CREATED_SUBSCRIPTION = gql`
  subscription OnEventCreated($calendarId: ID) {
    eventCreated(calendarId: $calendarId) {
      id
      calendarId
      summary
      description
      location
      start {
        dateTime
        date
        timeZone
      }
      end {
        dateTime
        date
        timeZone
      }
      status
      htmlLink
    }
  }
`;

const EVENT_UPDATED_SUBSCRIPTION = gql`
  subscription OnEventUpdated($calendarId: ID) {
    eventUpdated(calendarId: $calendarId) {
      id
      calendarId
      summary
      description
      location
      start {
        dateTime
        date
        timeZone
      }
      end {
        dateTime
        date
        timeZone
      }
      status
      htmlLink
    }
  }
`;

const EVENT_DELETED_SUBSCRIPTION = gql`
  subscription OnEventDeleted($calendarId: ID) {
    eventDeleted(calendarId: $calendarId) {
      id
      calendarId
    }
  }
`;

interface EventSubscriptionHookResult {
    loading: boolean;
    error?: Error;
}

export function useEventSubscriptions(
    calendarId?: string,
    options?: {
        onEventCreated?: (event: CalendarEvent) => void;
        onEventUpdated?: (event: CalendarEvent) => void;
        onEventDeleted?: (payload: { id: string; calendarId: string }) => void;
    },
): EventSubscriptionHookResult {
    // Subscribe to event created
    const { loading: createdLoading, error: createdError } = useSubscription(
        EVENT_CREATED_SUBSCRIPTION,
        {
            variables: { calendarId },
            onData: ({ data }) => {
                if (data.data?.eventCreated && options?.onEventCreated) {
                    options.onEventCreated(
                        data.data.eventCreated as CalendarEvent,
                    );
                }
            },
            skip: !calendarId,
        },
    );

    // Subscribe to event updated
    const { loading: updatedLoading, error: updatedError } = useSubscription(
        EVENT_UPDATED_SUBSCRIPTION,
        {
            variables: { calendarId },
            onData: ({ data }) => {
                if (data.data?.eventUpdated && options?.onEventUpdated) {
                    options.onEventUpdated(
                        data.data.eventUpdated as CalendarEvent,
                    );
                }
            },
            skip: !calendarId,
        },
    );

    // Subscribe to event deleted
    const { loading: deletedLoading, error: deletedError } = useSubscription(
        EVENT_DELETED_SUBSCRIPTION,
        {
            variables: { calendarId },
            onData: ({ data }) => {
                if (data.data?.eventDeleted && options?.onEventDeleted) {
                    options.onEventDeleted(data.data.eventDeleted);
                }
            },
            skip: !calendarId,
        },
    );

    return {
        loading: createdLoading || updatedLoading || deletedLoading,
        error: createdError || updatedError || deletedError,
    };
}
