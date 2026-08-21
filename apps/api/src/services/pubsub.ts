import { createPubSub } from "graphql-yoga";
import type {
    GqlCalendarEvent,
    GqlEventDeletedPayload,
    GqlRoutine,
    GqlRoutineDeletedPayload,
} from "../generated/resolvers-types";

export type PubSubEvents = {
    eventCreated: [
        { userId: string; calendarId: string; event: GqlCalendarEvent },
    ];
    eventUpdated: [
        { userId: string; calendarId: string; event: GqlCalendarEvent },
    ];
    eventDeleted: [
        {
            userId: string;
            calendarId: string;
            payload: GqlEventDeletedPayload;
        },
    ];
    routineCreated: [{ userId: string; routine: GqlRoutine }];
    routineUpdated: [{ userId: string; routine: GqlRoutine }];
    routineDeleted: [{ userId: string; payload: GqlRoutineDeletedPayload }];
};

export const pubsub = createPubSub<PubSubEvents>();
