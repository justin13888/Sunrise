import { randomUUID } from "node:crypto";
import {
    type ConflictResolution,
    type Dependency,
    type Duration,
    type EnergyLevel,
    type Frequency,
    type PriorityLevel,
    type Routine,
    routineValidator,
    type TimeOfDay,
    type TimeWindow,
} from "@sunrise/models";
import { and, eq } from "drizzle-orm";
import { db } from "../db";
import { routines } from "../db/schema";

export interface CreateRoutineInput {
    name: string;
    description?: string;
    duration: Duration;
    priority: PriorityLevel;
    flexibility: number;
    energy_level_required: EnergyLevel;
    category: string;
    frequency: Frequency;
    time_preferences: TimeOfDay[];
    availability_windows: TimeWindow[];
    dependencies: Dependency[];
    minimum_gap_minutes: number;
    buffer_time_minutes: number;
    conflict_resolution: ConflictResolution;
    can_be_grouped: boolean;
    preferred_batch_size?: number;
    enabled: boolean;
    tags: string[];
}

/**
 * Update patch. `undefined` means "leave unchanged"; an explicit `null` on
 * the nullable fields (description, preferred_batch_size) clears the value.
 */
export type UpdateRoutineInput = Omit<
    Partial<CreateRoutineInput>,
    "description" | "preferred_batch_size"
> & {
    description?: string | null;
    preferred_batch_size?: number | null;
};

/**
 * Raised when routine input fails validation. Resolvers translate this into
 * a BAD_USER_INPUT GraphQL error; plain Errors remain internal (masked).
 */
export class RoutineValidationError extends Error {
    constructor(message: string) {
        super(message);
        this.name = "RoutineValidationError";
    }
}

function describeValidationProblems(value: unknown): string {
    return routineValidator
        .errors(value)
        .map(
            (issue) =>
                `${issue.path === "" ? "/" : issue.path}: ${issue.message}`,
        )
        .join("; ");
}

/**
 * Validates a fully-assembled routine object: structural schema check plus
 * cross-field constraints the schema cannot express. Throws a
 * RoutineValidationError with a descriptive message prefixed by `label`.
 */
function assertValidRoutine(routine: Routine, label: string): void {
    if (routine.name.trim().length === 0) {
        throw new RoutineValidationError(`${label}: name must not be empty`);
    }

    if (!routineValidator.check(routine)) {
        throw new RoutineValidationError(
            `${label}: ${describeValidationProblems(routine)}`,
        );
    }

    const { min_duration, max_duration } = routine.duration;
    if (
        min_duration !== undefined &&
        max_duration !== undefined &&
        min_duration > max_duration
    ) {
        throw new RoutineValidationError(
            `${label}: duration.min_duration (${min_duration}) must be <= duration.max_duration (${max_duration})`,
        );
    }

    for (const window of routine.availability_windows) {
        const start = window.start_hour * 60 + window.start_minute;
        const end = window.end_hour * 60 + window.end_minute;
        if (start >= end) {
            throw new RoutineValidationError(
                `${label}: availability window must start before it ends (got ${window.start_hour}:${window.start_minute} - ${window.end_hour}:${window.end_minute})`,
            );
        }
    }
}

function rowToRoutine(row: typeof routines.$inferSelect): Routine {
    let routine: Routine;
    try {
        routine = {
            id: row.id,
            name: row.name,
            description: row.description ?? undefined,
            duration: JSON.parse(row.duration) as Duration,
            priority: row.priority as PriorityLevel,
            flexibility: row.flexibility,
            energy_level_required: row.energyLevelRequired as EnergyLevel,
            category: row.category,
            frequency: row.frequency as Frequency,
            time_preferences: JSON.parse(row.timePreferences) as TimeOfDay[],
            availability_windows: JSON.parse(
                row.availabilityWindows,
            ) as TimeWindow[],
            dependencies: JSON.parse(row.dependencies) as Dependency[],
            minimum_gap_minutes: row.minimumGapMinutes,
            buffer_time_minutes: row.bufferTimeMinutes,
            conflict_resolution: row.conflictResolution as ConflictResolution,
            can_be_grouped: row.canBeGrouped,
            preferred_batch_size: row.preferredBatchSize ?? undefined,
            enabled: row.enabled,
            tags: JSON.parse(row.tags) as string[],
        };
    } catch (error) {
        throw new Error(
            `Routine ${row.id} has corrupt JSON data: ${
                error instanceof Error ? error.message : String(error)
            }`,
        );
    }

    if (!routineValidator.check(routine)) {
        throw new Error(
            `Routine ${row.id} failed validation: ${describeValidationProblems(routine)}`,
        );
    }

    return routine;
}

export class RoutineService {
    async createRoutine(
        userId: string,
        input: CreateRoutineInput,
    ): Promise<Routine> {
        const id = randomUUID();
        const now = new Date();

        const candidate: Routine = {
            id,
            name: input.name,
            description: input.description,
            duration: input.duration,
            priority: input.priority,
            flexibility: input.flexibility,
            energy_level_required: input.energy_level_required,
            category: input.category,
            frequency: input.frequency,
            time_preferences: input.time_preferences,
            availability_windows: input.availability_windows,
            dependencies: input.dependencies,
            minimum_gap_minutes: input.minimum_gap_minutes,
            buffer_time_minutes: input.buffer_time_minutes,
            conflict_resolution: input.conflict_resolution,
            can_be_grouped: input.can_be_grouped,
            preferred_batch_size: input.preferred_batch_size,
            enabled: input.enabled,
            tags: input.tags,
        };
        assertValidRoutine(candidate, "Invalid routine input");

        await db.insert(routines).values({
            id,
            userId,
            name: input.name,
            description: input.description ?? null,
            duration: JSON.stringify(input.duration),
            priority: input.priority,
            flexibility: input.flexibility,
            energyLevelRequired: input.energy_level_required,
            category: input.category,
            frequency: input.frequency,
            timePreferences: JSON.stringify(input.time_preferences),
            availabilityWindows: JSON.stringify(input.availability_windows),
            dependencies: JSON.stringify(input.dependencies),
            minimumGapMinutes: input.minimum_gap_minutes,
            bufferTimeMinutes: input.buffer_time_minutes,
            conflictResolution: input.conflict_resolution,
            canBeGrouped: input.can_be_grouped,
            preferredBatchSize: input.preferred_batch_size ?? null,
            enabled: input.enabled,
            tags: JSON.stringify(input.tags),
            createdAt: now,
            updatedAt: now,
        });

        const row = await db
            .select()
            .from(routines)
            .where(and(eq(routines.id, id), eq(routines.userId, userId)))
            .get();

        if (!row) throw new Error("Failed to create routine");
        return rowToRoutine(row);
    }

    async getRoutine(id: string, userId: string): Promise<Routine | null> {
        const row = await db
            .select()
            .from(routines)
            .where(and(eq(routines.id, id), eq(routines.userId, userId)))
            .get();

        return row ? rowToRoutine(row) : null;
    }

    async listRoutines(userId: string): Promise<Routine[]> {
        const rows = await db
            .select()
            .from(routines)
            .where(eq(routines.userId, userId))
            .all();

        return rows.map(rowToRoutine);
    }

    async updateRoutine(
        id: string,
        userId: string,
        input: UpdateRoutineInput,
    ): Promise<Routine | null> {
        const existing = await this.getRoutine(id, userId);
        if (!existing) return null;

        const merged: Routine = { ...existing };
        for (const [key, value] of Object.entries(input)) {
            if (value === undefined) continue;
            // Explicit null clears the (optional) field.
            (merged as unknown as Record<string, unknown>)[key] =
                value === null ? undefined : value;
        }
        assertValidRoutine(merged, `Invalid update for routine ${id}`);

        const updates: Partial<typeof routines.$inferInsert> = {
            updatedAt: new Date(),
        };

        if (input.name !== undefined) updates.name = input.name;
        if (input.description !== undefined)
            updates.description = input.description ?? null;
        if (input.duration !== undefined)
            updates.duration = JSON.stringify(input.duration);
        if (input.priority !== undefined) updates.priority = input.priority;
        if (input.flexibility !== undefined)
            updates.flexibility = input.flexibility;
        if (input.energy_level_required !== undefined)
            updates.energyLevelRequired = input.energy_level_required;
        if (input.category !== undefined) updates.category = input.category;
        if (input.frequency !== undefined) updates.frequency = input.frequency;
        if (input.time_preferences !== undefined)
            updates.timePreferences = JSON.stringify(input.time_preferences);
        if (input.availability_windows !== undefined)
            updates.availabilityWindows = JSON.stringify(
                input.availability_windows,
            );
        if (input.dependencies !== undefined)
            updates.dependencies = JSON.stringify(input.dependencies);
        if (input.minimum_gap_minutes !== undefined)
            updates.minimumGapMinutes = input.minimum_gap_minutes;
        if (input.buffer_time_minutes !== undefined)
            updates.bufferTimeMinutes = input.buffer_time_minutes;
        if (input.conflict_resolution !== undefined)
            updates.conflictResolution = input.conflict_resolution;
        if (input.can_be_grouped !== undefined)
            updates.canBeGrouped = input.can_be_grouped;
        if (input.preferred_batch_size !== undefined)
            updates.preferredBatchSize = input.preferred_batch_size ?? null;
        if (input.enabled !== undefined) updates.enabled = input.enabled;
        if (input.tags !== undefined) updates.tags = JSON.stringify(input.tags);

        await db
            .update(routines)
            .set(updates)
            .where(and(eq(routines.id, id), eq(routines.userId, userId)));

        return this.getRoutine(id, userId);
    }

    async deleteRoutine(id: string, userId: string): Promise<boolean> {
        const deleted = await db
            .delete(routines)
            .where(and(eq(routines.id, id), eq(routines.userId, userId)))
            .returning({ id: routines.id });

        return deleted.length > 0;
    }
}

export const routineService = new RoutineService();
