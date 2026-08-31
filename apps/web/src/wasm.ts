/**
 * WASM Core bridge.
 *
 * v1 Web: the Rust `sunrise-core` is compiled to wasm32-unknown-unknown
 * via `wasm-bindgen` (build pipeline owned by the developer:
 * `cargo build -p sunrise-core --target wasm32-unknown-unknown`).
 * Until that pipeline runs, `loadCore` returns a stub that mirrors the
 * Core's surface so UI engineers can iterate locally.
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

let cached: CoreApi | null = null;

/**
 * Returns a Core API. v1 returns a stub that lives in localStorage so
 * the PWA shell renders something during early development; the real
 * WASM build replaces this implementation when the bindgen output
 * lands at `apps/web/src/wasm/sunrise_core_bg.wasm`.
 */
export async function loadCore(): Promise<CoreApi> {
    if (cached) {
        return cached;
    }
    cached = makeStub();
    return cached;
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
