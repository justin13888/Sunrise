import { GoogleCalendarService } from "@sunrise/gcal";
import { verifyJWT } from "./services/jwt";
import { tokenStore } from "./services/tokenStore";
import type { User } from "./types/user";

export interface GraphQLContext {
    user?: User;
    refreshToken?: string;
    calendarService: GoogleCalendarService;
    req: Request;
}

export async function createContext(req: Request): Promise<GraphQLContext> {
    const authHeader = req.headers.get("authorization");

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

    if (authHeader?.startsWith("Bearer ")) {
        const token = authHeader.slice(7);
        const payload = await verifyJWT(token);
        if (payload) {
            try {
                const tokens = await tokenStore.getTokens(payload.userId);
                if (tokens?.email) {
                    user = {
                        id: tokens.userId,
                        email: tokens.email,
                        name: tokens.name || tokens.email,
                        picture: tokens.picture,
                        verified: true,
                    };
                    refreshToken = tokens.refreshToken;
                }
            } catch (error) {
                console.warn("Failed to load tokens for user:", payload.userId, error);
            }
        }
    }

    return {
        user,
        refreshToken,
        calendarService,
        req,
    };
}
