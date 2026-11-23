/**
 * What the browser specs share: whether the wasm bundle was built, and a
 * handle on the page's own core worker.
 *
 * The app has no capture UI yet, and its one list is Today, which a task with
 * no plan or deadline never reaches. So the specs do not drive the UI to
 * create a task. They wrap `Worker` before the page's scripts run, keep the
 * worker `src/wasm.ts` starts, and speak the worker's own protocol
 * (`CoreRequest` in, `WorkerMessage` out) to it. Everything under test is
 * still the shipped code: the page starts the worker, the worker loads the
 * bundle and opens the vault in OPFS, and the specs' calls queue behind the
 * page's own.
 */

import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import type { Page } from "@playwright/test";
import type { CoreCallName, CoreRequest, WorkerMessage } from "../src/wasm";
import { ROOT_FILE } from "../src/worker-core";

export { ROOT_FILE };

/** `mise run web-wasm` writes it; `vite build` copies it into `dist/`. */
const BUNDLE = fileURLToPath(
    new URL("../public/wasm/sunrise_core_wasm_bg.wasm", import.meta.url),
);

/** Why a spec is skipped, or `null` when the bundle is there to test. */
export function missingBundle(): string | null {
    return existsSync(BUNDLE)
        ? null
        : `no web core bundle at ${BUNDLE}; run \`mise run web-wasm\` (or \`mise run web-e2e\`, which builds it first)`;
}

/** The page-side record the init script keeps. */
interface Harness {
    workers: Worker[];
    /** Every non-reply message the page's worker posted: `ready` or `failed`. */
    lifecycle: WorkerMessage[];
    nextId: number;
}

declare global {
    interface Window {
        __sunriseE2E?: Harness;
    }
}

/**
 * Wrap `Worker` on every document the page loads, before the app's scripts,
 * and record what the worker says about opening the vault.
 */
export async function captureCoreWorker(page: Page): Promise<void> {
    await page.addInitScript(() => {
        const harness: Harness = { workers: [], lifecycle: [], nextId: 0 };
        window.__sunriseE2E = harness;
        const Native = window.Worker;
        window.Worker = class extends Native {
            constructor(url: string | URL, options?: WorkerOptions) {
                super(url, options);
                harness.workers.push(this);
                this.addEventListener(
                    "message",
                    (event: MessageEvent<WorkerMessage>) => {
                        if (event.data.kind !== "reply") {
                            harness.lifecycle.push(event.data);
                        }
                    },
                );
            }
        };
    });
}

/** The first thing the page's worker said: `ready`, or `failed` and why. */
export async function workerOutcome(page: Page): Promise<WorkerMessage> {
    const handle = await page.waitForFunction(
        () => window.__sunriseE2E?.lifecycle[0],
    );
    return (await handle.jsonValue()) as WorkerMessage;
}

/**
 * One core call on the page's worker: resolves to what it answers, rejects
 * with the core's error. Ids start far above the page's own counter, which
 * counts from 0, so a reply is never taken for the other's.
 */
export function coreCall(
    page: Page,
    call: CoreCallName,
    arg: CoreRequest["arg"],
): Promise<string> {
    return page.evaluate(
        ([call, arg]) =>
            new Promise<string>((resolve, reject) => {
                const harness = window.__sunriseE2E;
                const worker = harness?.workers[0];
                if (!harness || !worker) {
                    reject(new Error("the page started no worker"));
                    return;
                }
                const id = 1_000_000 + harness.nextId++;
                const onMessage = (event: MessageEvent<WorkerMessage>) => {
                    const msg = event.data;
                    if (msg.kind !== "reply" || msg.id !== id) {
                        return;
                    }
                    worker.removeEventListener("message", onMessage);
                    if (msg.ok) {
                        resolve(msg.json);
                    } else {
                        reject(new Error(msg.error));
                    }
                };
                worker.addEventListener("message", onMessage);
                worker.postMessage({ id, call, arg } satisfies CoreRequest);
            }),
        [call, arg] as const,
    );
}

/** The vault-root file's bytes in this origin's OPFS, or `null` if absent. */
export function readRootFile(page: Page): Promise<number[] | null> {
    return page.evaluate(async (name) => {
        const dir = await navigator.storage.getDirectory();
        try {
            const handle = await dir.getFileHandle(name);
            const file = await handle.getFile();
            return [...new Uint8Array(await file.arrayBuffer())];
        } catch {
            return null;
        }
    }, ROOT_FILE);
}

/** Replace the vault-root file in this origin's OPFS with `bytes`. */
export function writeRootFile(page: Page, bytes: number[]): Promise<void> {
    return page.evaluate(
        async ([name, bytes]) => {
            const dir = await navigator.storage.getDirectory();
            const handle = await dir.getFileHandle(name, { create: true });
            const writable = await handle.createWritable();
            await writable.write(new Uint8Array(bytes));
            await writable.close();
        },
        [ROOT_FILE, bytes] as const,
    );
}
