import type { RefreshTokenMutation } from "../generated/graphql";
import { RefreshTokenDocument } from "../generated/graphql";
import { apolloClient } from "./apollo";

// Access tokens expire after 1 hour; refresh proactively before that so
// active sessions never die mid-use.
const REFRESH_INTERVAL_MS = 45 * 60 * 1000;

async function refreshAccessToken(): Promise<void> {
    if (!localStorage.getItem("access_token")) return;

    try {
        const result = await apolloClient.mutate<RefreshTokenMutation>({
            mutation: RefreshTokenDocument,
        });
        const accessToken = result.data?.refreshToken.accessToken;
        if (accessToken) {
            localStorage.setItem("access_token", accessToken);
        }
    } catch {
        // Ignore failures: if the session is truly dead, the errorLink's
        // UNAUTHENTICATED handling redirects to /auth on the next request.
    }
}

/**
 * Start the periodic session refresh. Returns a cleanup function that
 * stops it (for use in a useEffect).
 */
export function startSessionRefresh(): () => void {
    const intervalId = window.setInterval(
        refreshAccessToken,
        REFRESH_INTERVAL_MS,
    );
    return () => window.clearInterval(intervalId);
}
