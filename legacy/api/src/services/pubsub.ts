import { createPubSub } from "graphql-yoga";
import type {
    GqlCalendarEvent,
    GqlEventDeletedPayload,
    GqlRoutine,
    GqlRoutineDeletedPayload,
} from "../generated/resolvers-types";

export type PubSubEvents = {
    eventCreated: [{ calendarId: string; event: GqlCalendarEvent }];
    eventUpdated: [{ calendarId: string; event: GqlCalendarEvent }];
    eventDeleted: [{ calendarId: string; payload: GqlEventDeletedPayload }];
    routineCreated: [{ routine: GqlRoutine }];
    routineUpdated: [{ routine: GqlRoutine }];
    routineDeleted: [{ payload: GqlRoutineDeletedPayload }];
};

export const pubsub = createPubSub<PubSubEvents>();
