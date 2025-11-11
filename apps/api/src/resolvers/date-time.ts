import { GraphQLScalarType } from "graphql";
import { Kind } from "graphql/language";

export const DateTimeScalar = new GraphQLScalarType({
    name: "DateTime",
    serialize: (value: unknown) => {
        if (value instanceof Date) return value.toISOString();
        if (typeof value === "string") return value;
        throw new Error("Value must be a Date or ISO string");
    },
    parseValue: (value: unknown) => {
        if (typeof value === "string") return new Date(value);
        throw new Error("Value must be an ISO string");
    },
    parseLiteral: (ast) => {
        if (ast.kind === Kind.STRING) return new Date(ast.value);
        throw new Error("Value must be an ISO string literal");
    },
});
