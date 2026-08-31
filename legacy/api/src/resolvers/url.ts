import { GraphQLScalarType } from "graphql";
import { Kind } from "graphql/language";

export const URLScalar = new GraphQLScalarType({
    name: "URL",
    serialize: (value: unknown) => String(value),
    parseValue: (value: unknown) => String(value),
    parseLiteral: (ast) => {
        if (ast.kind === Kind.STRING) return ast.value;
        throw new Error("Value must be a string literal");
    },
});
