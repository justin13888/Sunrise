import { createPubSub } from 'graphql-yoga'
import type { CalendarEvent, EventDeletedPayload } from '../generated/resolvers-types'

export type PubSubEvents = {
    eventCreated: [{ calendarId: string; event: CalendarEvent }]
    eventUpdated: [{ calendarId: string; event: CalendarEvent }]
    eventDeleted: [{ calendarId: string; payload: EventDeletedPayload }]
}

export const pubsub = createPubSub<PubSubEvents>()
