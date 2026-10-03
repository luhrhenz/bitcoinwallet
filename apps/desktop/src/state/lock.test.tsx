import { act, fireEvent, screen, within } from "@testing-library/react";
import { banner, fundedWallet, PASSWORD, renderApp, type, waitForDashboard } from "../test/helpers";

describe("unlock", () => {
  it("shows a friendly error for a wrong password, then unlocks", async () => {
    await renderApp((mock) => fundedWallet(mock));
    await waitForDashboard();
    // Watch-only: balances show while locked.
    expect(screen.getByRole("region", { name: /balance/i })).toHaveTextContent("0.01000000 BTC");

    fireEvent.click(banner().getByRole("button", { name: /Locked/ }));
    const dialog = await screen.findByRole("dialog", { name: "Unlock wallet" });
    const input = within(dialog).getByLabelText("Wallet password");
    expect(input).toHaveFocus();

    type(input, "not my password");
    fireEvent.click(within(dialog).getByRole("button", { name: "Unlock" }));
    expect(await within(dialog).findByText("That password is not correct. Try again.")).toBeInTheDocument();
    expect(dialog).not.toHaveTextContent("wrong_password");

    type(input, PASSWORD);
    fireEvent.click(within(dialog).getByRole("button", { name: "Unlock" }));
    expect(await banner().findByRole("button", { name: /Unlocked/ })).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("closes on Escape without unlocking", async () => {
    await renderApp((mock) => fundedWallet(mock));
    await waitForDashboard();
    fireEvent.click(banner().getByRole("button", { name: /Locked/ }));
    await screen.findByRole("dialog");
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(banner().getByRole("button", { name: /Locked/ })).toBeInTheDocument();
  });
});

describe("auto-lock", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("locks after the configured minutes without activity; activity restarts the clock", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { mock } = await renderApp(async (m) => {
      await fundedWallet(m, { unlocked: true });
      await m.setSettings({ ...(await m.getSettings()), auto_lock_minutes: 2 });
    });
    await waitForDashboard();
    expect(banner().getByRole("button", { name: /Unlocked/ })).toBeInTheDocument();
    const lock = vi.spyOn(mock, "lock");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(60_000);
    });
    fireEvent.keyDown(window, { key: "Shift" }); // the user is still here
    await act(async () => {
      await vi.advanceTimersByTimeAsync(90_000);
    });
    expect(lock).not.toHaveBeenCalled();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(31_000);
    });
    expect(lock).toHaveBeenCalledTimes(1);
    expect(await screen.findByText(/Locked after 2 minutes without activity/)).toBeInTheDocument();
    expect(banner().getByRole("button", { name: /Locked/ })).toBeInTheDocument();
  });

  it("doesn't run while the wallet is locked", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { mock } = await renderApp((m) => fundedWallet(m));
    await waitForDashboard();
    const lock = vi.spyOn(mock, "lock");
    await act(async () => {
      await vi.advanceTimersByTimeAsync(60 * 60_000);
    });
    expect(lock).not.toHaveBeenCalled();
  });
});
