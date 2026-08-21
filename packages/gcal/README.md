# @sunrise/gcal

Pure library wrapping the Google Calendar v3 API (via `googleapis` + `google-auth-library`). No server or CLI code is exported — the API server (`@sunrise/api`) consumes it for OAuth and calendar operations.

## API

`GoogleCalendarService` (constructed with client ID, client secret, and redirect URI):

- `getAuthUrl(state?)` — build the Google OAuth consent URL
- `getTokensFromCode(code)` — exchange an authorization code for tokens
- `createAuthenticatedClient(tokens)` — build an `OAuth2Client` from stored tokens
- `getUserInfo(auth)` — fetch the authenticated user's email/name/picture (throws if Google returns no email)
- `listCalendars(auth)` — list the user's calendars
- `listEvents(auth, calendarId?, maxResults?, pageToken?, timeMin?, timeMax?, orderBy?)` — list events in a time range (RFC3339 strings)
- `getEvent(auth, calendarId, eventId)` — fetch a single event
- `createEvent(auth, calendarId, event)` — create an event
- `updateEvent(auth, calendarId, eventId, patch)` — update an event (PATCH semantics: only provided fields change)
- `deleteEvent(auth, calendarId, eventId)` — delete an event

Type re-exports: `OAuth2Client`, `calendar_v3`.

## Demo

`src/demo.ts` is a standalone demo/CLI script for exercising the service manually. It is not part of the package's exports.
