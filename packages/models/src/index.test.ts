import { Type } from "@sinclair/typebox";
import { describe, expect, it } from "vitest";
import {
    assertRoutine,
    createValidator,
    DEFAULT_ROUTINE_CATEGORIES,
    dependencyValidator,
    durationValidator,
    PRECONFIGURED_ROUTINES,
    ROUTINE_TEMPLATES,
    type Routine,
    routineCategoryDefinitionValidator,
    routineTemplateValidator,
    routineValidator,
    timeWindowValidator,
    validateRoutine,
} from "./index";

const VALID_ROUTINE: Routine = {
    id: "morning_exercise",
    name: "Morning Exercise",
    description: "Energizing physical activity to start the day",
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

describe("createValidator", () => {
    const validator = createValidator(
        Type.Object({ count: Type.Number({ minimum: 1 }) }),
    );

    it("check returns true for valid values", () => {
        expect(validator.check({ count: 3 })).toBe(true);
    });

    it("check returns false for invalid values", () => {
        expect(validator.check({ count: 0 })).toBe(false);
        expect(validator.check(null)).toBe(false);
        expect(validator.check("nope")).toBe(false);
    });

    it("assert returns the value when valid", () => {
        expect(validator.assert({ count: 3 })).toEqual({ count: 3 });
    });

    it("assert throws a readable error naming the label and path", () => {
        expect(() => validator.assert({ count: 0 }, "widget")).toThrow(
            /Invalid widget: .*\/count/,
        );
    });

    it("errors lists every problem and is empty for valid values", () => {
        expect(validator.errors({ count: 3 })).toEqual([]);
        const issues = validator.errors({ count: 0 });
        expect(issues.length).toBeGreaterThan(0);
        expect(issues[0].path).toBe("/count");
        expect(issues[0].message).toBeTruthy();
    });
});

describe("routineValidator", () => {
    it("accepts a valid routine", () => {
        expect(routineValidator.check(VALID_ROUTINE)).toBe(true);
        expect(validateRoutine(VALID_ROUTINE)).toBe(true);
        expect(assertRoutine(VALID_ROUTINE)).toEqual(VALID_ROUTINE);
    });

    it("rejects a bad enum value", () => {
        const bad = { ...VALID_ROUTINE, priority: "urgent" };
        expect(validateRoutine(bad)).toBe(false);
        expect(() => assertRoutine(bad)).toThrow(/Invalid routine: .*priority/);
    });

    it("rejects negative duration minutes", () => {
        const bad = {
            ...VALID_ROUTINE,
            duration: { minutes: -5, flexible: false },
        };
        expect(validateRoutine(bad)).toBe(false);
        expect(() => assertRoutine(bad)).toThrow(/duration\/minutes/);
    });

    it("rejects a missing required field", () => {
        const { name: _name, ...missingName } = VALID_ROUTINE;
        expect(validateRoutine(missingName)).toBe(false);
        expect(() => assertRoutine(missingName)).toThrow(/name/);
    });

    it("rejects an empty name", () => {
        expect(validateRoutine({ ...VALID_ROUTINE, name: "" })).toBe(false);
    });

    it("rejects a non-UUID category", () => {
        expect(
            validateRoutine({ ...VALID_ROUTINE, category: "not-a-uuid" }),
        ).toBe(false);
    });

    it("accepts flexibility boundaries 0 and 100, rejects outside", () => {
        expect(validateRoutine({ ...VALID_ROUTINE, flexibility: 0 })).toBe(
            true,
        );
        expect(validateRoutine({ ...VALID_ROUTINE, flexibility: 100 })).toBe(
            true,
        );
        expect(validateRoutine({ ...VALID_ROUTINE, flexibility: -1 })).toBe(
            false,
        );
        expect(validateRoutine({ ...VALID_ROUTINE, flexibility: 101 })).toBe(
            false,
        );
    });

    it("rejects negative gap and buffer minutes", () => {
        expect(
            validateRoutine({ ...VALID_ROUTINE, minimum_gap_minutes: -1 }),
        ).toBe(false);
        expect(
            validateRoutine({ ...VALID_ROUTINE, buffer_time_minutes: -1 }),
        ).toBe(false);
    });

    it("rejects preferred_batch_size below 1", () => {
        expect(
            validateRoutine({ ...VALID_ROUTINE, preferred_batch_size: 0 }),
        ).toBe(false);
        expect(
            validateRoutine({ ...VALID_ROUTINE, preferred_batch_size: 1 }),
        ).toBe(true);
    });
});

describe("durationValidator", () => {
    it("accepts a valid duration", () => {
        expect(durationValidator.check({ minutes: 30, flexible: false })).toBe(
            true,
        );
    });

    it("rejects minutes below the minimum", () => {
        expect(durationValidator.check({ minutes: 0, flexible: false })).toBe(
            false,
        );
        expect(durationValidator.check({ minutes: -10, flexible: true })).toBe(
            false,
        );
    });

    it("rejects minutes above the maximum", () => {
        expect(durationValidator.check({ minutes: 481, flexible: false })).toBe(
            false,
        );
    });
});

describe("timeWindowValidator", () => {
    it("accepts boundary values hour 23 and minute 59", () => {
        expect(
            timeWindowValidator.check({
                start_hour: 0,
                start_minute: 0,
                end_hour: 23,
                end_minute: 59,
            }),
        ).toBe(true);
    });

    it("rejects hour 24 and minute 60", () => {
        expect(
            timeWindowValidator.check({
                start_hour: 24,
                start_minute: 0,
                end_hour: 23,
                end_minute: 0,
            }),
        ).toBe(false);
        expect(
            timeWindowValidator.check({
                start_hour: 0,
                start_minute: 60,
                end_hour: 23,
                end_minute: 0,
            }),
        ).toBe(false);
    });

    it("rejects negative hours", () => {
        expect(
            timeWindowValidator.check({
                start_hour: -1,
                start_minute: 0,
                end_hour: 12,
                end_minute: 0,
            }),
        ).toBe(false);
    });
});

describe("dependencyValidator", () => {
    it("accepts a valid dependency", () => {
        expect(
            dependencyValidator.check({
                routine_id: "wake_up",
                relationship: "after",
                buffer_minutes: 5,
            }),
        ).toBe(true);
    });

    it("rejects an unknown relationship", () => {
        expect(
            dependencyValidator.check({
                routine_id: "wake_up",
                relationship: "during",
            }),
        ).toBe(false);
    });

    it("rejects negative buffer minutes", () => {
        expect(
            dependencyValidator.check({
                routine_id: "wake_up",
                relationship: "before",
                buffer_minutes: -5,
            }),
        ).toBe(false);
    });
});

describe("built-in constants conform to their own schemas", () => {
    it("every DEFAULT_ROUTINE_CATEGORIES entry is a valid category definition", () => {
        for (const category of DEFAULT_ROUTINE_CATEGORIES) {
            expect(routineCategoryDefinitionValidator.errors(category)).toEqual(
                [],
            );
        }
    });

    it("every PRECONFIGURED_ROUTINES entry is a valid routine", () => {
        for (const routine of PRECONFIGURED_ROUTINES) {
            expect(routineValidator.errors(routine)).toEqual([]);
        }
    });

    it("every PRECONFIGURED_ROUTINES category references a default category", () => {
        const ids = new Set(DEFAULT_ROUTINE_CATEGORIES.map((c) => c.id));
        for (const routine of PRECONFIGURED_ROUTINES) {
            expect(ids.has(routine.category)).toBe(true);
        }
    });

    it("every ROUTINE_TEMPLATES entry is a valid template", () => {
        for (const template of ROUTINE_TEMPLATES) {
            expect(routineTemplateValidator.errors(template)).toEqual([]);
        }
    });
});
