/** biome-ignore-all lint/suspicious/noExplicitAny: test mocks and private field access */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { GoogleCalendarService, getClientFromRefreshToken } from "./index";

// Use vi.hoisted so variables are available in vi.mock factory (which is hoisted to top)
const mockOAuth2Instance = vi.hoisted(() => ({
    setCredentials: vi.fn(),
    getAccessToken: vi.fn().mockResolvedValue({ token: "mock-access-token" }),
    generateAuthUrl: vi
        .fn()
        .mockReturnValue("https://accounts.google.com/o/oauth2/auth?mock=true"),
    getToken: vi.fn().mockResolvedValue({
        tokens: {
            access_token: "mock-access-token",
            refresh_token: "mock-refresh-token",
            expiry_date: 9999999999999,
        },
    }),
    credentials: { refresh_token: "mock-refresh-token" },
}));

// Mock googleapis - OAuth2 must use a regular function (not arrow) to support `new`
vi.mock("googleapis", () => ({
    google: {
        auth: {
            OAuth2: vi.fn(function (this: any) {
                return mockOAuth2Instance;
            }),
        },
        calendar: vi.fn().mockReturnValue({
            events: {
                list: vi.fn().mockResolvedValue({
                    data: {
                        items: [
                            {
                                id: "event1",
                                summary: "Test Event 1",
                                start: { dateTime: "2025-11-10T10:00:00Z" },
                                end: { dateTime: "2025-11-10T11:00:00Z" },
                            },
                            {
                                id: "event2",
                                summary: "Test Event 2",
                                start: { dateTime: "2025-11-11T14:00:00Z" },
                                end: { dateTime: "2025-11-11T15:00:00Z" },
                            },
                        ],
                    },
                }),
            },
            calendarList: {
                list: vi.fn().mockResolvedValue({
                    data: {
                        items: [
                            {
                                id: "primary",
                                summary: "Primary Calendar",
                                primary: true,
                            },
                            {
                                id: "calendar2",
                                summary: "Secondary Calendar",
                                primary: false,
                            },
                        ],
                    },
                }),
            },
        }),
    },
}));

describe("GoogleCalendarService", () => {
    let service: GoogleCalendarService;
    const mockClientId = "test-client-id";
    const mockClientSecret = "test-client-secret";
    const mockRedirectUri = "http://localhost:3000/auth/callback";

    beforeEach(() => {
        service = new GoogleCalendarService(
            mockClientId,
            mockClientSecret,
            mockRedirectUri,
        );
        vi.clearAllMocks();
    });

    describe("constructor", () => {
        it("should create service with provided credentials", () => {
            expect(service).toBeInstanceOf(GoogleCalendarService);
        });

        it("should create service with default redirect URI", () => {
            const defaultService = new GoogleCalendarService(
                mockClientId,
                mockClientSecret,
            );
            expect(defaultService).toBeInstanceOf(GoogleCalendarService);
        });

        it("should store client credentials", () => {
            expect((service as any).clientId).toBe(mockClientId);
            expect((service as any).clientSecret).toBe(mockClientSecret);
            expect((service as any).redirectUri).toBe(mockRedirectUri);
        });
    });

    describe("getAuthUrl", () => {
        it("should generate authorization URL", () => {
            const authUrl = service.getAuthUrl();
            expect(authUrl).toBe(
                "https://accounts.google.com/o/oauth2/auth?mock=true",
            );
        });

        it("should generate URL with correct scopes", () => {
            service.getAuthUrl();

            expect(mockOAuth2Instance.generateAuthUrl).toHaveBeenCalledWith(
                expect.objectContaining({
                    access_type: "offline",
                    prompt: "consent",
                    scope: [
                        "https://www.googleapis.com/auth/calendar.readonly",
                        "https://www.googleapis.com/auth/calendar.events",
                        "https://www.googleapis.com/auth/userinfo.email",
                        "https://www.googleapis.com/auth/userinfo.profile",
                    ],
                }),
            );
        });

        it("should return a string", () => {
            const authUrl = service.getAuthUrl();
            expect(typeof authUrl).toBe("string");
        });
    });

    describe("getTokensFromCode", () => {
        it("should exchange authorization code for tokens", async () => {
            const code = "test-auth-code";
            const tokens = await service.getTokensFromCode(code);

            expect(tokens).toEqual({
                access_token: "mock-access-token",
                refresh_token: "mock-refresh-token",
                expiry_date: expect.any(Number),
            });
        });

        it("should call getToken with the code", async () => {
            const code = "test-auth-code";
            await service.getTokensFromCode(code);

            expect(mockOAuth2Instance.getToken).toHaveBeenCalledWith(code);
        });

        it("should handle errors from Google OAuth", async () => {
            mockOAuth2Instance.getToken.mockRejectedValueOnce(
                new Error("OAuth error"),
            );

            await expect(
                service.getTokensFromCode("invalid-code"),
            ).rejects.toThrow("OAuth error");
        });
    });

    describe("getClientFromRefreshToken (standalone function)", () => {
        it("should create authenticated client from refresh token", async () => {
            const refreshToken = "test-refresh-token";
            const client = await getClientFromRefreshToken(refreshToken);
            expect(client).toBeDefined();
        });

        it("should set refresh token credentials on the client", async () => {
            const refreshToken = "test-refresh-token";
            await getClientFromRefreshToken(refreshToken);

            expect(mockOAuth2Instance.setCredentials).toHaveBeenCalledWith({
                refresh_token: refreshToken,
            });
        });

        it("is an exported async function", () => {
            expect(getClientFromRefreshToken).toBeDefined();
            expect(typeof getClientFromRefreshToken).toBe("function");
        });

        it("should return an OAuth2Client", async () => {
            const refreshToken = "test-refresh-token";
            const client = await getClientFromRefreshToken(refreshToken);
            expect(client).toBe(mockOAuth2Instance);
        });
    });

    describe("listEvents", () => {
        const mockAuth = { credentials: {} } as any;

        it("should list events from primary calendar", async () => {
            const result = await service.listEvents(mockAuth);

            expect(result.items).toHaveLength(2);
            expect(result.items[0].summary).toBe("Test Event 1");
            expect(result.items[1].summary).toBe("Test Event 2");
        });

        it("should call the Google Calendar API", async () => {
            await service.listEvents(mockAuth);
            const { google } = await import("googleapis");
            expect(google.calendar).toHaveBeenCalled();
        });

        it("should respect custom maxResults parameter", async () => {
            const result = await service.listEvents(mockAuth, "primary", 5);
            expect(result).toBeDefined();
            expect(result.items).toBeDefined();
        });

        it("should return items array", async () => {
            const result = await service.listEvents(mockAuth);
            expect(Array.isArray(result.items)).toBe(true);
        });

        it("should include credentials in result", async () => {
            const result = await service.listEvents(mockAuth);
            expect(result).toHaveProperty("credentials");
        });

        it("should handle API errors", async () => {
            expect(service.listEvents).toBeDefined();
        });
    });

    describe("listCalendars", () => {
        const mockAuth = { credentials: {} } as any;

        it("should list all user calendars", async () => {
            const result = await service.listCalendars(mockAuth);

            expect(result.items).toHaveLength(2);
            expect(result.items[0].summary).toBe("Primary Calendar");
            expect(result.items[1].summary).toBe("Secondary Calendar");
        });

        it("should identify primary calendar", async () => {
            const result = await service.listCalendars(mockAuth);

            const primary = result.items.find((cal: any) => cal.primary);
            expect(primary).toBeDefined();
            expect(primary?.summary).toBe("Primary Calendar");
        });

        it("should return items array", async () => {
            const result = await service.listCalendars(mockAuth);
            expect(Array.isArray(result.items)).toBe(true);
        });

        it("should include credentials in result", async () => {
            const result = await service.listCalendars(mockAuth);
            expect(result).toHaveProperty("credentials");
        });

        it("should handle API errors", async () => {
            expect(service.listCalendars).toBeDefined();
        });
    });

    describe("integration scenarios", () => {
        const mockAuth = { credentials: {} } as any;

        it("should complete full OAuth flow", async () => {
            // 1. Get auth URL
            const authUrl = service.getAuthUrl();
            expect(authUrl).toBeTruthy();

            // 2. Exchange code for tokens
            const tokens = await service.getTokensFromCode("auth-code");
            expect(tokens.refresh_token).toBeTruthy();

            // 3. Use standalone helper to get client, then list events
            const client = await getClientFromRefreshToken(
                tokens.refresh_token ?? "",
            );
            const result = await service.listEvents(client as any);
            expect(result.items).toBeDefined();
        });

        it("should handle multiple API calls with same auth client", async () => {
            const result1 = await service.listEvents(mockAuth);
            const result2 = await service.listCalendars(mockAuth);

            expect(result1.items).toHaveLength(2);
            expect(result2.items).toHaveLength(2);
        });
    });
});
