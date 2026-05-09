import { useEffect, useState } from "react";
import { coreInvoke, type Task } from "./tauri";
import { Today } from "./views/Today";

type View = "today" | "inbox";

export function App() {
	const [view, setView] = useState<View>("today");
	const [tasks, setTasks] = useState<Task[]>([]);

	useEffect(() => {
		coreInvoke<Task[]>("query_today", { now_ms: Date.now() })
			.then(setTasks)
			.catch((err) => console.error(err));
	}, [view]);

	return (
		<div style={styles.shell}>
			<nav style={styles.nav}>
				<button
					type="button"
					style={view === "today" ? styles.activeTab : styles.tab}
					onClick={() => setView("today")}
				>
					Today
				</button>
				<button
					type="button"
					style={view === "inbox" ? styles.activeTab : styles.tab}
					onClick={() => setView("inbox")}
				>
					Inbox
				</button>
			</nav>
			<main style={styles.main}>
				{view === "today" ? <Today tasks={tasks} /> : <div>Inbox (coming soon)</div>}
			</main>
		</div>
	);
}

const styles: Record<string, React.CSSProperties> = {
	shell: {
		display: "flex",
		flexDirection: "column",
		height: "100vh",
		fontFamily: "system-ui, -apple-system, sans-serif",
	},
	nav: {
		display: "flex",
		gap: 8,
		padding: 8,
		borderBottom: "1px solid #333",
	},
	tab: {
		padding: "4px 12px",
		border: "1px solid transparent",
		background: "transparent",
		cursor: "pointer",
	},
	activeTab: {
		padding: "4px 12px",
		border: "1px solid #999",
		background: "#222",
		color: "#fff",
		cursor: "pointer",
	},
	main: {
		flex: 1,
		padding: 16,
		overflow: "auto",
	},
};
