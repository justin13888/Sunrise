/** biome-ignore-all lint/suspicious/noExplicitAny: test mocks and private field access */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { GoogleCalendarService } from "./index";

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

const mockUserInfoGet = vi.hoisted(() =>
    vi.fn().mockResolvedValue({
        data: {
            email: "user@example.com",
            name: "Test User",
            picture: "https://example.com/avatar.png",
        },
    }),
);

// Mock googleapis - OAuth2 must use a regular function (not arrow) to support `new`
vi.mock("googleapis", () => ({
    google: {
        auth: {
            OAuth2: vi.fn(function (this: any) {
                return mockOAuth2Instance;
            }),
        },
        oauth2: vi.fn().mockImplementation(() => ({
            userinfo: {
                get: mockUserInfoGet,
            },
        })),
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
                get: vi.fn().mockResolvedValue({
                    data: {
                        id: "event1",
                        summary: "Test Event 1",
                        start: { dateTime: "2025-11-10T10:00:00Z" },
                        end: { dateTime: "2025-11-10T11:00:00Z" },
                    },
                }),
                insert: vi.fn().mockResolvedValue({
                    data: {
                        id: "new-event-id",
                        summary: "New Event",
                        start: { dateTime: "2025-12-01T09:00:00Z" },
                        end: { dateTime: "2025-12-01T10:00:00Z" },
                    },
                }),
                patch: vi.fn().mockResolvedValue({
                    data: {
                        id: "event1",
                        summary: "Updated Event",
                        start: { dateTime: "2025-12-01T09:00:00Z" },
                        end: { dateTime: "2025-12-01T10:00:00Z" },
                    },
                }),
                delete: vi.fn().mockResolvedValue({ data: {} }),
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

    describe("createAuthenticatedClient", () => {
        it("should set the provided credentials on the client", () => {
            service.createAuthenticatedClient({
                access_token: "access",
                refresh_token: "refresh",
                expiry_date: 123456,
            });

            expect(mockOAuth2Instance.setCredentials).toHaveBeenCalledWith({
                access_token: "access",
                refresh_token: "refresh",
                expiry_date: 123456,
                token_type: "Bearer",
            });
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

    describe("getUserInfo", () => {
        const mockAuth = { credentials: {} } as any;

        it("should return email, name, and picture", async () => {
            const info = await service.getUserInfo(mockAuth);

            expect(info).toEqual({
                email: "user@example.com",
                name: "Test User",
                picture: "https://example.com/avatar.png",
            });
        });

        it("should call the oauth2 v2 userinfo endpoint with the auth client", async () => {
            await service.getUserInfo(mockAuth);
            const { google } = await import("googleapis");
            expect(google.oauth2).toHaveBeenCalledWith({
                version: "v2",
                auth: mockAuth,
            });
            expect(mockUserInfoGet).toHaveBeenCalled();
        });

        it("should omit missing name and picture", async () => {
            mockUserInfoGet.mockResolvedValueOnce({
                data: { email: "user@example.com" },
            });

            const info = await service.getUserInfo(mockAuth);
            expect(info).toEqual({
                email: "user@example.com",
                name: undefined,
                picture: undefined,
            });
        });

        it("should throw when Google returns no email", async () => {
            mockUserInfoGet.mockResolvedValueOnce({
                data: { name: "No Email" },
            });

            await expect(service.getUserInfo(mockAuth)).rejects.toThrow(
                "no email in response",
            );
        });

        it("should propagate API errors", async () => {
            mockUserInfoGet.mockRejectedValueOnce(new Error("userinfo failed"));

            await expect(service.getUserInfo(mockAuth)).rejects.toThrow(
                "userinfo failed",
            );
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

        it("should pass timeMin/timeMax strings through to the API", async () => {
            await service.listEvents(
                mockAuth,
                "primary",
                20,
                undefined,
                "2025-11-01T00:00:00.000Z",
                "2025-12-01T00:00:00.000Z",
            );
            const { google } = await import("googleapis");
            const calendarInstance = (google.calendar as any).mock.results[0]
                ?.value;
            expect(calendarInstance.events.list).toHaveBeenCalledWith(
                expect.objectContaining({
                    timeMin: "2025-11-01T00:00:00.000Z",
                    timeMax: "2025-12-01T00:00:00.000Z",
                }),
            );
        });

        it("should not default timeMin when it is not provided", async () => {
            await service.listEvents(mockAuth);
            const { google } = await import("googleapis");
            const calendarInstance = (google.calendar as any).mock.results[0]
                ?.value;
            expect(calendarInstance.events.list).toHaveBeenCalledWith(
                expect.objectContaining({
                    timeMin: undefined,
                    timeMax: undefined,
                }),
            );
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
    });

    describe("getEvent", () => {
        const mockAuth = { credentials: {} } as any;

        it("should fetch a single event", async () => {
            const event = await service.getEvent(mockAuth, "primary", "event1");
            expect(event.id).toBe("event1");
            expect(event.summary).toBe("Test Event 1");
        });

        it("should call events.get with correct params", async () => {
            await service.getEvent(mockAuth, "work-calendar", "event1");
            const { google } = await import("googleapis");
            const calendarInstance = (google.calendar as any).mock.results[0]
                ?.value;
            expect(calendarInstance.events.get).toHaveBeenCalledWith({
                calendarId: "work-calendar",
                eventId: "event1",
            });
        });

        it("should propagate API errors (e.g. 404)", async () => {
            const { google } = await import("googleapis");
            (google.calendar as any).mockImplementationOnce(() => ({
                events: {
                    get: vi.fn().mockRejectedValue(
                        Object.assign(new Error("Not Found"), {
                            code: 404,
                        }),
                    ),
                },
            }));
            await expect(
                service.getEvent(mockAuth, "primary", "missing"),
            ).rejects.toThrow("Not Found");
        });
    });

    describe("createEvent", () => {
        const mockAuth = { credentials: {} } as any;
        const newEvent = {
            summary: "New Event",
            start: { dateTime: "2025-12-01T09:00:00Z" },
            end: { dateTime: "2025-12-01T10:00:00Z" },
        };

        it("should create an event and return it", async () => {
            const result = await service.createEvent(
                mockAuth,
                "primary",
                newEvent,
            );
            expect(result.id).toBe("new-event-id");
            expect(result.summary).toBe("New Event");
        });

        it("should call events.insert with correct params", async () => {
            await service.createEvent(mockAuth, "primary", newEvent);
            const { google } = await import("googleapis");
            const calendarInstance = (google.calendar as any).mock.results[0]
                ?.value;
            expect(calendarInstance.events.insert).toHaveBeenCalledWith({
                calendarId: "primary",
                requestBody: newEvent,
            });
        });

        it("should propagate API errors", async () => {
            const { google } = await import("googleapis");
            (google.calendar as any).mockImplementationOnce(() => ({
                events: {
                    insert: vi.fn().mockRejectedValue(new Error("API error")),
                },
            }));
            await expect(
                service.createEvent(mockAuth, "primary", newEvent),
            ).rejects.toThrow("API error");
        });
    });

    describe("updateEvent", () => {
        const mockAuth = { credentials: {} } as any;
        const updatedEvent = {
            summary: "Updated Event",
            start: { dateTime: "2025-12-01T09:00:00Z" },
            end: { dateTime: "2025-12-01T10:00:00Z" },
        };

        it("should update an event and return updated data", async () => {
            const result = await service.updateEvent(
                mockAuth,
                "primary",
                "event1",
                updatedEvent,
            );
            expect(result.id).toBe("event1");
            expect(result.summary).toBe("Updated Event");
        });

        it("should call events.patch with correct params", async () => {
            await service.updateEvent(
                mockAuth,
                "primary",
                "event1",
                updatedEvent,
            );
            const { google } = await import("googleapis");
            const calendarInstance = (google.calendar as any).mock.results[0]
                ?.value;
            expect(calendarInstance.events.patch).toHaveBeenCalledWith({
                calendarId: "primary",
                eventId: "event1",
                requestBody: updatedEvent,
            });
        });

        it("should propagate API errors", async () => {
            const { google } = await import("googleapis");
            (google.calendar as any).mockImplementationOnce(() => ({
                events: {
                    patch: vi.fn().mockRejectedValue(new Error("Not found")),
                },
            }));
            await expect(
                service.updateEvent(
                    mockAuth,
                    "primary",
                    "nonexistent",
                    updatedEvent,
                ),
            ).rejects.toThrow("Not found");
        });
    });

    describe("deleteEvent", () => {
        const mockAuth = { credentials: {} } as any;

        it("should delete an event without error", async () => {
            await expect(
                service.deleteEvent(mockAuth, "primary", "event1"),
            ).resolves.toBeUndefined();
        });

        it("should call events.delete with correct params", async () => {
            await service.deleteEvent(mockAuth, "primary", "event1");
            const { google } = await import("googleapis");
            const calendarInstance = (google.calendar as any).mock.results[0]
                ?.value;
            expect(calendarInstance.events.delete).toHaveBeenCalledWith({
                calendarId: "primary",
                eventId: "event1",
            });
        });

        it("should propagate API errors", async () => {
            const { google } = await import("googleapis");
            (google.calendar as any).mockImplementationOnce(() => ({
                events: {
                    delete: vi
                        .fn()
                        .mockRejectedValue(new Error("Delete failed")),
                },
            }));
            await expect(
                service.deleteEvent(mockAuth, "primary", "event1"),
            ).rejects.toThrow("Delete failed");
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

            // 3. Build an authenticated client, then list events
            const client = service.createAuthenticatedClient({
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
            });
            const result = await service.listEvents(client);
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
