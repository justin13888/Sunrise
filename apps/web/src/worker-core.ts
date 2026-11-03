/**
 * The web core worker's logic, with its handles injected (ADR-0055).
 *
 * `core.worker.ts` opens the vault the moment it is imported, so nothing in it
 * can be tested. What it decides lives here instead, with the OPFS directory,
 * the reply channel and the message source passed in: reading or minting the
 * vault root, and answering requests one at a time.
 */

import type { CoreRequest, WorkerMessage } from "./wasm";

/** The OPFS file holding the 32-byte vault root, beside the pool VFS. */
export const ROOT_FILE = "sunrise-vault-root";
export const ROOT_BYTES = 32;

/** The bundle's typed calls (`crates/sunrise-core-wasm/src/calls.rs`). */
export interface WasmCore {
    createTask(title: string): Promise<string>;
    completeTask(id: string): Promise<void>;
    today(nowMs: number): Promise<string>;
    inbox(): Promise<string>;
    closeVault(): Promise<void>;
}

/**
 * The part of an OPFS directory `loadOrCreateRoot` uses. A
 * `FileSystemDirectoryHandle` is one.
 */
export interface RootDirectory {
    getFileHandle(
        name: string,
        options: { create: boolean },
    ): Promise<RootFileHandle>;
}

/** The part of a `FileSystemFileHandle` `loadOrCreateRoot` uses. */
export interface RootFileHandle {
    getFile(): Promise<{ size: number; arrayBuffer(): Promise<ArrayBuffer> }>;
    createSyncAccessHandle(): Promise<{
        write(buffer: Uint8Array, options: { at: number }): number;
        flush(): void;
        close(): void;
    }>;
}

/** One request, as the call it names. */
async function dispatch(core: WasmCore, req: CoreRequest): Promise<string> {
    switch (req.call) {
        case "createTask":
            return core.createTask(String(req.arg));
        case "completeTask":
            await core.completeTask(String(req.arg));
            return "";
        case "today":
            return core.today(Number(req.arg));
        case "inbox":
            return core.inbox();
    }
}

/**
 * Read the vault root from `dir`, minting it on first run.
 *
 * Stored in plaintext beside the plaintext vault (ADR-0055 §4): on the web it
 * keys nothing at rest. A file of any other length is refused rather than
 * replaced, since a new root would not open the vault the old one made.
 */
export async function loadOrCreateRoot(
    dir: RootDirectory,
): Promise<Uint8Array> {
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

/**
 * Answer the requests `addListener` delivers through `post`, in arrival
 * order, so no two core calls overlap. A call that throws is answered with
 * `ok: false` and the queue goes on.
 */
export function serve(
    core: WasmCore,
    post: (msg: WorkerMessage) => void,
    addListener: (listener: (event: { data: CoreRequest }) => void) => void,
): void {
    let queue: Promise<void> = Promise.resolve();
    addListener((event) => {
        const request = event.data;
        const { id } = request;
        queue = queue.then(async () => {
            try {
                const out = await dispatch(core, request);
                post({ kind: "reply", id, ok: true, json: out });
            } catch (e) {
                post({ kind: "reply", id, ok: false, error: String(e) });
            }
        });
    });
}
