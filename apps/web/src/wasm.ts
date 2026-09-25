/**
 * The web client's bridge to the core (ADR-0055).
 *
 * `loadCore()` runs the real `sunrise-core`, compiled to wasm, in a dedicated
 * worker (`core.worker.ts`) when the browser can host it: workers, OPFS and
 * `navigator.locks`. Where it cannot, or where the worker fails to open the
 * vault (no wasm bundle was built, or OPFS refused a sync access handle), it
 * falls back to a `localStorage` stub with the same surface, so the shell
 * still renders.
 *
 * Everything below `loadCore` is pure and is what `wasm.test.ts` drives: the
 * JSON each call sends, and how a reply becomes `Task`s.
 */

export type TaskState = "todo" | "in_progress" | "done" | "cancelled";

export interface Task {
    id: string;
    title: string;
    state: TaskState;
    stream_id: string;
    scheduled_at?: string | null;
    due_at?: string | null;
}

export interface CoreApi {
    queryToday(): Promise<Task[]>;
    queryInbox(): Promise<Task[]>;
    createTask(title: string): Promise<{ id: string }>;
    completeTask(id: string): Promise<void>;
}

/** What the page asks the worker for: one core call, JSON in. */
export interface CoreRequest {
    id: number;
    kind: "submit" | "query";
    json: string;
}

/** What the worker answers with. */
export type WorkerMessage =
    | { kind: "ready" }
    | { kind: "failed"; reason: string }
    | { kind: "reply"; id: number; ok: true; json: string }
    | { kind: "reply"; id: number; ok: false; error: string };

/** One core call: resolves to the result JSON, rejects with the core's error. */
export type CoreCall = (
    kind: CoreRequest["kind"],
    json: string,
) => Promise<string>;

let cached: Promise<CoreApi> | null = null;

/**
 * Returns the core. Cached, so every caller shares one worker and one vault.
 */
export function loadCore(): Promise<CoreApi> {
    cached ??= openWorkerCore().then((core) => core ?? makeStub());
    return cached;
}

/** The capabilities the worker path needs, read off `globalThis`. */
export interface WorkerEnv {
    Worker?: unknown;
    navigator?: {
        storage?: { getDirectory?: unknown };
        locks?: unknown;
    };
}

/**
 * Whether this browser can host the worker core: a dedicated worker for the
 * SAHPool VFS, OPFS for the vault, and `navigator.locks` for single-tab
 * ownership. Private windows that withhold OPFS answer `false` here or fail
 * in the worker; both land on the stub.
 */
export function supportsWorkerCore(env: WorkerEnv): boolean {
    return (
        typeof env.Worker === "function" &&
        typeof env.navigator?.storage?.getDirectory === "function" &&
        env.navigator?.locks !== undefined
    );
}

/** `Command` JSON, in `serde_json`'s externally tagged form. */
export const commands = {
    createTask: (title: string) =>
        JSON.stringify({ CreateTask: { title, contexts: [] } }),
    completeTask: (id: string) => JSON.stringify({ CompleteTask: id }),
};

/** `Query` JSON, likewise. */
export const queries = {
    today: (nowMs: number) =>
        JSON.stringify({ Today: { now_ms: nowMs, contexts: [] } }),
    inbox: () => JSON.stringify("Inbox"),
};

interface CoreTask {
    id: string;
    title: string;
    state: TaskState;
    stream_id: string;
}

/**
 * The tasks in a `QueryResult`: `Tasks` (Today) or `StreamTasks` (Inbox).
 *
 * `scheduled_at` and `due_at` are left off. The core sends them as tagged
 * `SunriseTime` objects, not the strings this `Task` declares, and nothing on
 * screen reads them yet.
 */
export function tasksFromQueryResult(json: string): Task[] {
    const result = JSON.parse(json) as {
        Tasks?: CoreTask[];
        StreamTasks?: CoreTask[];
    };
    const rows = result.Tasks ?? result.StreamTasks;
    if (!rows) {
        throw new Error(`expected a task list, got ${json.slice(0, 80)}`);
    }
    return rows.map(({ id, title, state, stream_id }) => ({
        id,
        title,
        state,
        stream_id,
    }));
}

/** The `CoreApi` over a core call, however the call is carried. */
export function makeWorkerCore(call: CoreCall, now: () => number): CoreApi {
    return {
        async queryToday() {
            return tasksFromQueryResult(
                await call("query", queries.today(now())),
            );
        },
        async queryInbox() {
            return tasksFromQueryResult(await call("query", queries.inbox()));
        },
        async createTask(title: string) {
            const result = JSON.parse(
                await call("submit", commands.createTask(title)),
            ) as { entity: string };
            return { id: result.entity };
        },
        async completeTask(id: string) {
            await call("submit", commands.completeTask(id));
        },
    };
}

/**
 * Start the worker and wait for it to open the vault. `null` means use the
 * stub.
 *
 * Waits as long as another tab holds the vault: the worker queues on the
 * lock, and opens the vault when that tab closes (ADR-0055 §4).
 */
async function openWorkerCore(): Promise<CoreApi | null> {
    if (!supportsWorkerCore(globalThis as WorkerEnv)) {
        return null;
    }
    const worker = new Worker(new URL("./core.worker.ts", import.meta.url), {
        type: "module",
    });
    const pending = new Map<
        number,
        { resolve: (json: string) => void; reject: (e: Error) => void }
    >();
    const opened = new Promise<boolean>((resolve) => {
        worker.addEventListener("error", (event) => {
            // Before `ready` this falls back to the stub; after it, no reply
            // is coming for what is in flight, so say so rather than hang.
            resolve(false);
            for (const waiter of pending.values()) {
                waiter.reject(new Error(`web core worker: ${event.message}`));
            }
            pending.clear();
        });
        worker.addEventListener(
            "message",
            (event: MessageEvent<WorkerMessage>) => {
                const msg = event.data;
                if (msg.kind === "ready") {
                    resolve(true);
                } else if (msg.kind === "failed") {
                    console.warn(
                        `web core unavailable, using the stub: ${msg.reason}`,
                    );
                    resolve(false);
                } else {
                    const waiter = pending.get(msg.id);
                    pending.delete(msg.id);
                    if (msg.ok) {
                        waiter?.resolve(msg.json);
                    } else {
                        waiter?.reject(new Error(msg.error));
                    }
                }
            },
        );
    });
    if (!(await opened)) {
        worker.terminate();
        return null;
    }
    let next = 0;
    const call: CoreCall = (kind, json) =>
        new Promise((resolve, reject) => {
            const id = next++;
            pending.set(id, { resolve, reject });
            worker.postMessage({ id, kind, json } satisfies CoreRequest);
        });
    return makeWorkerCore(call, Date.now);
}

function makeStub(): CoreApi {
    const KEY = "sunrise-web-stub-tasks-v1";
    function read(): Task[] {
        try {
            const raw = localStorage.getItem(KEY);
            return raw ? (JSON.parse(raw) as Task[]) : [];
        } catch {
            return [];
        }
    }
    function write(t: Task[]) {
        localStorage.setItem(KEY, JSON.stringify(t));
    }
    return {
        async queryToday() {
            return read().filter(
                (t) => t.state !== "done" && t.state !== "cancelled",
            );
        },
        async queryInbox() {
            return read();
        },
        async createTask(title: string) {
            const tasks = read();
            const id = `tsk_${Date.now()}_${Math.random().toString(36).slice(2, 8)}`;
            tasks.push({ id, title, state: "todo", stream_id: "str_inbox" });
            write(tasks);
            return { id };
        },
        async completeTask(id: string) {
            const tasks = read().map((t) =>
                t.id === id ? { ...t, state: "done" as const } : t,
            );
            write(tasks);
        },
    };
}
