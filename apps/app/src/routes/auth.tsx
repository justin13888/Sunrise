import { createFileRoute, useRouter } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { Button } from "../components/ui/button";
import {
    useAuthenticateWithCodeMutation,
    useGetAuthUrlQuery,
} from "../generated/graphql";

export const Route = createFileRoute("/auth")({
    component: AuthComponent,
});

function AuthComponent() {
    const router = useRouter();
    const [isAuthenticating, setIsAuthenticating] = useState(false);
    const [error, setError] = useState<string | null>(null);

    const { data: authUrlData, loading: authUrlLoading } = useGetAuthUrlQuery();
    const [authenticateWithCode] = useAuthenticateWithCodeMutation();

    useEffect(() => {
        // Handle the auth callback from popup
        const handleMessage = async (event: MessageEvent) => {
            // Allow messages from the API server (localhost:3000) where OAuth callback is handled
            const allowedOrigins = [
                "http://localhost:3000",
                "http://localhost:1420",
            ];
            if (!allowedOrigins.includes(event.origin)) {
                console.log("Rejected message from origin:", event.origin);
                return;
            }

            console.log("📨 Received message from popup:", event.data);

            if (event.data.code) {
                setIsAuthenticating(true);
                setError(null);

                try {
                    const result = await authenticateWithCode({
                        variables: { code: event.data.code },
                    });

                    if (result.data?.authenticateWithCode) {
                        const { accessToken, user } =
                            result.data.authenticateWithCode;

                        localStorage.setItem("access_token", accessToken);

                        console.log(
                            "✅ Successfully authenticated:",
                            user.email,
                        );

                        // Navigate to schedule page
                        router.navigate({ to: "/schedule" });
                    }
                } catch (err) {
                    setError(
                        err instanceof Error
                            ? err.message
                            : "Authentication failed",
                    );
                } finally {
                    setIsAuthenticating(false);
                }
            } else if (event.data.error) {
                setError(`Authentication error: ${event.data.error}`);
            }
        };

        window.addEventListener("message", handleMessage);
        return () => window.removeEventListener("message", handleMessage);
    }, [authenticateWithCode, router]);

    const handleSignIn = () => {
        if (!authUrlData?.authUrl) return;

        const popup = window.open(
            authUrlData.authUrl,
            "google-auth",
            "width=500,height=600,scrollbars=yes,resizable=yes",
        );

        // Focus the popup if it exists
        popup?.focus();
    };

    if (authUrlLoading) {
        return (
            <div className="min-h-screen flex items-center justify-center">
                <div className="text-center">
                    <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-gray-900 mx-auto mb-4"></div>
                    <p>Loading authentication...</p>
                </div>
            </div>
        );
    }

    return (
        <div className="min-h-screen flex items-center justify-center bg-gray-50">
            <div className="max-w-md w-full bg-white rounded-lg shadow-md p-8">
                <div className="text-center">
                    <h1 className="text-3xl font-bold text-gray-900 mb-2">
                        Welcome to Sunrise
                    </h1>
                    <p className="text-gray-600 mb-8">
                        Connect your Google Calendar to get started
                    </p>

                    {error && (
                        <div className="bg-red-50 border border-red-200 text-red-700 px-4 py-3 rounded mb-6">
                            {error}
                        </div>
                    )}

                    <Button
                        onClick={handleSignIn}
                        disabled={isAuthenticating || !authUrlData?.authUrl}
                        className="w-full"
                    >
                        {isAuthenticating ? (
                            <>
                                <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white mr-2"></div>
                                Authenticating...
                            </>
                        ) : (
                            <>
                                <svg
                                    className="w-5 h-5 mr-2"
                                    viewBox="0 0 24 24"
                                    aria-hidden="true"
                                >
                                    <path
                                        fill="currentColor"
                                        d="M22.56 12.25c0-.78-.07-1.53-.2-2.25H12v4.26h5.92c-.26 1.37-1.04 2.53-2.21 3.31v2.77h3.57c2.08-1.92 3.28-4.74 3.28-8.09z"
                                    />
                                    <path
                                        fill="currentColor"
                                        d="M12 23c2.97 0 5.46-.98 7.28-2.66l-3.57-2.77c-.98.66-2.23 1.06-3.71 1.06-2.86 0-5.29-1.93-6.16-4.53H2.18v2.84C3.99 20.53 7.7 23 12 23z"
                                    />
                                    <path
                                        fill="currentColor"
                                        d="M5.84 14.09c-.22-.66-.35-1.36-.35-2.09s.13-1.43.35-2.09V7.07H2.18C1.43 8.55 1 10.22 1 12s.43 3.45 1.18 4.93l2.85-2.22.81-.62z"
                                    />
                                    <path
                                        fill="currentColor"
                                        d="M12 5.38c1.62 0 3.06.56 4.21 1.64l3.15-3.15C17.45 2.09 14.97 1 12 1 7.7 1 3.99 3.47 2.18 7.07l3.66 2.84c.87-2.6 3.3-4.53 6.16-4.53z"
                                    />
                                </svg>
                                Sign in with Google
                            </>
                        )}
                    </Button>
                </div>
            </div>
        </div>
    );
}
