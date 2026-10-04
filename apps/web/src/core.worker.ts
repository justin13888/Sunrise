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

import type { CoreRequest, WorkerMessage } from "./wasm";

declare const self: DedicatedWorkerGlobalScope;

/**
 * Where `mise run web-wasm` writes the `wasm-bindgen --target web` output.
 * Served from `apps/web/public/`, untracked, and fetched at run time rather
 * than imported, so a build without it still bundles and falls back.
 */
const BUNDLE_URL = "/wasm/sunrise_core_wasm.js";

/** One holder per origin; see ADR-0055 §4. */
const LOCK_NAME = "sunrise-vault";

/** The OPFS file holding the 32-byte vault root, beside the pool VFS. */
const ROOT_FILE = "sunrise-vault-root";
const ROOT_BYTES = 32;

/** The vault's path inside the SAHPool VFS. */
const VAULT_DIR = "/sunrise";

/** `<semver>+<platform>`, as `CoreConfig::app` wants it. */
const APP = "0.1.0+web";

interface WasmCore {
    submitJson(json: string): Promise<string>;
    queryJson(json: string): Promise<string>;
    closeVault(): Promise<void>;
}

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

/**
 * Read the vault root, minting it on first run.
 *
 * Stored in plaintext beside the plaintext vault (ADR-0055 §4): on the web it
 * keys nothing at rest. A file of any other length is refused rather than
 * replaced, since a new root would not open the vault the old one made.
 */
async function loadOrCreateRoot(): Promise<Uint8Array> {
    const dir = await navigator.storage.getDirectory();
    const handle = await dir.getFileHandle(ROOT_FILE, { create: true });
    const file = await handle.getFile();
    if (file.size === ROOT_BYTES) {
        return new Uint8Array(await file.arrayBuffer());
    }
    if (file.size !== 0) {
        throw new Error(
            `${ROOT_FILE} is ${file.size} bytes, expected ${ROOT_BYTES}`,
        );
    }
    const root = crypto.getRandomValues(new Uint8Array(ROOT_BYTES));
    const access = await handle.createSyncAccessHandle();
    try {
        access.write(root, { at: 0 });
        access.flush();
    } finally {
        access.close();
    }
    return root;
}

async function open(): Promise<WasmCore> {
    // The bundle before the lock: a build without it should fall back to the
    // stub at once, not after waiting out another tab.
    const bundle = (await import(
        /* @vite-ignore */ new URL(BUNDLE_URL, self.location.origin).href
    )) as Bundle;
    await bundle.default();
    await holdVaultLock();
    const root = await loadOrCreateRoot();
    const timezone = Intl.DateTimeFormat().resolvedOptions().timeZone;
    return bundle.openVault(VAULT_DIR, root, APP, timezone);
}

/** Answer requests in arrival order, so no two core calls overlap. */
function serve(core: WasmCore) {
    let queue: Promise<void> = Promise.resolve();
    self.addEventListener("message", (event: MessageEvent<CoreRequest>) => {
        const { id, kind, json } = event.data;
        queue = queue.then(async () => {
            try {
                const out =
                    kind === "submit"
                        ? await core.submitJson(json)
                        : await core.queryJson(json);
                post({ kind: "reply", id, ok: true, json: out });
            } catch (e) {
                post({ kind: "reply", id, ok: false, error: String(e) });
            }
        });
    });
}

open().then(
    (core) => {
        serve(core);
        post({ kind: "ready" });
    },
    (e) => post({ kind: "failed", reason: String(e) }),
);
