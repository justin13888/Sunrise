---
status: accepted
---

# Web Client

A React (TS) PWA running the Sunrise core compiled to WebAssembly. Offline-capable; installable; runs without a browser session-by-session.

> **v1 status — WASM core deferred; the MSRV half of the blocker is cleared.**
> The architecture below is the *target*. A gated spike (see
> [ADR 0012](../11-adr/0012-web-wasm-deferred.md)) found that the only `rusqlite`
> line integrating `sqlite-wasm-rs` (`0.40`, via `ffi-sqlite-wasm-rs`) requires
> Rust ≥ 1.91 (`libsqlite3-sys 0.38.1`'s `cfg_select!`), which the then-pinned
> MSRV of 1.88 could not compile — it failed on the **native**
> `bundled-sqlcipher` build, before wasm was even attempted, which is the
> spike's hard gate.
>
> [ADR-0026](../11-adr/0026-msrv-bump.md) has since moved the pin to **1.91.1**,
> firing ADR-0012's own revisit trigger. That removes the reason the spike
> stopped; it does not do the work the spike sized. What remains is the
> `rusqlite` 0.31 → 0.40 swap across `sunrise-storage` and `sunrise-core` —
> nine minor versions, 100+ call sites, and a wholesale change of the native
> SQLite/SQLCipher stack — tracked as
> [#52](https://github.com/justin13888/Sunrise/issues/52), still subject to the
> same hard gate. **Until it lands, v1 web ships the `localStorage` stub**
> behind `apps/web/src/wasm.ts`'s `loadCore()` seam: in-tab, **unencrypted**, no
> OPFS, no real `sunrise-core`. It exists so the PWA shell renders for UI
> development.
>
> Note also that even once the WASM path lands, `sqlite-wasm-rs` yields
> **plaintext SQLite in OPFS** (no SQLCipher key pragmas on wasm) — a
> spec-accepted v1 web gap — and multi-tab exclusivity moves to the JS layer's
> `navigator.locks`.

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

- *Target state:* `wa-sqlite` (WASM SQLite with FTS5), async access only, via Web Worker. Nothing in the tree builds it — the WASM core is deferred ([ADR-0012](../11-adr/0012-web-wasm-deferred.md)) and no `wa-sqlite` dependency is declared anywhere.
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

*Target state.* The web client would connect to whatever sync server URL is configured, with the operator hosting the static assets or pointing the user at a hosted app configured against their server. A runtime server-URL setting is the web client's own deliverable and does not exist; there is no "settings handshake" protocol anywhere in the tree. Note also that v1 ships one server shape, self-host ([ADR-0027](../11-adr/0027-v1-self-host-first.md)), so there is no managed alternative to choose between.

## What about the browser as a *capture* tool?

A browser extension (separate codebase, smaller scope) gives us:

- Global keyboard shortcut.
- "Send page to Sunrise" action.
- Selection-to-task command.

Tracked as v1.x; not in v1 scope.
