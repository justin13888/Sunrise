import { GoogleCalendarService } from '@sunrise/gcal'
import type { User } from './types/user'
import { tokenStore } from './services/tokenStore'

export interface GraphQLContext {
    user?: User
    refreshToken?: string
    calendarService: GoogleCalendarService
    req: Request
}

// TODO: Review this function vv
export async function createContext(req: Request): Promise<GraphQLContext> {
    // Extract user info and refresh token from headers/session
    const authHeader = req.headers.get('authorization')
    const refreshTokenHeader = req.headers.get('x-refresh-token')
    const userIdHeader = req.headers.get('x-user-id') // For temporary user ID passing

    const clientId = process.env.GOOGLE_CLIENT_ID
    const clientSecret = process.env.GOOGLE_CLIENT_SECRET

    if (!clientId || !clientSecret) {
        throw new Error('Missing required Google OAuth credentials')
    }

    const calendarService = new GoogleCalendarService(
        clientId,
        clientSecret,
        process.env.GOOGLE_REDIRECT_URI || 'http://localhost:3000/auth/callback'
    )

    let user: User | undefined
    let refreshToken: string | undefined

    // TODO: Implement proper JWT token validation
    // For now, we'll use a temporary approach with user ID header
    if (userIdHeader) {
        try {
            const tokens = await tokenStore.getTokens(userIdHeader)
            if (tokens) {
                user = {
                    id: tokens.userId,
                    email: tokens.email || 'unknown@example.com',
                    name: tokens.name || 'Unknown User',
                    picture: tokens.picture,
                    verified: true
                }
                refreshToken = tokens.refreshToken
            }
        } catch (error) {
            console.warn('Failed to load tokens for user:', userIdHeader, error)
        }
    }

    // Legacy approach: if refresh token is provided directly in header
    if (!user && refreshTokenHeader) {
        // In production, you'd validate the token and extract user info
        user = {
            id: 'legacy-user',
            email: 'user@example.com',
            name: 'Legacy User',
            verified: true
        }
        refreshToken = refreshTokenHeader
    }

    return {
        user,
        refreshToken,
        calendarService,
        req
    }
}
