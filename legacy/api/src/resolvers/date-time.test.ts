/** biome-ignore-all lint/suspicious/noExplicitAny: necessary for testing */
import { Kind } from "graphql/language";
import { describe, expect, it } from "vitest";
import { DateTimeScalar } from "./date-time";

describe("DateTimeScalar", () => {
    describe("serialize", () => {
        it("should serialize Date object to ISO string", () => {
            const date = new Date("2025-11-10T12:00:00.000Z");
            const result = DateTimeScalar.serialize(date);
            expect(result).toBe("2025-11-10T12:00:00.000Z");
        });

        it("should serialize ISO string as is", () => {
            const isoString = "2025-11-10T12:00:00.000Z";
            const result = DateTimeScalar.serialize(isoString);
            expect(result).toBe(isoString);
        });

        it("should throw error for invalid types", () => {
            expect(() => DateTimeScalar.serialize(123)).toThrow(
                "Value must be a Date or ISO string",
            );
            expect(() => DateTimeScalar.serialize(null)).toThrow(
                "Value must be a Date or ISO string",
            );
            expect(() => DateTimeScalar.serialize(undefined)).toThrow(
                "Value must be a Date or ISO string",
            );
            expect(() => DateTimeScalar.serialize({})).toThrow(
                "Value must be a Date or ISO string",
            );
        });

        it("should handle different Date formats", () => {
            const dates = [
                new Date("2025-01-01"),
                new Date("2025-12-31T23:59:59Z"),
                new Date(0), // Unix epoch
                new Date("1999-12-31T23:59:59.999Z"),
            ];

            dates.forEach((date) => {
                const result = DateTimeScalar.serialize(date);
                expect(result).toBe(date.toISOString());
            });
        });
    });

    describe("parseValue", () => {
        it("should parse ISO string to Date", () => {
            const isoString = "2025-11-10T12:00:00.000Z";
            const result = DateTimeScalar.parseValue(isoString);
            expect(result).toBeInstanceOf(Date);
            expect(result.toISOString()).toBe(isoString);
        });

        it("should parse various date string formats", () => {
            const testCases = [
                "2025-11-10",
                "2025-11-10T12:00:00Z",
                "2025-11-10T12:00:00.000Z",
                "2025-11-10T12:00:00+00:00",
            ];

            testCases.forEach((dateString) => {
                const result = DateTimeScalar.parseValue(dateString);
                expect(result).toBeInstanceOf(Date);
                expect(result.toString()).not.toBe("Invalid Date");
            });
        });

        it("should throw error for non-string values", () => {
            expect(() => DateTimeScalar.parseValue(123)).toThrow(
                "Value must be an ISO string",
            );
            expect(() => DateTimeScalar.parseValue(null)).toThrow(
                "Value must be an ISO string",
            );
            expect(() => DateTimeScalar.parseValue(undefined)).toThrow(
                "Value must be an ISO string",
            );
            expect(() => DateTimeScalar.parseValue({})).toThrow(
                "Value must be an ISO string",
            );
        });

        it("should create Date even for invalid date strings", () => {
            // Note: new Date('invalid') creates Invalid Date but doesn't throw
            const result = DateTimeScalar.parseValue("invalid");
            expect(result).toBeInstanceOf(Date);
        });
    });

    describe("parseLiteral", () => {
        it("should parse string literal to Date", () => {
            const ast = {
                kind: Kind.STRING,
                value: "2025-11-10T12:00:00.000Z",
            } as const;
            const result = DateTimeScalar.parseLiteral(ast as any, {});
            expect(result).toBeInstanceOf(Date);
            expect(result.toISOString()).toBe("2025-11-10T12:00:00.000Z");
        });

        it("should parse various date string literals", () => {
            const testCases = [
                "2025-11-10",
                "2025-11-10T12:00:00Z",
                "2025-11-10T12:00:00.000Z",
            ];

            testCases.forEach((dateString) => {
                const ast = {
                    kind: Kind.STRING,
                    value: dateString,
                } as const;
                const result = DateTimeScalar.parseLiteral(ast as any, {});
                expect(result).toBeInstanceOf(Date);
            });
        });

        it("should throw error for non-string literals", () => {
            const intAst = {
                kind: Kind.INT,
                value: "123",
            } as const;
            expect(() =>
                DateTimeScalar.parseLiteral(intAst as any, {}),
            ).toThrow("Value must be an ISO string literal");

            const floatAst = {
                kind: Kind.FLOAT,
                value: "123.45",
            } as const;
            expect(() =>
                DateTimeScalar.parseLiteral(floatAst as any, {}),
            ).toThrow("Value must be an ISO string literal");

            const boolAst = {
                kind: Kind.BOOLEAN,
                value: true,
            } as const;
            expect(() =>
                DateTimeScalar.parseLiteral(boolAst as any, {}),
            ).toThrow("Value must be an ISO string literal");
        });

        it("should handle empty string literal", () => {
            const ast = {
                kind: Kind.STRING,
                value: "",
            } as const;
            const result = DateTimeScalar.parseLiteral(ast as any, {});
            expect(result).toBeInstanceOf(Date);
        });
    });

    describe("roundtrip", () => {
        it("should preserve date through parseValue -> serialize", () => {
            const isoString = "2025-11-10T12:00:00.000Z";
            const parsed = DateTimeScalar.parseValue(isoString);
            const serialized = DateTimeScalar.serialize(parsed);
            expect(serialized).toBe(isoString);
        });

        it("should preserve date through parseLiteral -> serialize", () => {
            const isoString = "2025-11-10T12:00:00.000Z";
            const ast = {
                kind: Kind.STRING,
                value: isoString,
            } as const;
            const parsed = DateTimeScalar.parseLiteral(ast as any, {});
            const serialized = DateTimeScalar.serialize(parsed);
            expect(serialized).toBe(isoString);
        });
    });

    describe("edge cases", () => {
        it("should handle Unix epoch", () => {
            const epoch = new Date(0);
            const serialized = DateTimeScalar.serialize(epoch);
            expect(serialized).toBe("1970-01-01T00:00:00.000Z");
        });

        it("should handle far future dates", () => {
            const futureDate = new Date("2999-12-31T23:59:59.999Z");
            const serialized = DateTimeScalar.serialize(futureDate);
            expect(serialized).toBe("2999-12-31T23:59:59.999Z");
        });

        it("should handle milliseconds precision", () => {
            const dateWithMs = new Date("2025-11-10T12:00:00.123Z");
            const serialized = DateTimeScalar.serialize(dateWithMs);
            expect(serialized).toBe("2025-11-10T12:00:00.123Z");
        });
    });
});
