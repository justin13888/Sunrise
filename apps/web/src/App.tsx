import { taskStateGlyph } from "@sunrise/ui";
import { useEffect, useState } from "react";
import { t } from "./i18n";
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
            <h1>{t.web.app.title()}</h1>
            {error ? (
                <p style={{ color: "tomato" }}>{error}</p>
            ) : (
                <ul style={{ listStyle: "none", padding: 0 }}>
                    {tasks.length === 0 ? (
                        <li>{t.web.app.empty()}</li>
                    ) : (
                        tasks.map((task) => (
                            <li
                                key={task.id}
                                style={{ paddingBlock: 4, paddingInline: 0 }}
                            >
                                <span style={{ fontFamily: "monospace" }}>
                                    {taskStateGlyph[task.state]}
                                </span>{" "}
                                {task.title}
                            </li>
                        ))
                    )}
                </ul>
            )}
        </main>
    );
}
