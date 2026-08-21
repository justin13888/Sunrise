import { GraphQLError, GraphQLScalarType } from "graphql";
import { Kind } from "graphql/language";

function validateUrl(value: unknown, context: string): string {
    if (value instanceof URL) {
        return value.toString();
    }
    if (typeof value !== "string") {
        throw new GraphQLError(`${context}: value must be a URL string`);
    }
    try {
        new URL(value);
    } catch {
        throw new GraphQLError(`${context}: "${value}" is not a valid URL`);
    }
    return value;
}

export const URLScalar = new GraphQLScalarType({
    name: "URL",
    description:
        "A valid absolute URL string (validated with the WHATWG URL parser)",
    serialize: (value: unknown) => validateUrl(value, "URL cannot serialize"),
    parseValue: (value: unknown) =>
        validateUrl(value, "URL cannot parse value"),
    parseLiteral: (ast) => {
        if (ast.kind === Kind.STRING) {
            return validateUrl(ast.value, "URL cannot parse literal");
        }
        throw new GraphQLError("Value must be a string literal");
    },
});
