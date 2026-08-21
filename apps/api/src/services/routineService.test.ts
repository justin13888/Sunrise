import { beforeEach, describe, expect, it, vi } from "vitest";
import type { CreateRoutineInput } from "./routineService";

vi.mock("../db", async () => {
    const { createTestDb } = await import("../db/testUtils");

    const { sqlite, db } = createTestDb();
    sqlite.exec(
        "INSERT OR IGNORE INTO users (id, email) VALUES ('user1', 'user1@example.com');",
    );

    (globalThis as Record<string, unknown>).__testSqliteRoutine = sqlite;
    return { db };
});

import { RoutineService, RoutineValidationError } from "./routineService";

function getSqlite() {
    return (globalThis as Record<string, unknown>).__testSqliteRoutine as {
        exec: (sql: string) => void;
    };
}

const BASE_INPUT: CreateRoutineInput = {
    name: "Morning Exercise",
    description: "A good workout",
    duration: {
        minutes: 45,
        flexible: true,
        min_duration: 20,
        max_duration: 90,
    },
    priority: "high",
    flexibility: 4,
    energy_level_required: "medium",
    category: "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
    frequency: "daily",
    time_preferences: ["morning"],
    availability_windows: [
        { start_hour: 6, start_minute: 0, end_hour: 11, end_minute: 0 },
    ],
    dependencies: [],
    minimum_gap_minutes: 30,
    buffer_time_minutes: 10,
    conflict_resolution: "reschedule",
    can_be_grouped: false,
    enabled: true,
    tags: ["morning", "fitness"],
};

describe("RoutineService", () => {
    let service: RoutineService;

    beforeEach(() => {
        service = new RoutineService();
        getSqlite().exec("DELETE FROM routines;");
    });

    describe("createRoutine", () => {
        it("should create a routine and return it", async () => {
            const routine = await service.createRoutine("user1", BASE_INPUT);

            expect(routine.id).toBeTruthy();
            expect(routine.name).toBe("Morning Exercise");
            expect(routine.description).toBe("A good workout");
            expect(routine.priority).toBe("high");
            expect(routine.flexibility).toBe(4);
            expect(routine.energy_level_required).toBe("medium");
            expect(routine.frequency).toBe("daily");
            expect(routine.enabled).toBe(true);
            expect(routine.tags).toEqual(["morning", "fitness"]);
        });

        it("should serialize and deserialize duration JSON", async () => {
            const routine = await service.createRoutine("user1", BASE_INPUT);

            expect(routine.duration.minutes).toBe(45);
            expect(routine.duration.flexible).toBe(true);
            expect(routine.duration.min_duration).toBe(20);
            expect(routine.duration.max_duration).toBe(90);
        });

        it("should serialize and deserialize time_preferences", async () => {
            const routine = await service.createRoutine("user1", BASE_INPUT);
            expect(routine.time_preferences).toEqual(["morning"]);
        });

        it("should serialize and deserialize availability_windows", async () => {
            const routine = await service.createRoutine("user1", BASE_INPUT);
            expect(routine.availability_windows).toHaveLength(1);
            expect(routine.availability_windows[0]).toEqual({
                start_hour: 6,
                start_minute: 0,
                end_hour: 11,
                end_minute: 0,
            });
        });

        it("should handle optional description as undefined", async () => {
            const input = { ...BASE_INPUT, description: undefined };
            const routine = await service.createRoutine("user1", input);
            expect(routine.description).toBeUndefined();
        });

        it("should handle optional preferred_batch_size", async () => {
            const input = { ...BASE_INPUT, preferred_batch_size: 3 };
            const routine = await service.createRoutine("user1", input);
            expect(routine.preferred_batch_size).toBe(3);
        });
    });

    describe("getRoutine", () => {
        it("should return a routine by id and userId", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            const fetched = await service.getRoutine(created.id, "user1");

            expect(fetched).not.toBeNull();
            expect(fetched?.id).toBe(created.id);
            expect(fetched?.name).toBe("Morning Exercise");
        });

        it("should return null for non-existent id", async () => {
            const result = await service.getRoutine("nonexistent", "user1");
            expect(result).toBeNull();
        });

        it("should return null for wrong userId", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            const result = await service.getRoutine(created.id, "other-user");
            expect(result).toBeNull();
        });
    });

    describe("listRoutines", () => {
        it("should return empty array when no routines", async () => {
            const list = await service.listRoutines("user1");
            expect(list).toEqual([]);
        });

        it("should return all routines for a user", async () => {
            await service.createRoutine("user1", BASE_INPUT);
            await service.createRoutine("user1", {
                ...BASE_INPUT,
                name: "Evening Walk",
            });

            const list = await service.listRoutines("user1");
            expect(list).toHaveLength(2);
        });

        it("should not return routines from other users", async () => {
            await service.createRoutine("user1", BASE_INPUT);
            // user2 doesn't exist in DB so this would fail FK constraint - skip cross-user test
            const list = await service.listRoutines("nonexistent-user");
            expect(list).toHaveLength(0);
        });
    });

    describe("updateRoutine", () => {
        it("should update name and description", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            const updated = await service.updateRoutine(created.id, "user1", {
                name: "Updated Name",
                description: "Updated description",
            });

            expect(updated).not.toBeNull();
            expect(updated?.name).toBe("Updated Name");
            expect(updated?.description).toBe("Updated description");
        });

        it("should update priority and flexibility", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            const updated = await service.updateRoutine(created.id, "user1", {
                priority: "low",
                flexibility: 9,
            });

            expect(updated?.priority).toBe("low");
            expect(updated?.flexibility).toBe(9);
        });

        it("should update enabled status", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            const updated = await service.updateRoutine(created.id, "user1", {
                enabled: false,
            });

            expect(updated?.enabled).toBe(false);
        });

        it("should update tags", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            const updated = await service.updateRoutine(created.id, "user1", {
                tags: ["new-tag"],
            });

            expect(updated?.tags).toEqual(["new-tag"]);
        });

        it("should update duration JSON", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            const updated = await service.updateRoutine(created.id, "user1", {
                duration: { minutes: 60, flexible: false },
            });

            expect(updated?.duration.minutes).toBe(60);
            expect(updated?.duration.flexible).toBe(false);
        });

        it("should return null for non-existent routine", async () => {
            const result = await service.updateRoutine("nonexistent", "user1", {
                name: "New Name",
            });
            expect(result).toBeNull();
        });

        it("should return null when userId does not match", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            const result = await service.updateRoutine(
                created.id,
                "wrong-user",
                {
                    name: "Hacked",
                },
            );
            expect(result).toBeNull();
        });

        it("should preserve unchanged fields", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            const updated = await service.updateRoutine(created.id, "user1", {
                name: "Changed",
            });

            expect(updated?.frequency).toBe(created.frequency);
            expect(updated?.category).toBe(created.category);
            expect(updated?.tags).toEqual(created.tags);
        });

        it("clears description when explicitly set to null", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            expect(created.description).toBe("A good workout");

            const updated = await service.updateRoutine(created.id, "user1", {
                description: null,
            });
            expect(updated?.description).toBeUndefined();

            const fetched = await service.getRoutine(created.id, "user1");
            expect(fetched?.description).toBeUndefined();
        });

        it("clears preferred_batch_size when explicitly set to null", async () => {
            const created = await service.createRoutine("user1", {
                ...BASE_INPUT,
                preferred_batch_size: 3,
            });
            expect(created.preferred_batch_size).toBe(3);

            const updated = await service.updateRoutine(created.id, "user1", {
                preferred_batch_size: null,
            });
            expect(updated?.preferred_batch_size).toBeUndefined();
        });

        it("leaves description untouched when the patch omits it (undefined)", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            const updated = await service.updateRoutine(created.id, "user1", {
                name: "Renamed",
            });
            expect(updated?.description).toBe("A good workout");
        });
    });

    describe("deleteRoutine", () => {
        it("should delete a routine and return true", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            const deleted = await service.deleteRoutine(created.id, "user1");

            expect(deleted).toBe(true);
            const fetched = await service.getRoutine(created.id, "user1");
            expect(fetched).toBeNull();
        });

        it("should return false for non-existent routine", async () => {
            const result = await service.deleteRoutine("nonexistent", "user1");
            expect(result).toBe(false);
        });

        it("should return false when userId does not match", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            const result = await service.deleteRoutine(
                created.id,
                "wrong-user",
            );
            expect(result).toBe(false);

            // Original should still exist
            const fetched = await service.getRoutine(created.id, "user1");
            expect(fetched).not.toBeNull();
        });

        it("should not affect other routines when deleting one", async () => {
            const r1 = await service.createRoutine("user1", BASE_INPUT);
            const r2 = await service.createRoutine("user1", {
                ...BASE_INPUT,
                name: "Other",
            });

            await service.deleteRoutine(r1.id, "user1");

            const remaining = await service.listRoutines("user1");
            expect(remaining).toHaveLength(1);
            expect(remaining[0].id).toBe(r2.id);
        });
    });

    describe("input validation on create", () => {
        it("rejects an empty name after trimming", async () => {
            await expect(
                service.createRoutine("user1", { ...BASE_INPUT, name: "   " }),
            ).rejects.toThrow(/name must not be empty/);
        });

        it("throws RoutineValidationError for invalid input", async () => {
            await expect(
                service.createRoutine("user1", {
                    ...BASE_INPUT,
                    flexibility: 999,
                }),
            ).rejects.toBeInstanceOf(RoutineValidationError);
        });

        it("rejects a bad enum value", async () => {
            await expect(
                service.createRoutine("user1", {
                    ...BASE_INPUT,
                    priority: "urgent" as never,
                }),
            ).rejects.toThrow(/Invalid routine input.*priority/);
        });

        it("rejects non-positive duration minutes", async () => {
            await expect(
                service.createRoutine("user1", {
                    ...BASE_INPUT,
                    duration: { minutes: 0, flexible: false },
                }),
            ).rejects.toThrow(/duration\/minutes/);
            await expect(
                service.createRoutine("user1", {
                    ...BASE_INPUT,
                    duration: { minutes: -30, flexible: false },
                }),
            ).rejects.toThrow(/duration\/minutes/);
        });

        it("rejects min_duration greater than max_duration", async () => {
            await expect(
                service.createRoutine("user1", {
                    ...BASE_INPUT,
                    duration: {
                        minutes: 45,
                        flexible: true,
                        min_duration: 90,
                        max_duration: 30,
                    },
                }),
            ).rejects.toThrow(
                /min_duration \(90\) must be <= duration\.max_duration/,
            );
        });

        it("rejects out-of-range availability window hours and minutes", async () => {
            await expect(
                service.createRoutine("user1", {
                    ...BASE_INPUT,
                    availability_windows: [
                        {
                            start_hour: 24,
                            start_minute: 0,
                            end_hour: 25,
                            end_minute: 0,
                        },
                    ],
                }),
            ).rejects.toThrow(/availability_windows/);
            await expect(
                service.createRoutine("user1", {
                    ...BASE_INPUT,
                    availability_windows: [
                        {
                            start_hour: 8,
                            start_minute: 60,
                            end_hour: 9,
                            end_minute: 0,
                        },
                    ],
                }),
            ).rejects.toThrow(/availability_windows/);
        });

        it("rejects a window that does not start before it ends", async () => {
            await expect(
                service.createRoutine("user1", {
                    ...BASE_INPUT,
                    availability_windows: [
                        {
                            start_hour: 10,
                            start_minute: 0,
                            end_hour: 10,
                            end_minute: 0,
                        },
                    ],
                }),
            ).rejects.toThrow(/must start before it ends/);
            await expect(
                service.createRoutine("user1", {
                    ...BASE_INPUT,
                    availability_windows: [
                        {
                            start_hour: 18,
                            start_minute: 0,
                            end_hour: 9,
                            end_minute: 0,
                        },
                    ],
                }),
            ).rejects.toThrow(/must start before it ends/);
        });

        it("rejects negative gap and buffer minutes", async () => {
            await expect(
                service.createRoutine("user1", {
                    ...BASE_INPUT,
                    minimum_gap_minutes: -1,
                }),
            ).rejects.toThrow(/minimum_gap_minutes/);
            await expect(
                service.createRoutine("user1", {
                    ...BASE_INPUT,
                    buffer_time_minutes: -1,
                }),
            ).rejects.toThrow(/buffer_time_minutes/);
        });

        it("rejects preferred_batch_size below 1", async () => {
            await expect(
                service.createRoutine("user1", {
                    ...BASE_INPUT,
                    preferred_batch_size: 0,
                }),
            ).rejects.toThrow(/preferred_batch_size/);
        });

        it("rejects out-of-range flexibility", async () => {
            await expect(
                service.createRoutine("user1", {
                    ...BASE_INPUT,
                    flexibility: -1,
                }),
            ).rejects.toThrow(/flexibility/);
            await expect(
                service.createRoutine("user1", {
                    ...BASE_INPUT,
                    flexibility: 101,
                }),
            ).rejects.toThrow(/flexibility/);
        });

        it("accepts boundary values: hour 23, minute 59, flexibility 0 and 100", async () => {
            const atLowerFlexibility = await service.createRoutine("user1", {
                ...BASE_INPUT,
                flexibility: 0,
                availability_windows: [
                    {
                        start_hour: 23,
                        start_minute: 0,
                        end_hour: 23,
                        end_minute: 59,
                    },
                ],
            });
            expect(atLowerFlexibility.flexibility).toBe(0);
            expect(atLowerFlexibility.availability_windows[0]).toEqual({
                start_hour: 23,
                start_minute: 0,
                end_hour: 23,
                end_minute: 59,
            });

            const atUpperFlexibility = await service.createRoutine("user1", {
                ...BASE_INPUT,
                flexibility: 100,
            });
            expect(atUpperFlexibility.flexibility).toBe(100);
        });
    });

    describe("input validation on update", () => {
        it("rejects an invalid patch", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            await expect(
                service.updateRoutine(created.id, "user1", {
                    flexibility: 101,
                }),
            ).rejects.toThrow(
                new RegExp(`Invalid update for routine ${created.id}`),
            );
        });

        it("throws RoutineValidationError for an invalid patch", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            await expect(
                service.updateRoutine(created.id, "user1", {
                    flexibility: 101,
                }),
            ).rejects.toBeInstanceOf(RoutineValidationError);
        });

        it("rejects a whitespace-only name", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            await expect(
                service.updateRoutine(created.id, "user1", { name: "  " }),
            ).rejects.toThrow(/name must not be empty/);
        });

        it("rejects a patch that makes min_duration exceed max_duration", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            await expect(
                service.updateRoutine(created.id, "user1", {
                    duration: {
                        minutes: 45,
                        flexible: true,
                        min_duration: 120,
                        max_duration: 60,
                    },
                }),
            ).rejects.toThrow(
                /min_duration \(120\) must be <= duration\.max_duration/,
            );
        });

        it("leaves the routine unchanged when the patch is rejected", async () => {
            const created = await service.createRoutine("user1", BASE_INPUT);
            await expect(
                service.updateRoutine(created.id, "user1", {
                    flexibility: 101,
                    name: "Should Not Stick",
                }),
            ).rejects.toThrow();

            const fetched = await service.getRoutine(created.id, "user1");
            expect(fetched?.name).toBe(BASE_INPUT.name);
            expect(fetched?.flexibility).toBe(BASE_INPUT.flexibility);
        });
    });

    describe("validation on read", () => {
        function insertRawRoutine(overrides: {
            id: string;
            duration?: string;
            priority?: string;
        }) {
            const duration =
                overrides.duration ?? '{"minutes":45,"flexible":false}';
            const priority = overrides.priority ?? "high";
            getSqlite().exec(`
                INSERT INTO routines (
                    id, user_id, name, description, duration, priority,
                    flexibility, energy_level_required, category, frequency,
                    time_preferences, availability_windows, dependencies,
                    minimum_gap_minutes, buffer_time_minutes,
                    conflict_resolution, can_be_grouped, preferred_batch_size,
                    enabled, tags, created_at, updated_at
                ) VALUES (
                    '${overrides.id}', 'user1', 'Raw Routine', NULL,
                    '${duration}', '${priority}',
                    5, 'medium', '6ba7b810-9dad-11d1-80b4-00c04fd430c8',
                    'daily', '["morning"]',
                    '[{"start_hour":6,"start_minute":0,"end_hour":11,"end_minute":0}]',
                    '[]', 0, 5, 'reschedule', 0, NULL, 1, '[]', 0, 0
                );
            `);
        }

        it("getRoutine throws naming the id when a JSON column is corrupt", async () => {
            insertRawRoutine({ id: "corrupt-json", duration: "not json{{" });
            await expect(
                service.getRoutine("corrupt-json", "user1"),
            ).rejects.toThrow(/Routine corrupt-json has corrupt JSON data/);
        });

        it("corrupt-row errors are internal (not RoutineValidationError)", async () => {
            insertRawRoutine({ id: "corrupt-internal", duration: "}{" });
            const error = await service
                .getRoutine("corrupt-internal", "user1")
                .then(
                    () => null,
                    (e: unknown) => e,
                );
            expect(error).toBeInstanceOf(Error);
            expect(error).not.toBeInstanceOf(RoutineValidationError);
        });

        it("getRoutine throws naming the id when stored data fails validation", async () => {
            insertRawRoutine({
                id: "bad-priority",
                priority: "urgent",
            });
            await expect(
                service.getRoutine("bad-priority", "user1"),
            ).rejects.toThrow(/Routine bad-priority failed validation/);
        });

        it("listRoutines throws instead of silently returning corrupted rows", async () => {
            await service.createRoutine("user1", BASE_INPUT);
            insertRawRoutine({ id: "corrupt-in-list", duration: "}{" });
            await expect(service.listRoutines("user1")).rejects.toThrow(
                /Routine corrupt-in-list has corrupt JSON data/,
            );
        });

        it("valid raw rows read back cleanly", async () => {
            insertRawRoutine({ id: "clean-row" });
            const routine = await service.getRoutine("clean-row", "user1");
            expect(routine?.id).toBe("clean-row");
            expect(routine?.duration.minutes).toBe(45);
        });
    });
});
