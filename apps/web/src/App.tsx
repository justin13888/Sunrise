import { taskStateGlyph } from "@sunrise/ui";
import { useEffect, useState } from "react";
import { loadCore, type Task } from "./wasm";

export function App() {
	const [tasks, setTasks] = useState<Task[]>([]);
	const [error, setError] = useState<string | null>(null);

	useEffect(() => {
		loadCore()
			.then(async (core) => {
				const today = await core.queryToday();
				setTasks(today);
			})
			.catch((e) => setError(String(e)));
	}, []);

	return (
		<main style={{ fontFamily: "system-ui", padding: 16 }}>
			<h1>Sunrise — Web</h1>
			{error ? (
				<p style={{ color: "tomato" }}>{error}</p>
			) : (
				<ul style={{ listStyle: "none", padding: 0 }}>
					{tasks.length === 0 ? (
						<li>Nothing on the list.</li>
					) : (
						tasks.map((t) => (
							<li key={t.id} style={{ padding: "4px 0" }}>
								<span style={{ fontFamily: "monospace" }}>
									{taskStateGlyph[t.state]}
								</span>{" "}
								{t.title}
							</li>
						))
					)}
				</ul>
			)}
		</main>
	);
}
