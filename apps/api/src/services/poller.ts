import { GoogleCalendarService } from "@sunrise/gcal";
import { tokenStore } from "./tokenStore";

export class PollingService {
    private isRunning = false;
    private timer: ReturnType<typeof setInterval> | null = null;
    private gcalService: GoogleCalendarService;

    constructor(
        clientId: string,
        clientSecret: string,
        redirectUri?: string,
    ) {
        this.gcalService = new GoogleCalendarService(
            clientId,
            clientSecret,
            redirectUri,
        );
    }

    start() {
        if (this.isRunning) return;
        this.isRunning = true;
        console.log("⏰ Starting calendar polling service...");

        // Poll immediately
        this.pollAll();

        // Then poll every minute
        this.timer = setInterval(() => {
            this.pollAll();
        }, 60 * 1000);
    }

    stop() {
        if (this.timer) {
            clearInterval(this.timer);
            this.timer = null;
        }
        this.isRunning = false;
        console.log("🛑 Stopping calendar polling service...");
    }

    private async pollAll() {
        try {
            const userIds = await tokenStore.getAllUserIds();
            console.log(`🔎 Polling calendars for ${userIds.length} users...`);

            for (const userId of userIds) {
                await this.pollUser(userId);
            }
        } catch (error) {
            console.error("❌ Polling loop error:", error);
        }
    }

    private async pollUser(userId: string) {
        try {
            const tokens = await tokenStore.getTokens(userId);
            if (!tokens) return;

            // Create Authenticated Client
            const auth = this.gcalService.createAuthenticatedClient({
                access_token: tokens.accessToken,
                refresh_token: tokens.refreshToken,
                expiry_date: tokens.expiresAt.getTime(),
            });

            // List Events
            const result = await this.gcalService.listEvents(auth);

            console.log(
                `📅 Poll ${userId}: Found ${result.items.length} events`,
            );

            // Check for token updates (refresh)
            if (result.credentials) {
                const newCreds = result.credentials;
                // Check if access token or expiry changed
                if (
                    newCreds.access_token !== tokens.accessToken ||
                    (newCreds.expiry_date &&
                        newCreds.expiry_date !== tokens.expiresAt.getTime())
                ) {
                    console.log(`🔄 Refreshing tokens for user ${userId}`);
                    await tokenStore.storeTokens(userId, {
                        ...tokens,
                        accessToken:
                            newCreds.access_token || tokens.accessToken,
                        refreshToken:
                            newCreds.refresh_token || tokens.refreshToken,
                        expiresAt: new Date(
                            newCreds.expiry_date || tokens.expiresAt.getTime(),
                        ),
                    });
                }
            }
        } catch (error) {
            console.error(`❌ Error polling for user ${userId}:`, error);
        }
    }
}

