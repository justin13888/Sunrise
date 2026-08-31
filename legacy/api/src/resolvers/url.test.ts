/** biome-ignore-all lint/suspicious/noExplicitAny: necessary for testing */
import { Kind } from "graphql/language";
import { describe, expect, it } from "vitest";
import { URLScalar } from "./url";

describe("URLScalar", () => {
    describe("serialize", () => {
        it("should serialize string to string", () => {
            const url = "https://example.com";
            const result = URLScalar.serialize(url);
            expect(result).toBe(url);
        });

        it("should serialize URL object to string", () => {
            const url = new URL("https://example.com/path");
            const result = URLScalar.serialize(url);
            expect(result).toBe("https://example.com/path");
        });

        it("should serialize number to string", () => {
            const result = URLScalar.serialize(123);
            expect(result).toBe("123");
        });

        it('should serialize null to string "null"', () => {
            const result = URLScalar.serialize(null);
            expect(result).toBe("null");
        });

        it('should serialize undefined to string "undefined"', () => {
            const result = URLScalar.serialize(undefined);
            expect(result).toBe("undefined");
        });

        it("should serialize object to string", () => {
            const result = URLScalar.serialize({ url: "test" });
            expect(result).toBe("[object Object]");
        });

        it("should handle various URL formats", () => {
            const urls = [
                "http://example.com",
                "https://example.com",
                "https://example.com/path/to/resource",
                "https://example.com/path?query=value",
                "https://example.com/path?query=value#hash",
                "https://user:pass@example.com:8080/path",
                "ftp://example.com",
                "mailto:test@example.com",
            ];

            urls.forEach((url) => {
                const result = URLScalar.serialize(url);
                expect(result).toBe(url);
            });
        });
    });

    describe("parseValue", () => {
        it("should parse string to string", () => {
            const url = "https://example.com";
            const result = URLScalar.parseValue(url);
            expect(result).toBe(url);
        });

        it("should parse number to string", () => {
            const result = URLScalar.parseValue(123);
            expect(result).toBe("123");
        });

        it('should parse null to string "null"', () => {
            const result = URLScalar.parseValue(null);
            expect(result).toBe("null");
        });

        it('should parse undefined to string "undefined"', () => {
            const result = URLScalar.parseValue(undefined);
            expect(result).toBe("undefined");
        });

        it("should parse object to string", () => {
            const result = URLScalar.parseValue({ url: "test" });
            expect(result).toBe("[object Object]");
        });

        it("should handle various URL formats", () => {
            const urls = [
                "http://example.com",
                "https://example.com/path",
                "https://example.com/path?query=value",
                "https://example.com/path#hash",
                "ftp://example.com",
                "mailto:test@example.com",
            ];

            urls.forEach((url) => {
                const result = URLScalar.parseValue(url);
                expect(result).toBe(url);
            });
        });

        it("should handle empty string", () => {
            const result = URLScalar.parseValue("");
            expect(result).toBe("");
        });

        it("should handle URL with special characters", () => {
            const url = "https://example.com/path with spaces";
            const result = URLScalar.parseValue(url);
            expect(result).toBe(url);
        });
    });

    describe("parseLiteral", () => {
        it("should parse string literal to string", () => {
            const ast = {
                kind: Kind.STRING,
                value: "https://example.com",
            } as const;
            const result = URLScalar.parseLiteral(ast as any, {});
            expect(result).toBe("https://example.com");
        });

        it("should parse various URL string literals", () => {
            const urls = [
                "http://example.com",
                "https://example.com/path",
                "https://example.com/path?query=value",
                "https://example.com/path#hash",
            ];

            urls.forEach((url) => {
                const ast = {
                    kind: Kind.STRING,
                    value: url,
                } as const;
                const result = URLScalar.parseLiteral(ast as any, {});
                expect(result).toBe(url);
            });
        });

        it("should throw error for non-string literals", () => {
            const intAst = {
                kind: Kind.INT,
                value: "123",
            } as const;
            expect(() => URLScalar.parseLiteral(intAst as any, {})).toThrow(
                "Value must be a string literal",
            );

            const floatAst = {
                kind: Kind.FLOAT,
                value: "123.45",
            } as const;
            expect(() => URLScalar.parseLiteral(floatAst as any, {})).toThrow(
                "Value must be a string literal",
            );

            const boolAst = {
                kind: Kind.BOOLEAN,
                value: true,
            } as const;
            expect(() => URLScalar.parseLiteral(boolAst as any, {})).toThrow(
                "Value must be a string literal",
            );
        });

        it("should handle empty string literal", () => {
            const ast = {
                kind: Kind.STRING,
                value: "",
            } as const;
            const result = URLScalar.parseLiteral(ast as any, {});
            expect(result).toBe("");
        });

        it("should handle URL with special characters in literal", () => {
            const ast = {
                kind: Kind.STRING,
                value: "https://example.com/path?query=hello world",
            } as const;
            const result = URLScalar.parseLiteral(ast as any, {});
            expect(result).toBe("https://example.com/path?query=hello world");
        });
    });

    describe("roundtrip", () => {
        it("should preserve URL through parseValue -> serialize", () => {
            const url = "https://example.com/path";
            const parsed = URLScalar.parseValue(url);
            const serialized = URLScalar.serialize(parsed);
            expect(serialized).toBe(url);
        });

        it("should preserve URL through parseLiteral -> serialize", () => {
            const url = "https://example.com/path";
            const ast = {
                kind: Kind.STRING,
                value: url,
            } as const;
            const parsed = URLScalar.parseLiteral(ast as any, {});
            const serialized = URLScalar.serialize(parsed);
            expect(serialized).toBe(url);
        });
    });

    describe("edge cases", () => {
        it("should handle very long URLs", () => {
            const longPath = "a".repeat(1000);
            const url = `https://example.com/${longPath}`;
            const result = URLScalar.serialize(url);
            expect(result).toBe(url);
        });

        it("should handle URLs with international characters", () => {
            const url = "https://example.com/путь";
            const result = URLScalar.parseValue(url);
            expect(result).toBe(url);
        });

        it("should handle relative URLs", () => {
            const url = "/relative/path";
            const result = URLScalar.parseValue(url);
            expect(result).toBe(url);
        });

        it("should handle protocol-relative URLs", () => {
            const url = "//example.com/path";
            const result = URLScalar.parseValue(url);
            expect(result).toBe(url);
        });

        it("should handle data URLs", () => {
            const url = "data:text/plain;base64,SGVsbG8=";
            const result = URLScalar.parseValue(url);
            expect(result).toBe(url);
        });
    });
});
