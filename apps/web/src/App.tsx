import { taskStateGlyph } from "@sunrise/ui";
import { useEffect, useState } from "react";
import { t } from "./i18n";
import { ServerSettings } from "./ServerSettings";
import { effectiveTaskState, loadCore, type Task } from "./wasm";

export function App() {
    const [tasks, setTasks] = useState<Task[]>([]);
    const [error, setError] = useState<string | null>(null);
    const [settingsOpen, setSettingsOpen] = useState(false);

    useEffect(() => {
        loadCore()
            .then(async (core) => {
                const today = await core.queryToday();
                setTasks(today);
            })
            .catch((e) => setError(String(e)));
    }, []);

    return (
        <main
            style={{ fontFamily: "system-ui", padding: 16, maxInlineSize: 720 }}
        >
            <header
                style={{
                    display: "flex",
                    alignItems: "baseline",
                    justifyContent: "space-between",
                    gap: 8,
                }}
            >
                <h1>
                    {settingsOpen ? t.web.settings.title() : t.web.app.title()}
                </h1>
                <button
                    type="button"
                    aria-expanded={settingsOpen}
                    onClick={() => setSettingsOpen((open) => !open)}
                >
                    {settingsOpen
                        ? t.web.settings.done()
                        : t.web.app.openSettings()}
                </button>
            </header>
            {settingsOpen ? (
                <ServerSettings />
            ) : error ? (
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
                                    {
                                        taskStateGlyph[
                                            effectiveTaskState(task.state)
                                        ]
                                    }
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
