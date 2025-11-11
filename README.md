# Sunrise

Sunrise is an open-source daily routine app that helps you focus on what matters! It aims to be accessible, available on all major desktop and mobile platforms, open source, and built with performant technologies.

<!-- TODO: Add screenshot and demo link -->

## Why Sunrise?

It's simple. Everybody has there own ways to stay organized but we give you simple, well-thought tools, for free! Self-host to maintain control of your data. Contribute to the open-source codebase to add features. Give feedback to help everyone else.

## Features

- **Google Calendar Integration**: Connect your Google Calendar to view and manage events
- **OAuth Authentication**: Secure authentication flow with Google OAuth 2.0
- **Real-time Calendar Sync**: View your calendars and upcoming events
- **GraphQL API**: Type-safe API with code generation
- **Desktop App**: Built with Tauri for native performance

## Development

### Technologies

Sunrise is built with the following technologies:

- **Frontend**: React, TanStack Router, Apollo Client, shadcn/ui
- **Backend**: Hono, GraphQL Yoga, Bun
- **Desktop**: Tauri (Rust)
- **Database**: File-based token storage (temporary)
- **Testing**: Vitest with comprehensive unit tests
- **Build Tools**: Vite, TypeScript, GraphQL Code Generator

### Project Structure

```
apps/
  api/        - GraphQL API server with Google Calendar integration
  app/        - React frontend with Tauri desktop wrapper
packages/
  gcal/       - Google Calendar service library
  models/     - Shared TypeScript models
```

### Getting Started

1. **Install dependencies**:

   ```bash
   bun install
   ```

2. **Set up the API**:
   - See [apps/api/README.md](apps/api/README.md) for detailed setup
   - Configure Google OAuth credentials
   - Start the API server:

     ```bash
     cd apps/api
     bun dev
     ```

3. **Start the frontend**:

   ```bash
   cd apps/app
   bun dev
   ```

4. **Run tests**:

   ```bash
   bun test          # Run all tests
   bun test:ui       # Run tests with UI
   bun test:coverage # Generate coverage report
   ```

### Testing

The project includes comprehensive unit tests for:

- **TokenStore**: File-based token management with expiration handling
- **Auth utilities**: Authentication helpers and guards
- **Custom scalars**: DateTime and URL GraphQL scalars
- **GoogleCalendarService**: OAuth flow and Calendar API integration

All tests use Vitest with 119 tests covering critical functionality.

## Architecture

### Authentication Flow

1. User clicks "Sign in with Google"
2. Opens OAuth popup with Google consent screen
3. User grants permissions
4. Callback returns authorization code
5. Frontend exchanges code for access/refresh tokens
6. Tokens stored securely for API requests

### API Design

- **GraphQL Schema**: Type-safe queries and mutations
- **Code Generation**: Auto-generated TypeScript types
- **Custom Scalars**: DateTime and URL with validation
- **Error Handling**: Structured errors with proper codes

## License

Sunrise is licensed under the [AGPL-3.0 License](LICENSE).
