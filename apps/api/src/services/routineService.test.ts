import { beforeEach, describe, expect, it, vi } from "vitest";
import type { CreateRoutineInput } from "./routineService";

vi.mock("../db", async () => {
    const { default: Database } = await import("better-sqlite3");
    const { drizzle } = await import("drizzle-orm/better-sqlite3");
    const schema = await import("../db/schema");

    const sqlite = new Database(":memory:");
    sqlite.exec(`
        CREATE TABLE IF NOT EXISTS users (
            id TEXT PRIMARY KEY,
            email TEXT NOT NULL,
            name TEXT,
            picture TEXT
        );
        CREATE TABLE IF NOT EXISTS oauth_tokens (
            id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL REFERENCES users(id),
            access_token TEXT NOT NULL,
            refresh_token TEXT NOT NULL,
            expires_at INTEGER NOT NULL,
            scope TEXT
        );
        CREATE TABLE IF NOT EXISTS routines (
            id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL REFERENCES users(id),
            name TEXT NOT NULL,
            description TEXT,
            duration TEXT NOT NULL,
            priority TEXT NOT NULL,
            flexibility INTEGER NOT NULL,
            energy_level_required TEXT NOT NULL,
            category TEXT NOT NULL,
            frequency TEXT NOT NULL,
            time_preferences TEXT NOT NULL,
            availability_windows TEXT NOT NULL,
            dependencies TEXT NOT NULL,
            minimum_gap_minutes INTEGER NOT NULL DEFAULT 0,
            buffer_time_minutes INTEGER NOT NULL DEFAULT 5,
            conflict_resolution TEXT NOT NULL,
            can_be_grouped INTEGER NOT NULL DEFAULT 1,
            preferred_batch_size INTEGER,
            enabled INTEGER NOT NULL DEFAULT 1,
            tags TEXT NOT NULL DEFAULT '[]',
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );
        INSERT OR IGNORE INTO users (id, email) VALUES ('user1', 'user1@example.com');
    `);

    const db = drizzle(sqlite, { schema });
    (globalThis as Record<string, unknown>).__testSqliteRoutine = sqlite;
    return { db };
});

import { RoutineService } from "./routineService";

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
});
