/**
 * Tauri IPC bridge.
 *
 * In production we use `@tauri-apps/api/core::invoke`; tests / web preview
 * fall back to a window.__SUNRISE_INVOKE__ stub. Production binding lands
 * once Tauri 2 deps are installed locally (deferred per CLAUDE.md note —
 * the `bun install` step is owned by the developer, not the build).
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

declare global {
	interface Window {
		__SUNRISE_INVOKE__?: <T>(cmd: string, args: unknown) => Promise<T>;
	}
}

export async function coreInvoke<T>(cmd: string, args: unknown): Promise<T> {
	if (typeof window !== "undefined" && window.__SUNRISE_INVOKE__) {
		return window.__SUNRISE_INVOKE__<T>(cmd, args);
	}
	// Tauri-runtime path: dynamic import so non-Tauri previews don't 404.
	try {
		const mod = await import("@tauri-apps/api/core");
		return await mod.invoke<T>(cmd, args as Record<string, unknown>);
	} catch (e) {
		console.warn("tauri invoke failed, returning empty result:", e);
		return [] as unknown as T;
	}
}
