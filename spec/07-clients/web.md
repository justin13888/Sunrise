---
status: draft
---

# Web Client

A React (TS) PWA running the Sunrise core compiled to WebAssembly. Offline-capable; installable; runs without a browser session-by-session.

## Targets

- Latest Chrome, Edge, Safari, Firefox; one major version back supported (best-effort).
- Mobile Safari and Mobile Chrome are first-class targets (the web app is a real fallback when the user can't install a native app).

## Architecture

```
React UI ──▶ Web Worker (sunrise-core WASM)
                │
                ▼ via OPFS
         vault.db (sqlite-wasm) + blob chunks
                │
                ▼ via WebSocket
            Sync server
```

## Web-specific concerns

### Storage

- **OPFS (Origin Private File System)** for the vault DB and blob chunks. Reliable, large quota, not user-deletable from "Clear browsing data" in most browsers (per-origin behavior varies).
- **IndexedDB** as a fallback for browsers without OPFS.
- **`localStorage` is never used** for anything sensitive (session cache only, < 5 KB).

### SQLite in the browser

- `wa-sqlite` (WASM SQLite with FTS5).
- Async access only, via Web Worker.
- We are not using the browser's built-in WebSQL or any other sync API.

### Service Worker

- Caches the app shell for offline launch.
- Receives Web Push and posts a message to the active tab (or wakes a hidden tab to drain sync).
- Coordinates background fetch where supported (Chrome) for initial sync resumption.

### Web Crypto API

We use Web Crypto for **TLS-relevant operations and basic primitives** but **not** for our crypto core — that's in the WASM bundle (RustCrypto + ChaCha + BLAKE3) for consistency with native clients.

Argon2id runs in WASM. Tradeoff: slower than native, but consistent and side-channel-aware.

### Multi-tab handling

Two open tabs sharing a vault would otherwise corrupt SQLite. Solution:

- **Web Lock API** (`navigator.locks`) acquires an exclusive lock for the writer tab.
- Other tabs become read-only views, listening via `BroadcastChannel` for change notifications.
- The writer tab can transfer the lock if it closes.

### Limitations vs native

| Limitation | Mitigation |
|---|---|
| No global hotkey (browser-scoped only) | Browser extension companion (separate spec; v1.x) |
| No background sync without Service Worker tricks; no guaranteed background time | Periodic Background Sync API where available; otherwise sync on tab focus |
| Reduced clipboard / drag-and-drop privileges | Use modern Clipboard API; permissions prompt as needed |
| Push only via Web Push (VAPID); no APNs/FCM directly | Use Web Push; iOS Safari supports it (16.4+) |
| OPFS has different quota policies per browser | Show storage usage in settings; warn at 80% |
| Private browsing: no persistent storage | Detect; warn user that data won't persist; fall back to in-memory only |

### Installable PWA

- Manifest with name, icons, themes.
- Standalone display mode.
- "Add to Home Screen" first-class.
- App-launch routing handled by the Service Worker.

### URL handling

- App routes use the History API.
- Deep links: `app.sunrise.example/capture?text=…` works in browser and as PWA.
- `share_target` declared in the manifest so the user can share to the PWA from other apps.

### Authentication unlock

- No OS keystore equivalent. The web client stores the unlock material as:
  - **OPFS-stored, passphrase-wrapped** (Argon2id) by default.
  - A user-set "remember on this device" toggle stores a session token in the Service Worker memory + `IndexedDB` with a derived key; locked after `n` minutes idle (configurable).
- Public computers: ephemeral mode — passphrase required on every visit, no persistence.

### Performance budgets

- First paint: ≤2s on a 4G connection, cold cache.
- Interactive: ≤3s.
- Time to "Today rendered": ≤4s on cold cache; ≤500ms on warm cache.

### Self-host vs managed cloud

The web client connects to whatever sync server URL is configured. For self-host, the operator hosts the static assets too (or points the user at the app at `app.sunrise.example` configured to talk to their server — supported via a settings handshake).

## What about the browser as a *capture* tool?

A browser extension (separate codebase, smaller scope) gives us:

- Global keyboard shortcut.
- "Send page to Sunrise" action.
- Selection-to-task command.

Tracked as v1.x; not in v1 scope.
