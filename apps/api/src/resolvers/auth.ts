import { GraphQLError } from "graphql";
import type { GraphQLContext } from "@/context";
import type { GraphQLResolveInfo } from "graphql";

/**
 * Authorization error for unauthenticated requests
 */
export class AuthenticationError extends GraphQLError {
    constructor(message = "Not authenticated") {
        super(message, {
            extensions: { code: "UNAUTHENTICATED" },
        });
    }
}

/**
 * Checks if the user is authenticated with both user and refresh token
 */
export function requireAuth(context: GraphQLContext): void {
    if (!context.user || !context.refreshToken) {
        throw new AuthenticationError();
    }
}

/**
 * Checks if the user exists (less strict than requireAuth)
 */
export function requireUser(context: GraphQLContext): void {
    if (!context.user) {
        throw new AuthenticationError();
    }
}

/**
 * Higher-order function that wraps resolvers requiring full authentication
 * (both user and refresh token)
 */
export function withAuth<TParent = any, TArgs = any, TResult = any>(
    resolver: (
        parent: TParent,
        args: TArgs,
        context: GraphQLContext & {
            user: NonNullable<GraphQLContext["user"]>;
            refreshToken: NonNullable<GraphQLContext["refreshToken"]>
        },
        info: GraphQLResolveInfo
    ) => Promise<TResult> | TResult
) {
    return async (
        parent: TParent,
        args: TArgs,
        context: GraphQLContext,
        info: GraphQLResolveInfo
    ): Promise<TResult> => {
        requireAuth(context);

        // TypeScript type assertion - we know user and refreshToken are defined after requireAuth
        const authenticatedContext = context as GraphQLContext & {
            user: NonNullable<GraphQLContext["user"]>;
            refreshToken: NonNullable<GraphQLContext["refreshToken"]>;
        };

        return resolver(parent, args, authenticatedContext, info);
    };
}

/**
 * Higher-order function that wraps resolvers requiring only user authentication
 * (user exists, but refresh token might not be needed)
 */
export function withUser<TParent = any, TArgs = any, TResult = any>(
    resolver: (
        parent: TParent,
        args: TArgs,
        context: GraphQLContext & { user: NonNullable<GraphQLContext["user"]> },
        info: GraphQLResolveInfo
    ) => Promise<TResult> | TResult
) {
    return async (
        parent: TParent,
        args: TArgs,
        context: GraphQLContext,
        info: GraphQLResolveInfo
    ): Promise<TResult> => {
        requireUser(context);

        // TypeScript type assertion - we know user is defined after requireUser
        const authenticatedContext = context as GraphQLContext & {
            user: NonNullable<GraphQLContext["user"]>;
        };

        return resolver(parent, args, authenticatedContext, info);
    };
}

/**
 * Utility function for manual authentication checks within resolvers
 * Returns the authenticated context with proper types
 */
export function ensureAuth(context: GraphQLContext) {
    requireAuth(context);
    return context as GraphQLContext & {
        user: NonNullable<GraphQLContext["user"]>;
        refreshToken: NonNullable<GraphQLContext["refreshToken"]>;
    };
}

/**
 * Utility function for manual user checks within resolvers
 * Returns the context with user properly typed as non-null
 */
export function ensureUser(context: GraphQLContext) {
    requireUser(context);
    return context as GraphQLContext & {
        user: NonNullable<GraphQLContext["user"]>;
    };
}
