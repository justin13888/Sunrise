import { describe, expect, it } from "vitest";
import {
    type CoreCall,
    commands,
    makeWorkerCore,
    queries,
    supportsWorkerCore,
    tasksFromQueryResult,
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
