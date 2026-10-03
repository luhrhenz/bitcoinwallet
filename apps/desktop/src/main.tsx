import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { setApi } from "./lib/api";
import type { MockControls } from "./lib/mock";
import "./styles.css";

/**
 * The in-memory mock backend runs for `npm run dev:mock` (VITE_MOCK) and whenever the page is
 * not inside Tauri (plain `npm run dev` in a browser), where the real commands don't exist.
 * It is loaded with a dynamic import, so it is a separate chunk the Tauri app never fetches.
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
