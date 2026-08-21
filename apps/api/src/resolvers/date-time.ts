import { GraphQLError, GraphQLScalarType } from "graphql";
import { Kind } from "graphql/language";

function parseDate(value: string, context: string): Date {
    const date = new Date(value);
    if (Number.isNaN(date.getTime())) {
        throw new GraphQLError(
            `${context}: "${value}" is not a valid date-time`,
        );
    }
    return date;
}

export const DateTimeScalar = new GraphQLScalarType({
    name: "DateTime",
    description: "An ISO-8601 encoded date-time string",
    serialize: (value: unknown) => {
        if (value instanceof Date) {
            if (Number.isNaN(value.getTime())) {
                throw new GraphQLError(
                    "DateTime cannot serialize an invalid Date",
                );
            }
            return value.toISOString();
        }
        if (typeof value === "string") {
            // Validate the string parses to a real date, but return it as-is
            parseDate(value, "DateTime cannot serialize value");
            return value;
        }
        throw new GraphQLError("Value must be a Date or ISO string");
    },
    parseValue: (value: unknown) => {
        if (typeof value === "string") {
            return parseDate(value, "DateTime cannot parse value");
        }
        throw new GraphQLError("Value must be an ISO string");
    },
    parseLiteral: (ast) => {
        if (ast.kind === Kind.STRING) {
            return parseDate(ast.value, "DateTime cannot parse literal");
        }
        throw new GraphQLError("Value must be an ISO string literal");
    },
});
