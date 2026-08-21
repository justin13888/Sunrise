/** biome-ignore-all lint/suspicious/noExplicitAny: necessary for testing */
import { Kind } from "graphql/language";
import { describe, expect, it } from "vitest";
import { URLScalar } from "./url";

describe("URLScalar", () => {
    describe("serialize", () => {
        it("should serialize a valid URL string as-is", () => {
            const url = "https://example.com";
            const result = URLScalar.serialize(url);
            expect(result).toBe(url);
        });

        it("should serialize URL object to string", () => {
            const url = new URL("https://example.com/path");
            const result = URLScalar.serialize(url);
            expect(result).toBe("https://example.com/path");
        });

        it("should throw for non-string values", () => {
            expect(() => URLScalar.serialize(123)).toThrow(
                "value must be a URL string",
            );
            expect(() => URLScalar.serialize(null)).toThrow(
                "value must be a URL string",
            );
            expect(() => URLScalar.serialize(undefined)).toThrow(
                "value must be a URL string",
            );
            expect(() => URLScalar.serialize({ url: "test" })).toThrow(
                "value must be a URL string",
            );
        });

        it("should throw for invalid URL strings", () => {
            expect(() => URLScalar.serialize("not a url")).toThrow(
                "is not a valid URL",
            );
            expect(() => URLScalar.serialize("")).toThrow("is not a valid URL");
        });

        it("should handle various valid URL formats", () => {
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
        it("should parse a valid URL string as-is", () => {
            const url = "https://example.com";
            const result = URLScalar.parseValue(url);
            expect(result).toBe(url);
        });

        it("should throw for non-string values", () => {
            expect(() => URLScalar.parseValue(123)).toThrow(
                "value must be a URL string",
            );
            expect(() => URLScalar.parseValue(null)).toThrow(
                "value must be a URL string",
            );
            expect(() => URLScalar.parseValue(undefined)).toThrow(
                "value must be a URL string",
            );
            expect(() => URLScalar.parseValue({ url: "test" })).toThrow(
                "value must be a URL string",
            );
        });

        it("should throw for invalid URL strings", () => {
            expect(() => URLScalar.parseValue("not a url")).toThrow(
                "is not a valid URL",
            );
            expect(() => URLScalar.parseValue("")).toThrow(
                "is not a valid URL",
            );
        });

        it("should handle various valid URL formats", () => {
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

        it("should accept URLs whose path contains spaces", () => {
            // The WHATWG parser encodes spaces, so this remains parseable
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

        it("should throw for invalid URL string literals", () => {
            const emptyAst = {
                kind: Kind.STRING,
                value: "",
            } as const;
            expect(() => URLScalar.parseLiteral(emptyAst as any, {})).toThrow(
                "is not a valid URL",
            );

            const invalidAst = {
                kind: Kind.STRING,
                value: "not a url",
            } as const;
            expect(() => URLScalar.parseLiteral(invalidAst as any, {})).toThrow(
                "is not a valid URL",
            );
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

        it("should reject relative URLs", () => {
            expect(() => URLScalar.parseValue("/relative/path")).toThrow(
                "is not a valid URL",
            );
        });

        it("should reject protocol-relative URLs", () => {
            expect(() => URLScalar.parseValue("//example.com/path")).toThrow(
                "is not a valid URL",
            );
        });

        it("should handle data URLs", () => {
            const url = "data:text/plain;base64,SGVsbG8=";
            const result = URLScalar.parseValue(url);
            expect(result).toBe(url);
        });
    });
});
