/**
 * The page, `core.worker.ts`, the `sunrise-core-wasm` bundle and OPFS, run
 * together in a browser (#1257). `src/worker-core.test.ts` and
 * `src/wasm.test.ts` cover each side against fakes; these cover the seam.
 */

import { expect, test } from "@playwright/test";
import type { Task } from "../src/wasm";
import { ROOT_BYTES } from "../src/worker-core";
import {
    captureCoreWorker,
    coreCall,
    missingBundle,
    ROOT_FILE,
    readRootFile,
    workerOutcome,
    writeRootFile,
} from "./harness";

const skipReason = missingBundle();
test.skip(skipReason !== null, skipReason ?? "");

test.beforeEach(async ({ page }) => {
    await captureCoreWorker(page);
});

test("opens a vault in OPFS, creates a task, and reads it back after a reload", async ({
    page,
}) => {
    await page.goto("/");
    expect(await workerOutcome(page)).toEqual({ kind: "ready" });
    // A fresh vault: nothing on Today, nothing in the Inbox.
    expect(JSON.parse(await coreCall(page, "today", Date.now()))).toEqual([]);
    expect(JSON.parse(await coreCall(page, "inbox", null))).toEqual([]);

    const root = await readRootFile(page);
    expect(root).toHaveLength(ROOT_BYTES);

    const title = `e2e task ${Date.now()}`;
    const id = await coreCall(page, "createTask", title);
    expect(id).not.toBe("");
    const inbox = JSON.parse(await coreCall(page, "inbox", null)) as Task[];
    expect(inbox).toContainEqual(
        expect.objectContaining({ id, title, state: "todo" }),
    );

    // A new document and a new worker: nothing survives but OPFS.
    await page.reload();
    expect(await workerOutcome(page)).toEqual({ kind: "ready" });
    expect(await readRootFile(page)).toEqual(root);
    const reread = JSON.parse(await coreCall(page, "inbox", null)) as Task[];
    expect(reread).toContainEqual(
        expect.objectContaining({ id, title, state: "todo" }),
    );
    // And not by way of the stub, which would have kept it here.
    expect(
        await page.evaluate(() =>
            localStorage.getItem("sunrise-web-stub-tasks-v1"),
        ),
    ).toBeNull();
});

test("refuses a vault-root file of the wrong length and leaves it in place", async ({
    page,
}) => {
    // Any same-origin document that does not start the core will do; the
    // bundle's own loader is one that is always there when this runs.
    await page.goto("/wasm/sunrise_core_wasm.js");
    await writeRootFile(page, [1, 2, 3, 4, 5]);

    const warnings: string[] = [];
    page.on("console", (msg) => {
        if (msg.type() === "warning") {
            warnings.push(msg.text());
        }
    });
    await page.goto("/");
    const reason = `${ROOT_FILE} is 5 bytes, expected ${ROOT_BYTES}`;
    expect(await workerOutcome(page)).toEqual({
        kind: "failed",
        reason: `Error: ${reason}`,
    });
    // The page falls back to the stub and still renders.
    await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
    await expect
        .poll(() => warnings)
        .toContainEqual(expect.stringContaining(reason));
    // Refused, not replaced: a new root would not open the old vault.
    expect(await readRootFile(page)).toEqual([1, 2, 3, 4, 5]);
});
