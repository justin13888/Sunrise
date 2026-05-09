/**
 * Shared design tokens for Sunrise client UIs.
 * Per `spec/07-clients/shared-ui-system.md`. Stay in sync with the
 * Stream color palette in `crates/sunrise-domain/src/stream.rs::StreamColor`.
 */

export const colors = {
	slate: "#475569",
	rose: "#e11d48",
	amber: "#d97706",
	emerald: "#059669",
	sky: "#0284c7",
	indigo: "#4f46e5",
	violet: "#7c3aed",
	pink: "#db2777",
} as const;

export type StreamColor = keyof typeof colors;

export const spacing = {
	xs: 4,
	sm: 8,
	md: 16,
	lg: 24,
	xl: 32,
} as const;

export const radii = {
	sm: 4,
	md: 8,
	lg: 12,
} as const;

export const taskStateGlyph = {
	todo: "[ ]",
	in_progress: "[·]",
	done: "[x]",
	cancelled: "[/]",
} as const;
