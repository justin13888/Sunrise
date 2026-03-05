import { GoogleCalendarService } from "@sunrise/gcal";
import { tokenStore } from "./services/tokenStore";
import type { User } from "./types/user";

export interface GraphQLContext {
    user?: User;
    refreshToken?: string;
    calendarService: GoogleCalendarService;
    req: Request;
}

// TODO: Review this function vv
export async function createContext(req: Request): Promise<GraphQLContext> {
    // Extract user info and refresh token from headers/session
    const _authHeader = req.headers.get("authorization");
    const refreshTokenHeader = req.headers.get("x-refresh-token");
    const userIdHeader = req.headers.get("x-user-id"); // For temporary user ID passing

    const clientId = process.env.GOOGLE_CLIENT_ID;
    const clientSecret = process.env.GOOGLE_CLIENT_SECRET;

    if (!clientId || !clientSecret) {
        throw new Error("Missing required Google OAuth credentials");
    }

    const calendarService = new GoogleCalendarService(
        clientId,
        clientSecret,
        process.env.GOOGLE_REDIRECT_URI ||
            "http://localhost:3000/auth/callback",
    );

    let user: User | undefined;
    let refreshToken: string | undefined;

    // TODO: Implement proper JWT token validation
    // For now, we'll use a temporary approach with user ID header
    if (userIdHeader) {
        console.log("🔍 Context - userIdHeader:", userIdHeader);
        try {
            const tokens = await tokenStore.getTokens(userIdHeader);
            console.log("🔍 Context - tokens loaded:", tokens ? "yes" : "no");
            if (tokens) {
                // Validate that we have the essential user information
                if (!tokens.email) {
                    console.error(
                        "❌ Context - stored tokens missing email for user:",
                        userIdHeader,
                    );
                    console.error(
                        "Token data may be corrupted. User should re-authenticate.",
                    );
                    // Don't set user context if data is incomplete
                } else {
                    user = {
                        id: tokens.userId,
                        email: tokens.email,
                        name: tokens.name || tokens.email, // Use email as fallback for name only
                        picture: tokens.picture,
                        verified: true,
                    };
                    refreshToken = tokens.refreshToken;
                    console.log("✅ Context - user set:", user.email);
                }
            } else {
                console.log(
                    "❌ Context - tokens not found or expired for user:",
                    userIdHeader,
                );
            }
        } catch (error) {
            console.warn(
                "Failed to load tokens for user:",
                userIdHeader,
                error,
            );
        }
    } else {
        console.log("❌ Context - no userIdHeader provided");
    }

    // Legacy approach: if refresh token is provided directly in header
    // This should not be used in production
    if (!user && refreshTokenHeader) {
        console.warn(
            "⚠️  Using legacy refresh token header - this should not be used in production",
        );
        console.warn(
            "User information cannot be determined from refresh token alone",
        );
        // Don't create a fake user - let the query fail if user is required
        refreshToken = refreshTokenHeader;
    }

    return {
        user,
        refreshToken,
        calendarService,
        req,
    };
}
