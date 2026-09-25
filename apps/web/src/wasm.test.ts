import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
    type CoreApi,
    type CoreCall,
    type CoreRequest,
    commands,
    makeWorkerCore,
    queries,
    supportsWorkerCore,
    tasksFromQueryResult,
    type WorkerMessage,
} from "./wasm";

/**
 * The JSON below is what `crates/sunrise-core-wasm`'s own tests send and get
 * back from the real core; these pin the web side of the same wire.
 */
describe("the web core bridge", () => {
    it("spells commands and queries the way serde_json reads them", () => {
        expect(JSON.parse(commands.createTask("Renew passport"))).toEqual({
            CreateTask: { title: "Renew passport", contexts: [] },
        });
        expect(JSON.parse(commands.completeTask("tsk_x"))).toEqual({
            CompleteTask: "tsk_x",
        });
        expect(JSON.parse(queries.today(42))).toEqual({
            Today: { now_ms: 42, contexts: [] },
        });
        expect(JSON.parse(queries.inbox())).toBe("Inbox");
    });

    it("reads Today's Tasks and the Inbox's StreamTasks alike", () => {
        const row = {
            id: "tsk_a",
            title: "A",
            state: "todo",
            stream_id: "str_inbox",
            created_at: 1,
            scheduled_at: { kind: "instant", at: 5 },
        };
        const expected = [
            { id: "tsk_a", title: "A", state: "todo", stream_id: "str_inbox" },
        ];
        expect(tasksFromQueryResult(JSON.stringify({ Tasks: [row] }))).toEqual(
            expected,
        );
        expect(
            tasksFromQueryResult(JSON.stringify({ StreamTasks: [row] })),
        ).toEqual(expected);
    });

    it("refuses a result that is not a task list", () => {
        expect(() =>
            tasksFromQueryResult(JSON.stringify("SyncStatus")),
        ).toThrow(/expected a task list/);
    });

    it("routes each CoreApi call through one core call", async () => {
        const sent: [string, unknown][] = [];
        const call: CoreCall = async (kind, json) => {
            sent.push([kind, JSON.parse(json)]);
            if (kind === "submit") {
                return JSON.stringify({ entity: "tsk_new" });
            }
            return JSON.stringify({ StreamTasks: [] });
        };
        const core = makeWorkerCore(call, () => 7);

        expect(await core.createTask("T")).toEqual({ id: "tsk_new" });
        await core.completeTask("tsk_new");
        expect(await core.queryInbox()).toEqual([]);
        expect(await core.queryToday()).toEqual([]);
        expect(sent).toEqual([
            ["submit", { CreateTask: { title: "T", contexts: [] } }],
            ["submit", { CompleteTask: "tsk_new" }],
            ["query", "Inbox"],
            ["query", { Today: { now_ms: 7, contexts: [] } }],
        ]);
    });

    it("surfaces the core's refusal as a rejection", async () => {
        const core = makeWorkerCore(
            () => Promise.reject(new Error("vault is closed")),
            () => 0,
        );
        await expect(core.queryInbox()).rejects.toThrow("vault is closed");
    });

    it("takes the worker path only with workers, OPFS and locks", () => {
        const full = {
            Worker: class {},
            navigator: { storage: { getDirectory: () => {} }, locks: {} },
        };
        expect(supportsWorkerCore(full)).toBe(true);
        expect(supportsWorkerCore({ ...full, Worker: undefined })).toBe(false);
        expect(supportsWorkerCore({ ...full, navigator: { locks: {} } })).toBe(
            false,
        );
        expect(
            supportsWorkerCore({
                ...full,
                navigator: { storage: { getDirectory: () => {} } },
            }),
        ).toBe(false);
        expect(supportsWorkerCore({})).toBe(false);
    });
});

/**
 * A stand-in for the dedicated worker: records what the page posts, and lets
 * a test play the worker's side of the conversation.
 */
class FakeWorker {
    static built: FakeWorker[] = [];
    readonly posted: CoreRequest[] = [];
    terminated = false;
    private readonly listeners = {
        message: [] as ((event: { data: WorkerMessage }) => void)[],
        error: [] as ((event: { message: string }) => void)[],
    };

    constructor(
        readonly url: URL,
        readonly options: { type?: string },
    ) {
        FakeWorker.built.push(this);
    }

    addEventListener(type: "message" | "error", listener: never) {
        this.listeners[type].push(listener);
    }

    postMessage(request: CoreRequest) {
        this.posted.push(request);
    }

    terminate() {
        this.terminated = true;
    }

    say(data: WorkerMessage) {
        for (const listener of this.listeners.message) {
            listener({ data });
        }
    }

    crash(message: string) {
        for (const listener of this.listeners.error) {
            listener({ message });
        }
    }
}

/**
 * `loadCore` end to end over a fake `Worker`: which path it takes, and how
 * replies find their callers. Each case imports a fresh module, because
 * `loadCore` caches its core for the life of the page.
 */
describe("loadCore", () => {
    const store = new Map<string, string>();

    beforeEach(() => {
        vi.resetModules();
        FakeWorker.built = [];
        store.clear();
        vi.stubGlobal("Worker", FakeWorker);
        vi.stubGlobal("navigator", {
            storage: { getDirectory: () => {} },
            locks: {},
        });
        vi.stubGlobal("localStorage", {
            getItem: (key: string) => store.get(key) ?? null,
            setItem: (key: string, value: string) => store.set(key, value),
        });
    });

    afterEach(() => {
        vi.unstubAllGlobals();
        vi.restoreAllMocks();
    });

    async function start() {
        const { loadCore } = await import("./wasm");
        const core = loadCore();
        const worker = FakeWorker.built.at(-1);
        return { core, worker, loadCore };
    }

    /** The stub keeps tasks in `localStorage` and posts nothing. */
    async function expectStub(
        worker: FakeWorker | undefined,
        core: Promise<CoreApi>,
    ) {
        const api = await core;
        const { id } = await api.createTask("Offline");
        expect(id).toMatch(/^tsk_/);
        expect((await api.queryInbox()).map((t) => t.title)).toEqual([
            "Offline",
        ]);
        expect(worker?.posted ?? []).toEqual([]);
    }

    it("builds no worker where the browser cannot host one", async () => {
        vi.stubGlobal("navigator", { storage: { getDirectory: () => {} } });
        const { core, worker } = await start();
        expect(worker).toBeUndefined();
        await expectStub(worker, core);
    });

    it("falls back to the stub when the worker cannot open the vault", async () => {
        const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
        const { core, worker } = await start();
        expect(worker?.url.pathname).toMatch(/core\.worker\.ts$/);
        expect(worker?.options).toEqual({ type: "module" });
        worker?.say({ kind: "failed", reason: "no wasm bundle" });
        await expectStub(worker, core);
        expect(worker?.terminated).toBe(true);
        expect(warn).toHaveBeenCalledWith(
            expect.stringContaining("no wasm bundle"),
        );
    });

    it("falls back to the stub when the worker errors before ready", async () => {
        const { core, worker } = await start();
        worker?.crash("import failed");
        await expectStub(worker, core);
        expect(worker?.terminated).toBe(true);
    });

    it("routes each reply to its caller by id, in any order", async () => {
        const { core, worker, loadCore } = await start();
        worker?.say({ kind: "ready" });
        const api = await core;
        expect(await loadCore()).toBe(api);
        expect(worker?.terminated).toBe(false);

        const created = api.createTask("Renew passport");
        const inbox = api.queryInbox();
        const completed = api.completeTask("tsk_gone");
        expect(worker?.posted).toEqual([
            {
                id: 0,
                kind: "submit",
                json: commands.createTask("Renew passport"),
            },
            { id: 1, kind: "query", json: queries.inbox() },
            { id: 2, kind: "submit", json: commands.completeTask("tsk_gone") },
        ]);

        worker?.say({ kind: "reply", id: 2, ok: false, error: "no such task" });
        worker?.say({
            kind: "reply",
            id: 1,
            ok: true,
            json: JSON.stringify({
                StreamTasks: [
                    { id: "tsk_a", title: "A", state: "todo", stream_id: "s" },
                ],
            }),
        });
        worker?.say({
            kind: "reply",
            id: 0,
            ok: true,
            json: JSON.stringify({ entity: "tsk_new" }),
        });

        await expect(completed).rejects.toThrow("no such task");
        expect(await inbox).toEqual([
            { id: "tsk_a", title: "A", state: "todo", stream_id: "s" },
        ]);
        expect(await created).toEqual({ id: "tsk_new" });
    });

    it("rejects the calls in flight when the worker errors after ready", async () => {
        const { core, worker } = await start();
        worker?.say({ kind: "ready" });
        const api = await core;

        const first = api.queryInbox();
        const second = api.createTask("T");
        worker?.crash("out of memory");

        await expect(first).rejects.toThrow("web core worker: out of memory");
        await expect(second).rejects.toThrow("web core worker: out of memory");
        // A reply that arrives after the crash has no caller left to reach.
        expect(() =>
            worker?.say({ kind: "reply", id: 0, ok: true, json: "{}" }),
        ).not.toThrow();
    });
});
