import { describe, expect, it, vi } from "vitest";
import type { CoreRequest, WorkerMessage } from "./wasm";
import {
    loadOrCreateRoot,
    ROOT_BYTES,
    ROOT_FILE,
    type RootDirectory,
    serve,
    type WasmCore,
} from "./worker-core";

/** An OPFS directory holding one root file with `stored` in it. */
function fakeDir(stored: Uint8Array) {
    let bytes = stored;
    const writes: { buffer: Uint8Array; at: number }[] = [];
    const calls: string[] = [];
    const dir: RootDirectory = {
        async getFileHandle(name, options) {
            calls.push(`open ${name} create=${options.create}`);
            return {
                async getFile() {
                    return {
                        size: bytes.length,
                        arrayBuffer: async () =>
                            bytes.slice().buffer as ArrayBuffer,
                    };
                },
                async createSyncAccessHandle() {
                    return {
                        write(buffer, { at }) {
                            writes.push({ buffer: buffer.slice(), at });
                            bytes = buffer.slice();
                            return buffer.length;
                        },
                        flush: () => calls.push("flush"),
                        close: () => calls.push("close"),
                    };
                },
            };
        },
    };
    return { dir, writes, calls, stored: () => bytes };
}

describe("loadOrCreateRoot", () => {
    it("reads a root of the right length as it is", async () => {
        const root = Uint8Array.from({ length: ROOT_BYTES }, (_, i) => i);
        const fake = fakeDir(root);
        expect(await loadOrCreateRoot(fake.dir)).toEqual(root);
        expect(fake.writes).toEqual([]);
    });

    it.each([
        1,
        ROOT_BYTES - 1,
        ROOT_BYTES + 1,
    ])("refuses a %i-byte root and leaves it in place", async (size) => {
        const stored = new Uint8Array(size).fill(7);
        const fake = fakeDir(stored);
        await expect(loadOrCreateRoot(fake.dir)).rejects.toThrow(
            `${ROOT_FILE} is ${size} bytes, expected ${ROOT_BYTES}`,
        );
        expect(fake.writes).toEqual([]);
        expect(fake.stored()).toEqual(stored);
    });

    it("mints a root on first run and writes it whole", async () => {
        const fake = fakeDir(new Uint8Array(0));
        const root = await loadOrCreateRoot(fake.dir);
        expect(root).toHaveLength(ROOT_BYTES);
        expect(fake.writes).toEqual([{ buffer: root, at: 0 }]);
        expect(fake.calls).toEqual([
            `open ${ROOT_FILE} create=true`,
            "flush",
            "close",
        ]);
        // The next run reads back the root this one minted.
        expect(await loadOrCreateRoot(fake.dir)).toEqual(root);
    });
});

/** A promise and the function that settles it. */
function deferred<T>() {
    let resolve!: (value: T) => void;
    const promise = new Promise<T>((r) => {
        resolve = r;
    });
    return { promise, resolve };
}

function fakeCore(overrides: Partial<WasmCore>): WasmCore {
    const unexpected = () => Promise.reject(new Error("unexpected call"));
    return {
        createTask: unexpected,
        completeTask: unexpected,
        today: unexpected,
        inbox: unexpected,
        closeVault: unexpected,
        ...overrides,
    };
}

/** Serve `core`, returning what it posts and a way to send it a request. */
function served(core: WasmCore) {
    const posted: WorkerMessage[] = [];
    let listener: ((event: { data: CoreRequest }) => void) | undefined;
    serve(
        core,
        (msg) => posted.push(msg),
        (l) => {
            listener = l;
        },
    );
    const send = (data: CoreRequest) => listener?.({ data });
    return { posted, send };
}

describe("serve", () => {
    it("answers in arrival order and never overlaps two calls", async () => {
        const slow = deferred<string>();
        const log: string[] = [];
        const core = fakeCore({
            createTask: async (title) => {
                log.push(`start ${title}`);
                const id = await slow.promise;
                log.push(`end ${title}`);
                return id;
            },
            inbox: async () => {
                log.push("start inbox");
                return "[]";
            },
        });
        const { posted, send } = served(core);
        send({ id: 1, call: "createTask", arg: "A" });
        send({ id: 2, call: "inbox", arg: null });

        await vi.waitFor(() => expect(log).toEqual(["start A"]));
        expect(posted).toEqual([]);

        slow.resolve("tsk_a");
        await vi.waitFor(() => expect(posted).toHaveLength(2));
        expect(log).toEqual(["start A", "end A", "start inbox"]);
        expect(posted).toEqual([
            { kind: "reply", id: 1, ok: true, json: "tsk_a" },
            { kind: "reply", id: 2, ok: true, json: "[]" },
        ]);
    });

    it("passes each call its argument", async () => {
        const seen: unknown[] = [];
        const core = fakeCore({
            completeTask: async (id) => {
                seen.push(id);
            },
            today: async (nowMs) => {
                seen.push(nowMs);
                return "[]";
            },
        });
        const { posted, send } = served(core);
        send({ id: 1, call: "completeTask", arg: "tsk_a" });
        send({ id: 2, call: "today", arg: 1_700_000_000_000 });
        await vi.waitFor(() => expect(posted).toHaveLength(2));
        expect(seen).toEqual(["tsk_a", 1_700_000_000_000]);
        expect(posted).toEqual([
            { kind: "reply", id: 1, ok: true, json: "" },
            { kind: "reply", id: 2, ok: true, json: "[]" },
        ]);
    });

    it("answers a throw with ok:false and goes on", async () => {
        const core = fakeCore({
            createTask: () => Promise.reject(new Error("title is empty")),
            inbox: async () => "[]",
        });
        const { posted, send } = served(core);
        send({ id: 1, call: "createTask", arg: "" });
        send({ id: 2, call: "inbox", arg: null });
        await vi.waitFor(() => expect(posted).toHaveLength(2));
        expect(posted).toEqual([
            {
                kind: "reply",
                id: 1,
                ok: false,
                error: "Error: title is empty",
            },
            { kind: "reply", id: 2, ok: true, json: "[]" },
        ]);
    });
});
