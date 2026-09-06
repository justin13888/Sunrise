// The generated custom properties, and the reason `tokens.css` is a real
// artefact rather than an emitted file nobody reads. It carries the
// `prefers-color-scheme: dark` and `prefers-reduced-motion: reduce` blocks, so
// importing it once here is what makes them apply.
import "@sunrise/ui-tokens/css";
import React from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";

const container = document.getElementById("root");
if (!container) {
    throw new Error("missing #root");
}
createRoot(container).render(
    <React.StrictMode>
        <App />
    </React.StrictMode>,
);
