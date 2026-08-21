# @sunrise/app

Sunrise client: React + TanStack Router + Apollo Client, packaged as a desktop app with Tauri v2 (also runs in the browser).

Mobile (Android/iOS) targets are **not yet configured** for this project.

## Prerequisites

- [Bun](https://bun.sh/)
- For the **desktop app** (Tauri v2): a Rust toolchain ([rustup](https://rustup.rs/)) plus system packages.

  Fedora:

  ```bash
  sudo dnf install webkit2gtk4.1-devel gtk3-devel libappindicator-gtk3-devel librsvg2-devel openssl-devel
  sudo dnf group install "c-development"
  ```

  Debian/Ubuntu:

  ```bash
  sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev libssl-dev build-essential curl wget file
  ```

  See the [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/) for other platforms.

## Development

```bash
bun install
bun dev              # Web at http://localhost:1420
bun run tauri dev    # Desktop app (starts the web dev server automatically)
```

Run the API server first (see [../api/README.md](../api/README.md)) — the app talks to it at `http://localhost:3000` by default.

### Environment

Copy `.env.example` to `.env` if you need to override defaults:

| Variable | Default | Description |
| --- | --- | --- |
| `VITE_API_URL` | `http://localhost:3000` | API server base URL (HTTP). |
| `VITE_WS_URL` | `VITE_API_URL` with `http` → `ws` | WebSocket base URL for subscriptions. |

### GraphQL codegen

`bun dev` and `bun run build` run GraphQL codegen automatically before starting. Codegen is **offline** — it reads the committed schema at `../api/src/schema.graphql` (no running server needed). Generated code lands in `src/generated/`, which stays gitignored.

### Sign-in flows

- **Web**: Google sign-in opens a popup; the API's callback page posts the authorization code back via `postMessage` (origin-checked). If the popup is blocked, paste the code shown on the callback page instead.
- **Desktop (Tauri)**: sign-in opens your system browser; copy the authorization code shown on the callback page and paste it into the app.

## Build

```bash
bun run build        # Web bundle (codegen + tsc + vite build)
bun run tauri build  # Desktop bundle
```

### Deploying against a non-localhost API

The Tauri content security policy (`src-tauri/tauri.conf.json`) only allows `connect-src` to `http://localhost:3000` and `ws://localhost:3000`. If you point the desktop app at a different API origin via `VITE_API_URL`/`VITE_WS_URL`, add that origin to the CSP as well.
