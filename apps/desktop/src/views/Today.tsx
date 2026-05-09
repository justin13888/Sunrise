import { taskStateGlyph } from "@sunrise/ui";
import type { Task } from "../tauri";

export function Today({ tasks }: { tasks: Task[] }) {
	return (
		<section>
			<header style={{ marginBottom: 12 }}>
				<h2 style={{ margin: 0 }}>Today</h2>
				<small>{tasks.length} tasks</small>
			</header>
			{tasks.length === 0 ? (
				<p>Nothing on the list.</p>
			) : (
				<ul style={{ listStyle: "none", padding: 0, margin: 0 }}>
					{tasks.map((t) => (
						<li key={t.id} style={{ padding: "4px 0" }}>
							<span style={{ fontFamily: "monospace" }}>{taskStateGlyph[t.state]}</span>{" "}
							{t.title}
						</li>
					))}
				</ul>
			)}
		</section>
	);
}
