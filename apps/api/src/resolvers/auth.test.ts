import type { GoogleCalendarService } from "@sunrise/gcal";
import type { GraphQLResolveInfo } from "graphql";
import { describe, expect, it, vi } from "vitest";
import type { GraphQLContext } from "../context";
import {
    AuthenticationError,
    ensureAuth,
    ensureUser,
    requireAuth,
    requireUser,
    withAuth,
    withUser,
} from "./auth";

describe("AuthenticationError", () => {
    it("should create an error with default message", () => {
        const error = new AuthenticationError();
        expect(error.message).toBe("Not authenticated");
        expect(error.extensions?.code).toBe("UNAUTHENTICATED");
    });

    it("should create an error with custom message", () => {
        const customMessage = "Custom auth error";
        const error = new AuthenticationError(customMessage);
        expect(error.message).toBe(customMessage);
        expect(error.extensions?.code).toBe("UNAUTHENTICATED");
    });

    it("should be instance of GraphQLError", () => {
        const error = new AuthenticationError();
        expect(error.name).toBe("GraphQLError");
    });
});

describe("requireAuth", () => {
    it("should not throw when user and refreshToken are present", () => {
        const context: GraphQLContext = {
            user: {
                id: "user1",
                email: "user1@example.com",
                verified: true,
            },
            refreshToken: "refresh-token",
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        expect(() => requireAuth(context)).not.toThrow();
    });

    it("should throw when user is missing", () => {
        const context: GraphQLContext = {
            refreshToken: "refresh-token",
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        expect(() => requireAuth(context)).toThrow(AuthenticationError);
        expect(() => requireAuth(context)).toThrow("Not authenticated");
    });

    it("should throw when refreshToken is missing", () => {
        const context: GraphQLContext = {
            user: {
                id: "user1",
                email: "user1@example.com",
                verified: true,
            },
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        expect(() => requireAuth(context)).toThrow(AuthenticationError);
    });

    it("should throw when both user and refreshToken are missing", () => {
        const context: GraphQLContext = {
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        expect(() => requireAuth(context)).toThrow(AuthenticationError);
    });
});

describe("requireUser", () => {
    it("should not throw when user is present", () => {
        const context: GraphQLContext = {
            user: {
                id: "user1",
                email: "user1@example.com",
                verified: true,
            },
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        expect(() => requireUser(context)).not.toThrow();
    });

    it("should not throw when user is present without refreshToken", () => {
        const context: GraphQLContext = {
            user: {
                id: "user1",
                email: "user1@example.com",
                verified: true,
            },
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        expect(() => requireUser(context)).not.toThrow();
    });

    it("should throw when user is missing", () => {
        const context: GraphQLContext = {
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        expect(() => requireUser(context)).toThrow(AuthenticationError);
        expect(() => requireUser(context)).toThrow("Not authenticated");
    });
});

describe("withAuth", () => {
    it("should call resolver when user and refreshToken are present", async () => {
        const mockResolver = vi.fn().mockResolvedValue("result");
        const wrappedResolver = withAuth(mockResolver);

        const context: GraphQLContext = {
            user: {
                id: "user1",
                email: "user1@example.com",
                verified: true,
            },
            refreshToken: "refresh-token",
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        const result = await wrappedResolver(
            {},
            {},
            context,
            {} as GraphQLResolveInfo,
        );

        expect(result).toBe("result");
        expect(mockResolver).toHaveBeenCalledTimes(1);
        expect(mockResolver).toHaveBeenCalledWith(
            {},
            {},
            expect.objectContaining({
                user: context.user,
                refreshToken: context.refreshToken,
            }),
            {},
        );
    });

    it("should throw when authentication is missing", async () => {
        const mockResolver = vi.fn();
        const wrappedResolver = withAuth(mockResolver);

        const context: GraphQLContext = {
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        await expect(
            wrappedResolver({}, {}, context, {} as GraphQLResolveInfo),
        ).rejects.toThrow(AuthenticationError);

        expect(mockResolver).not.toHaveBeenCalled();
    });

    it("should properly type the context in resolver", async () => {
        const mockResolver = vi.fn((_parent, _args, context) => {
            // TypeScript should know user and refreshToken are non-null
            return context.user.email;
        });
        const wrappedResolver = withAuth(mockResolver);

        const context: GraphQLContext = {
            user: {
                id: "user1",
                email: "test@example.com",
                verified: true,
            },
            refreshToken: "refresh-token",
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        const result = await wrappedResolver(
            {},
            {},
            context,
            {} as GraphQLResolveInfo,
        );
        expect(result).toBe("test@example.com");
    });

    it("should propagate resolver errors", async () => {
        const error = new Error("Resolver error");
        const mockResolver = vi.fn().mockRejectedValue(error);
        const wrappedResolver = withAuth(mockResolver);

        const context: GraphQLContext = {
            user: {
                id: "user1",
                email: "user1@example.com",
                verified: true,
            },
            refreshToken: "refresh-token",
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        await expect(
            wrappedResolver({}, {}, context, {} as GraphQLResolveInfo),
        ).rejects.toThrow("Resolver error");
    });
});

describe("withUser", () => {
    it("should call resolver when user is present", async () => {
        const mockResolver = vi.fn().mockResolvedValue("result");
        const wrappedResolver = withUser(mockResolver);

        const context: GraphQLContext = {
            user: {
                id: "user1",
                email: "user1@example.com",
                verified: true,
            },
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        const result = await wrappedResolver(
            {},
            {},
            context,
            {} as GraphQLResolveInfo,
        );

        expect(result).toBe("result");
        expect(mockResolver).toHaveBeenCalledTimes(1);
    });

    it("should call resolver even without refreshToken", async () => {
        const mockResolver = vi.fn().mockResolvedValue("result");
        const wrappedResolver = withUser(mockResolver);

        const context: GraphQLContext = {
            user: {
                id: "user1",
                email: "user1@example.com",
                verified: true,
            },
            // No refreshToken
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        const result = await wrappedResolver(
            {},
            {},
            context,
            {} as GraphQLResolveInfo,
        );

        expect(result).toBe("result");
        expect(mockResolver).toHaveBeenCalledTimes(1);
    });

    it("should throw when user is missing", async () => {
        const mockResolver = vi.fn();
        const wrappedResolver = withUser(mockResolver);

        const context: GraphQLContext = {
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        await expect(
            wrappedResolver({}, {}, context, {} as GraphQLResolveInfo),
        ).rejects.toThrow(AuthenticationError);

        expect(mockResolver).not.toHaveBeenCalled();
    });

    it("should properly type the context in resolver", async () => {
        const mockResolver = vi.fn((_parent, _args, context) => {
            // TypeScript should know user is non-null
            return context.user.id;
        });
        const wrappedResolver = withUser(mockResolver);

        const context: GraphQLContext = {
            user: {
                id: "user123",
                email: "user@example.com",
                verified: true,
            },
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        const result = await wrappedResolver(
            {},
            {},
            context,
            {} as GraphQLResolveInfo,
        );
        expect(result).toBe("user123");
    });
});

describe("ensureAuth", () => {
    it("should return properly typed context when authenticated", () => {
        const context: GraphQLContext = {
            user: {
                id: "user1",
                email: "user1@example.com",
                verified: true,
            },
            refreshToken: "refresh-token",
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        const result = ensureAuth(context);

        expect(result.user).toBe(context.user);
        expect(result.refreshToken).toBe(context.refreshToken);
        expect(result.calendarService).toBe(context.calendarService);
    });

    it("should throw when authentication is missing", () => {
        const context: GraphQLContext = {
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        expect(() => ensureAuth(context)).toThrow(AuthenticationError);
    });

    it("should throw when only user is present", () => {
        const context: GraphQLContext = {
            user: {
                id: "user1",
                email: "user1@example.com",
                verified: true,
            },
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        expect(() => ensureAuth(context)).toThrow(AuthenticationError);
    });

    it("should throw when only refreshToken is present", () => {
        const context: GraphQLContext = {
            refreshToken: "refresh-token",
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        expect(() => ensureAuth(context)).toThrow(AuthenticationError);
    });
});

describe("ensureUser", () => {
    it("should return properly typed context when user is present", () => {
        const context: GraphQLContext = {
            user: {
                id: "user1",
                email: "user1@example.com",
                verified: true,
                name: "Test User",
                picture: "https://example.com/pic.jpg",
            },
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        const result = ensureUser(context);

        expect(result.user).toBe(context.user);
        expect(result.user.id).toBe("user1");
        expect(result.user.email).toBe("user1@example.com");
        expect(result.user.name).toBe("Test User");
    });

    it("should work without refreshToken", () => {
        const context: GraphQLContext = {
            user: {
                id: "user1",
                email: "user1@example.com",
                verified: true,
            },
            // No refreshToken
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        const result = ensureUser(context);
        expect(result.user).toBe(context.user);
    });

    it("should throw when user is missing", () => {
        const context: GraphQLContext = {
            refreshToken: "refresh-token",
            calendarService: {} as GoogleCalendarService,
            req: new Request("http://localhost"),
        };

        expect(() => ensureUser(context)).toThrow(AuthenticationError);
    });
});
