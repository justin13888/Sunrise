import { FormatRegistry, type Static, type TSchema } from "@sinclair/typebox";
import { TypeCompiler } from "@sinclair/typebox/compiler";

// TypeBox ships no string formats by default; register the ones our schemas
// reference so `format: "uuid"` constraints are actually enforced.
const UUID_PATTERN =
    /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/;
if (!FormatRegistry.Has("uuid")) {
    FormatRegistry.Set("uuid", (value) => UUID_PATTERN.test(value));
}

/** A single validation problem, with a JSON-pointer-style path. */
export interface ValidationIssue {
    path: string;
    message: string;
}

/** Compiled validator for a TypeBox schema. */
export interface Validator<T extends TSchema> {
    /** Type guard: returns true if `value` conforms to the schema. */
    check(value: unknown): value is Static<T>;
    /**
     * Returns `value` typed as the schema's static type, or throws an Error
     * with a readable message listing the validation problems.
     */
    assert(value: unknown, label?: string): Static<T>;
    /** Returns every validation problem for `value` (empty when valid). */
    errors(value: unknown): ValidationIssue[];
}

function formatIssues(issues: ValidationIssue[]): string {
    return issues
        .map(
            (issue) =>
                `${issue.path === "" ? "/" : issue.path}: ${issue.message}`,
        )
        .join("; ");
}

/**
 * Compiles a TypeBox schema into a reusable validator with `check`, `assert`,
 * and `errors` helpers.
 */
export function createValidator<T extends TSchema>(schema: T): Validator<T> {
    const compiled = TypeCompiler.Compile(schema);

    const errors = (value: unknown): ValidationIssue[] =>
        [...compiled.Errors(value)].map((error) => ({
            path: error.path,
            message: error.message,
        }));

    return {
        check: (value): value is Static<T> => compiled.Check(value),
        assert: (value, label = "value") => {
            if (compiled.Check(value)) return value;
            throw new Error(`Invalid ${label}: ${formatIssues(errors(value))}`);
        },
        errors,
    };
}
