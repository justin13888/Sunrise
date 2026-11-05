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
 * call each method makes, and how a reply becomes `Task`s. The calls are the
 * web core's typed ones (`crates/sunrise-core-wasm/src/calls.rs`), so no
 * command or query JSON is spelled here: the page names a call and its
 * argument, and the core builds its own `Command` and `Query`.
 */

export type TaskState = "todo" | "in_progress" | "done" | "cancelled";

const TASK_STATES: ReadonlySet<string> = new Set<TaskState>([
    "todo",
    "in_progress",
    "done",
    "cancelled",
]);

/**
 * The state logic and rendering act on. The core sends a state a newer build
 * wrote as its raw string (ADR-0045 §6); like the core, this reads it as
 * `todo`, the open fallback.
 */
export function effectiveTaskState(state: string): TaskState {
    return TASK_STATES.has(state) ? (state as TaskState) : "todo";
}

export interface Task {
    id: string;
    title: string;
    /** A known `TaskState`, or a newer build's state kept verbatim. */
    state: string;
    stream_id: string;
    /** The plan (`planned_at`, ADR-0047), under its wire name. */
    scheduled_at?: string | null;
    /** The hard deadline (`hard_due_at`), under its wire name. */
    due_at?: string | null;
    /** The soft deadline; absent when unset. */
    target_at?: string | null;
}

export interface CoreApi {
    queryToday(): Promise<Task[]>;
    queryInbox(): Promise<Task[]>;
    createTask(title: string): Promise<{ id: string }>;
    completeTask(id: string): Promise<void>;
}

/** The web core's typed calls, by their JS names. */
export type CoreCallName = "createTask" | "completeTask" | "today" | "inbox";

/** What the page asks the worker for: one typed core call. */
export interface CoreRequest {
    id: number;
    call: CoreCallName;
    arg: string | number | null;
}

/** What the worker answers with. */
export type WorkerMessage =
    | { kind: "ready" }
    | { kind: "failed"; reason: string }
    | { kind: "reply"; id: number; ok: true; json: string }
    | { kind: "reply"; id: number; ok: false; error: string };

/**
 * One core call: resolves to what the call answers (an id, nothing, or rows
 * as JSON), rejects with the core's error.
 */
export type CoreCall = (
    call: CoreCallName,
    arg: CoreRequest["arg"],
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

/**
 * The rows `today` and `inbox` answer with, as the core writes them: `id`,
 * `title`, `state` and `stream_id`, nothing else.
 */
export function tasksFromRows(json: string): Task[] {
    const rows = JSON.parse(json) as unknown;
    if (!Array.isArray(rows)) {
        throw new Error(`expected a task list, got ${json.slice(0, 80)}`);
    }
    return rows.map(({ id, title, state, stream_id }: Task) => ({
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
            return tasksFromRows(await call("today", now()));
        },
        async queryInbox() {
            return tasksFromRows(await call("inbox", null));
        },
        async createTask(title: string) {
            return { id: await call("createTask", title) };
        },
        async completeTask(id: string) {
            await call("completeTask", id);
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
    const call: CoreCall = (name, arg) =>
        new Promise((resolve, reject) => {
            const id = next++;
            pending.set(id, { resolve, reject });
            worker.postMessage({ id, call: name, arg } satisfies CoreRequest);
        });
    return makeWorkerCore(call, Date.now);
}

/**
 * The stand-in when there is no core: a list in `localStorage`, so capture
 * still works and the shell renders.
 *
 * It has no Today. Which tasks are on Today — scheduled, due or overdue,
 * read in the reader's zone — is the core's decision, and a stub that
 * guessed it would show a list the real one disagrees with the moment the
 * core loads. Today is empty here; the Inbox holds what was captured.
 */
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
            return [];
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
