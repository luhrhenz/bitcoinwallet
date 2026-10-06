import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { setApi } from "./lib/api";
import type { MockControls } from "./lib/mock";
import "./styles.css";

/**
 * The mock backend, for `npm run dev:mock` or outside Tauri. Dynamically imported, so the Tauri
 * app never fetches it.
 */
async function backend(): Promise<MockControls | undefined> {
  const inTauri = "__TAURI_INTERNALS__" in window;
  if (inTauri && !import.meta.env.VITE_MOCK) return undefined;
  const { createMockApi } = await import("./lib/mock");
  const mock = createMockApi({ latencyMs: 200, syncStepMs: 70, autoMineMs: 45_000 });
  setApi(mock);
  return mock;
}

const root = document.getElementById("root");
if (!root) throw new Error("#root missing");

void backend().then((mock) =>
  createRoot(root).render(
    <StrictMode>
      <App mock={mock} />
    </StrictMode>,
  ),
);
