import { createPubSub } from "graphql-yoga";
import type {
    GqlCalendarEvent,
    GqlEventDeletedPayload,
} from "../generated/resolvers-types";

export type PubSubEvents = {
    eventCreated: [{ calendarId: string; event: GqlCalendarEvent }];
    eventUpdated: [{ calendarId: string; event: GqlCalendarEvent }];
    eventDeleted: [{ calendarId: string; payload: GqlEventDeletedPayload }];
};

export const pubsub = createPubSub<PubSubEvents>();
