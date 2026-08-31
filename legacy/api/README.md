# @sunrise/api

1. Add `.env` file based on `.env.example`.

2. Run the following:

    ```bash
    bun install
    bun dev # Run GraphQL codegen and start server
    ```

## Environment Setup

### 1. Set up Google OAuth

1. Go to [Google Cloud Console](https://console.cloud.google.com/)
2. Create a new project or select existing one
3. Enable the Google Calendar API
4. Create OAuth 2.0 credentials:
   - Application type: Web application
   - Authorized redirect URIs: `http://localhost:3000/auth/callback`
5. Download the credentials

### 2. Configure Environment

Create `/apps/api/.env` with your Google OAuth credentials:

```bash
GOOGLE_CLIENT_ID=your_google_client_id_here
GOOGLE_CLIENT_SECRET=your_google_client_secret_here
GOOGLE_REDIRECT_URI=http://localhost:3000/auth/callback
PORT=3000
```
