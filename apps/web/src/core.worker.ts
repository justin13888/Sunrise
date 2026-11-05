/**
 * The web core's dedicated worker (ADR-0055).
 *
 * Owns the vault for this tab: takes the `sunrise-vault` lock, loads the
 * `sunrise-core-wasm` bundle, mints or reads the vault root, opens the vault
 * in OPFS, and then answers `CoreRequest`s from the page one at a time. It has
 * to be a dedicated worker: the SAHPool VFS needs
 * `FileSystemSyncAccessHandle`, which exists nowhere else.
 *
 * Posts `ready` once the vault is open, or `failed` with a reason, after
 * which the page uses the stub and terminates this worker.
 */

import type { WorkerMessage } from "./wasm";
import { loadOrCreateRoot, serve, type WasmCore } from "./worker-core";

declare const self: DedicatedWorkerGlobalScope;

/**
 * Where `mise run web-wasm` writes the `wasm-bindgen --target web` output.
 * Served from `apps/web/public/`, untracked, and fetched at run time rather
 * than imported, so a build without it still bundles and falls back.
 */
const BUNDLE_URL = "/wasm/sunrise_core_wasm.js";

/** One holder per origin; see ADR-0055 §4. */
const LOCK_NAME = "sunrise-vault";

/** The vault's path inside the SAHPool VFS. */
const VAULT_DIR = "/sunrise";

/** `<semver>+<platform>`, as `CoreConfig::app` wants it. */
const APP = "0.1.0+web";

interface Bundle {
    default(): Promise<unknown>;
    openVault(
        vaultDir: string,
        root: Uint8Array,
        app: string,
        timezone: string,
    ): Promise<WasmCore>;
}

function post(msg: WorkerMessage) {
    self.postMessage(msg);
}

/**
 * Resolves once this worker holds the vault lock, and keeps holding it until
 * the worker dies (the tab closes). A second tab waits here.
 */
function holdVaultLock(): Promise<void> {
    return new Promise((acquired) => {
        void navigator.locks.request(LOCK_NAME, { mode: "exclusive" }, () => {
            acquired();
            return new Promise<never>(() => {});
        });
    });
}

async function open(): Promise<WasmCore> {
    // The bundle before the lock: a build without it should fall back to the
    // stub at once, not after waiting out another tab.
    const bundle = (await import(
        /* @vite-ignore */ new URL(BUNDLE_URL, self.location.origin).href
    )) as Bundle;
    await bundle.default();
    await holdVaultLock();
    const root = await loadOrCreateRoot(await navigator.storage.getDirectory());
    const timezone = Intl.DateTimeFormat().resolvedOptions().timeZone;
    return bundle.openVault(VAULT_DIR, root, APP, timezone);
}

open().then(
    (core) => {
        serve(core, post, (listener) =>
            self.addEventListener("message", listener),
        );
        post({ kind: "ready" });
    },
    (e) => post({ kind: "failed", reason: String(e) }),
);
