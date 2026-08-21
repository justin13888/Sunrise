import { useApolloClient } from "@apollo/client";
import { createFileRoute, useRouter } from "@tanstack/react-router";
import { isTauri } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { useCallback, useEffect, useState } from "react";
import { Button } from "../components/ui/button";
import {
    useAuthenticateWithCodeMutation,
    useGetAuthUrlQuery,
} from "../generated/graphql";

const API_URL = import.meta.env.VITE_API_URL || "http://localhost:3000";

export const Route = createFileRoute("/auth")({
    component: AuthComponent,
});

function AuthComponent() {
    const router = useRouter();
    const client = useApolloClient();
    const runningInTauri = isTauri();
    const [isAuthenticating, setIsAuthenticating] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [showCodeInput, setShowCodeInput] = useState(runningInTauri);
    const [manualCode, setManualCode] = useState("");

    const {
        data: authUrlData,
        loading: authUrlLoading,
        error: authUrlError,
        refetch: refetchAuthUrl,
    } = useGetAuthUrlQuery({
        // Show the loading state again while a Retry refetch is in flight.
        notifyOnNetworkStatusChange: true,
    });
    const [authenticateWithCode] = useAuthenticateWithCodeMutation();

    // Already signed in? Go straight to the schedule.
    useEffect(() => {
        if (localStorage.getItem("access_token")) {
            router.navigate({ to: "/schedule" });
        }
    }, [router]);

    const submitCode = useCallback(
        async (code: string) => {
            const trimmed = code.trim();
            if (!trimmed) return;

            setIsAuthenticating(true);
            setError(null);

            try {
                const result = await authenticateWithCode({
                    variables: { code: trimmed },
                });

                if (result.data?.authenticateWithCode) {
                    const { accessToken } = result.data.authenticateWithCode;
                    localStorage.setItem("access_token", accessToken);
                    // Drop any cached data from a previous session so the
                    // new session starts clean.
                    try {
                        await client.resetStore();
                    } catch (resetErr) {
                        console.error(
                            "Failed to reset Apollo cache:",
                            resetErr,
                        );
                    }
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
        },
        [authenticateWithCode, client, router],
    );

    useEffect(() => {
        // The desktop flow uses the system browser + paste-code input; the
        // popup message flow only applies on the web.
        if (runningInTauri) return;

        let apiOrigin: string;
        try {
            apiOrigin = new URL(API_URL).origin;
        } catch (err) {
            // Malformed VITE_API_URL: skip the popup message listener rather
            // than crashing the page. The paste-code flow still works.
            console.error("Invalid API URL, popup sign-in disabled:", err);
            return;
        }
        const handleMessage = (event: MessageEvent) => {
            // Only accept messages from the API-served OAuth callback page.
            if (event.origin !== apiOrigin) return;
            if (event.data?.source !== "sunrise-oauth") return;

            if (event.data.code) {
                submitCode(event.data.code);
            } else if (event.data.error) {
                setError(`Authentication error: ${event.data.error}`);
            }
        };

        window.addEventListener("message", handleMessage);
        return () => window.removeEventListener("message", handleMessage);
    }, [runningInTauri, submitCode]);

    const handleSignIn = async () => {
        if (!authUrlData?.authUrl) return;
        setError(null);

        if (runningInTauri) {
            try {
                await openUrl(authUrlData.authUrl);
            } catch (err) {
                setError(
                    err instanceof Error
                        ? err.message
                        : "Failed to open the browser",
                );
            }
            return;
        }

        const popup = window.open(
            authUrlData.authUrl,
            "google-auth",
            "width=500,height=600,scrollbars=yes,resizable=yes",
        );

        if (popup) {
            popup.focus();
        } else {
            // Popup blocked - fall back to the paste-code flow.
            setShowCodeInput(true);
        }
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

    if (authUrlError && !authUrlData?.authUrl) {
        return (
            <div className="min-h-screen flex items-center justify-center bg-gray-50">
                <div className="max-w-md w-full bg-white rounded-lg shadow-md p-8 text-center">
                    <h1 className="text-2xl font-bold text-gray-900 mb-4">
                        Welcome to Sunrise
                    </h1>
                    <div className="bg-red-50 border border-red-200 text-red-700 px-4 py-3 rounded mb-6">
                        Could not reach the sign-in service:{" "}
                        {authUrlError.message}
                    </div>
                    <Button onClick={() => refetchAuthUrl()} className="w-full">
                        Retry
                    </Button>
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

                    {runningInTauri && (
                        <p className="text-gray-500 text-sm mt-4">
                            Your browser will open to sign in. Afterwards, copy
                            the authorization code shown there and paste it
                            below.
                        </p>
                    )}

                    {showCodeInput ? (
                        <div className="mt-6 text-left">
                            <label
                                htmlFor="auth-code"
                                className="block text-sm font-medium text-gray-700 mb-1"
                            >
                                Authorization code
                            </label>
                            <div className="flex gap-2">
                                <input
                                    id="auth-code"
                                    type="text"
                                    value={manualCode}
                                    onChange={(e) =>
                                        setManualCode(e.target.value)
                                    }
                                    onKeyDown={(e) => {
                                        if (e.key === "Enter") {
                                            submitCode(manualCode);
                                        }
                                    }}
                                    disabled={isAuthenticating}
                                    className="flex-1 border border-gray-300 rounded-md px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500"
                                    placeholder="Paste authorization code"
                                />
                                <Button
                                    onClick={() => submitCode(manualCode)}
                                    disabled={
                                        isAuthenticating || !manualCode.trim()
                                    }
                                >
                                    Submit
                                </Button>
                            </div>
                        </div>
                    ) : (
                        <button
                            type="button"
                            onClick={() => setShowCodeInput(true)}
                            className="mt-4 text-sm text-blue-600 hover:text-blue-800 underline"
                        >
                            Having trouble with the popup? Paste an
                            authorization code instead
                        </button>
                    )}
                </div>
            </div>
        </div>
    );
}
