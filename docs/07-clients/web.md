---
status: accepted
---

# Web Client

A React (TS) PWA running the Sunrise core compiled to WebAssembly. Offline-capable; installable; runs without a browser session-by-session.

> **Status — the WASM core runs, locally and unencrypted
> ([ADR-0055](../11-adr/0055-web-wasm-core.md)); not deployed
> ([#11](https://github.com/justin13888/Sunrise/issues/11)).**
> `crates/sunrise-core-wasm` compiles `sunrise-core` to `wasm32-unknown-unknown`
> and exposes it as JSON in, JSON out. `apps/web/src/core.worker.ts` runs it in a
> dedicated worker over the OPFS `SyncAccessHandle` pool VFS, and
> `apps/web/src/wasm.ts`'s `loadCore()` uses it where the browser has workers,
> OPFS and `navigator.locks`, falling back to the `localStorage` stub elsewhere
> or when no bundle was built (`mise run web-wasm`).
>
> What the architecture below describes and the build does not yet do:
>
> - **Encryption at rest.** The vault is **plaintext SQLite in OPFS**, and its
>   root is stored beside it; ADR-0055 §4 states the gap and what the product
>   tells a user about it. Encryption waits on the passphrase unlock below.
> - **Sync.** The worker opens a local vault only; the SSE transport is
>   native-only.
> - **Attachments.** The blob store is `std::fs`, which the wasm target lacks;
>   adding one fails with an error.
> - **Read-only tabs.** One tab holds the vault; another waits for it rather
>   than showing a read-only view.
> - **Unlock.** No passphrase, pairing or recovery flow: every web vault is its
>   own account, and clearing the origin's storage loses it.

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
                ▼ via SSE (down) + typed POST (up), ADR-0023
            Sync server
```

## Web-specific concerns

### Storage

- **OPFS (Origin Private File System)** for the vault DB and blob chunks. Reliable, large quota, not user-deletable from "Clear browsing data" in most browsers (per-origin behavior varies).
- **IndexedDB** as a fallback for browsers without OPFS.
- **`localStorage` is never used** for anything sensitive (session cache only, < 5 KB).

#### OPFS quota handling

- Storage usage = sum of OPFS file sizes under `sunrise/`. Computed via `navigator.storage.estimate()` (browser-reported) and a recursive `getDirectoryHandle().values()` walk for our own accounting.
- Warn at 80% of `quota`; block writes at 95%. **The code is unallocated:** `STORAGE_QUOTA_EXCEEDED` was removed from the registry with ADR-0027 and its id is burned, so a browser-local storage cap needs a new one before this can be built.

### SQLite in the browser

- `sqlite-wasm-rs` (SQLite compiled for `wasm32-unknown-unknown`, FTS5 included), bound by `rusqlite`'s `ffi-sqlite-wasm-rs`, so the core's storage layer runs unchanged; persisted through `sqlite-wasm-vfs`'s OPFS `SyncAccessHandle` pool, which needs a dedicated worker ([ADR-0055](../11-adr/0055-web-wasm-core.md)). Synchronous inside the worker; the page reaches it by message only. Not `wa-sqlite`: a second SQLite would put the storage layer behind a different API on one platform.
- We are not using the browser's built-in WebSQL or any other sync API.

### Service Worker

- Caches the app shell for offline launch.
- Receives Web Push and posts a message to the active tab (or wakes a hidden tab to drain sync).
- Coordinates background fetch where supported (Chrome) for initial sync resumption.

### Web Crypto API

We use Web Crypto for **TLS-relevant operations and basic primitives** but **not** for our crypto core — that's in the WASM bundle (RustCrypto + ChaCha + BLAKE3) for consistency with native clients.

Argon2id runs in WASM. Tradeoff: slower than native, but consistent and side-channel-aware.

#### Argon2id WASM perf budget

- Target: ≤ 2 s on a 2022-class laptop, ≤ 5 s on a 2020-class phone browser.
- Calibration on first unlock; if > 5 s, surface "Recovery is slow on this browser" notice.

### Multi-tab handling

Two open tabs sharing a vault would otherwise corrupt SQLite. Solution:

- **Web Lock API** (`navigator.locks`) acquires an exclusive lock for the writer tab. *Built:* the core worker takes `sunrise-vault` before it opens the vault and holds it for its lifetime.
- Other tabs become read-only views, listening via `BroadcastChannel` for change notifications. *Not built:* another tab's worker waits on the lock instead.
- The writer tab can transfer the lock if it closes. *Built:* a waiting tab opens the vault the moment the holder closes.

### Limitations vs native

| Limitation | Mitigation |
|---|---|
| No global hotkey (browser-scoped only) | Browser extension companion (separate spec; not built) |
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

#### Deep-link service-worker timing

Deep links arriving before the Service Worker is ready are queued in `localStorage.deeplink_queue` (max 8 entries, FIFO) and replayed on the first SW-activated event. Entries older than 5 minutes are discarded.

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

*Target state.* The web client would connect to whatever sync server URL is configured, with the operator hosting the static assets or pointing the user at a hosted app configured against their server. A runtime server-URL setting is the web client's own deliverable and does not exist; there is no "settings handshake" protocol anywhere in the tree. Note also that there is one server shape, self-host ([ADR-0027](../11-adr/0027-v1-self-host-first.md)), so there is no managed alternative to choose between.

## What about the browser as a *capture* tool?

A browser extension (separate codebase, smaller scope) gives us:

- Global keyboard shortcut.
- "Send page to Sunrise" action.
- Selection-to-task command.

Not built.
