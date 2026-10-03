import { fireEvent, render, screen, within } from "@testing-library/react";
import { App } from "../App";
import { setApi } from "../lib/api";
import { createMockApi, type MockOptions, type MockWalletApi } from "../lib/mock";

export const PASSWORD = "correct horse battery";
/** A valid testnet4/signet address (BIP173 test vector, re-encoded with `tb`). */
export const TB_ADDRESS = "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx";
export const BCRT_ADDRESS = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080";
export const BC_ADDRESS = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";

/** A zero-latency mock, prepared by `setup`, installed as the `api`, and the app rendered. */
export async function renderApp(setup?: (mock: MockWalletApi) => Promise<void> | void, options: MockOptions = {}) {
  const mock = createMockApi({ latencyMs: 0, syncStepMs: 0, ...options });
  await setup?.(mock);
  setApi(mock);
  const utils = render(<App />);
  return { mock, ...utils };
}

/** A wallet with `sat` confirmed (one block deep once the app syncs). Locked unless asked. */
export async function fundedWallet(mock: MockWalletApi, { sat = 1_000_000, unlocked = false } = {}) {
  await mock.createWallet(12, PASSWORD);
  mock.simulateIncoming(sat);
  mock.mineBlocks(1);
  if (!unlocked) await mock.lock();
}

export function banner() {
  return within(screen.getByRole("banner"));
}

/** Wait for the dashboard to show after the app's own first sync. */
export async function waitForDashboard() {
  await screen.findByRole("heading", { name: /balance/i });
  await screen.findByText(/^As of block/);
}

export function nav(name: string) {
  fireEvent.click(within(screen.getByRole("navigation", { name: "Main" })).getByRole("button", { name }));
}

export function type(element: HTMLElement, value: string) {
  fireEvent.change(element, { target: { value } });
}
