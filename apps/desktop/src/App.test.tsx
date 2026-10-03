import { fireEvent, screen, within } from "@testing-library/react";
import { banner, fundedWallet, nav, PASSWORD, renderApp, type, waitForDashboard } from "./test/helpers";

const ABANDON = `${"abandon ".repeat(11)}about`;

describe("network badge", () => {
  it("is on every main screen", async () => {
    await renderApp((mock) => fundedWallet(mock));
    await waitForDashboard();
    expect(banner().getByText("TESTNET4")).toBeInTheDocument();

    for (const [item, heading] of [
      ["Receive", "Receive"],
      ["Send", "Send"],
      ["History", "History"],
      ["Settings", "Settings"],
      ["Overview", /balance/i],
    ] as const) {
      nav(item);
      await screen.findByRole("heading", { name: heading });
      expect(banner().getByText("TESTNET4")).toBeInTheDocument();
    }

    // The transaction screen too.
    nav("History");
    fireEvent.click(within(await screen.findByRole("list", { name: "All transactions" })).getAllByRole("button")[0]!);
    await screen.findByRole("heading", { name: "Received" });
    expect(banner().getByText("TESTNET4")).toBeInTheDocument();
  });

  it("is on the welcome screen and follows the network", async () => {
    await renderApp(undefined, { network: "regtest" });
    await screen.findByRole("button", { name: /create a new wallet/i });
    expect(banner().getByText("REGTEST")).toBeInTheDocument();
    expect(banner().queryByText("TESTNET4")).toBeNull();
  });
});

describe("dashboard", () => {
  it("syncs on open and shows balances as of a block", async () => {
    const { mock } = await renderApp(async (m) => {
      await fundedWallet(m);
      m.simulateIncoming(25_000); // still in the mempool
    });
    await waitForDashboard();
    const balance = screen.getByRole("region", { name: /balance/i });
    expect(balance).toHaveTextContent("0.01025000 BTC");
    expect(balance).toHaveTextContent("1,025,000 sat");
    expect(balance).toHaveTextContent("Confirmed0.01000000 BTC");
    expect(balance).toHaveTextContent("Unconfirmed0.00025000 BTC");
    expect(balance).toHaveTextContent(`As of block ${mock.tipHeight()}`);
    expect(screen.getByRole("list", { name: "Recent transactions" })).toHaveTextContent("Unconfirmed");
  });

  it("says when the node can't be reached and keeps the last numbers", async () => {
    const { mock } = await renderApp((m) => fundedWallet(m));
    await waitForDashboard();
    mock.setNodeOnline(false);
    fireEvent.click(screen.getByRole("button", { name: "Sync now" }));
    expect(await screen.findByText(/Can't reach your Bitcoin node/)).toBeInTheDocument();
    expect(screen.getByRole("region", { name: /balance/i })).toHaveTextContent("0.01000000 BTC");
  });
});

describe("receive", () => {
  it("shows the current address with a QR code and the address list", async () => {
    const { mock } = await renderApp((m) => fundedWallet(m));
    await waitForDashboard();
    nav("Receive");
    const current = await screen.findByRole("region", { name: "Current receive address" });
    // Address #0 was paid, so a fresh #1 is shown.
    expect(current).toHaveTextContent("Address #1");
    const address = (await mock.listAddresses())[1]!.address;
    expect(current).toHaveTextContent(address);
    expect(current.querySelector("svg title")?.textContent).toBe(`QR code for ${address}`);
    expect(screen.getByText("#0").parentElement).toHaveTextContent("Used");
    expect(screen.getByText("#1").parentElement).toHaveTextContent("Current");
  });

  it("copies the address and says so", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
    await renderApp((m) => fundedWallet(m));
    await waitForDashboard();
    nav("Receive");
    fireEvent.click(await screen.findByRole("button", { name: "Copy address" }));
    expect(await screen.findByRole("button", { name: "Copied" })).toBeInTheDocument();
    expect(writeText).toHaveBeenCalledWith(expect.stringMatching(/^tb1q/));
  });
});

describe("restore", () => {
  it("takes a phrase without spellcheck, explains mistakes, then scans with progress", async () => {
    await renderApp();
    fireEvent.click(await screen.findByRole("button", { name: /restore from a recovery phrase/i }));

    const phrase = screen.getByLabelText("Recovery phrase");
    expect(phrase).toHaveAttribute("autocomplete", "off");
    expect(phrase).toHaveAttribute("spellcheck", "false");
    expect(phrase).toHaveAttribute("autocorrect", "off");
    expect(phrase).toHaveAttribute("autocapitalize", "off");

    type(phrase, "abandon abandon");
    expect(screen.getByText("2 words. A recovery phrase has 12, 15, 18, 21 or 24.")).toBeInTheDocument();

    // Twelve words, wrong checksum: the backend's reason names no words.
    type(phrase, "abandon ".repeat(12));
    expect(screen.getByText("12 words.")).toBeInTheDocument();
    type(screen.getByLabelText("New wallet password"), PASSWORD);
    type(screen.getByLabelText("Repeat the password"), PASSWORD);
    fireEvent.click(screen.getByRole("button", { name: "Restore wallet" }));
    expect(
      await screen.findByText(
        "That recovery phrase isn't valid. Check each word and the order. Checksum mismatch (a word is probably mistyped or out of order).",
      ),
    ).toBeInTheDocument();

    type(phrase, `  ${ABANDON.toUpperCase()}\n`);
    type(screen.getByLabelText("Wallet birthday (optional)"), "12a");
    fireEvent.click(screen.getByRole("button", { name: "Restore wallet" }));
    expect(screen.getByText("A block height is a whole number, like 118000.")).toBeInTheDocument();

    type(screen.getByLabelText("Wallet birthday (optional)"), "");
    fireEvent.click(screen.getByRole("button", { name: "Restore wallet" }));
    expect(await screen.findByRole("heading", { name: "Wallet restored" })).toBeInTheDocument();
    expect(screen.getByText(/Found 3 transactions/)).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Open wallet" }));
    await waitForDashboard();
    expect(screen.getByRole("region", { name: /balance/i })).toHaveTextContent("0.00269718 BTC");
  });
});

describe("settings", () => {
  it("explains that each network has its own wallet and switches to it", async () => {
    await renderApp((m) => fundedWallet(m));
    await waitForDashboard();
    nav("Settings");
    await screen.findByRole("heading", { name: "Settings" });

    fireEvent.click(screen.getByRole("radio", { name: /SIGNET/ }));
    expect(screen.getByText("signet has its own wallet, separate from your testnet4 wallet.")).toBeInTheDocument();
    expect(screen.getByLabelText("RPC URL")).toHaveAttribute("placeholder", "http://127.0.0.1:38332");

    fireEvent.click(screen.getByRole("button", { name: "Switch to signet" }));
    // No signet wallet yet: back to Welcome, on signet.
    expect(await screen.findByRole("button", { name: /create a new wallet/i })).toBeInTheDocument();
    expect(banner().getByText("SIGNET")).toBeInTheDocument();
  });

  it("validates auto-lock minutes and the RPC URL", async () => {
    const { mock } = await renderApp((m) => fundedWallet(m));
    await waitForDashboard();
    nav("Settings");
    const save = vi.spyOn(mock, "setSettings");
    type(await screen.findByLabelText("Auto-lock after (minutes)"), "0");
    type(screen.getByLabelText("RPC URL"), "localhost:48332");
    fireEvent.click(screen.getByRole("button", { name: "Save settings" }));
    expect(screen.getByText("Choose a whole number of minutes from 1 to 60.")).toBeInTheDocument();
    expect(screen.getByText("Use a full URL, like http://127.0.0.1:48332.")).toBeInTheDocument();
    expect(save).not.toHaveBeenCalled();

    type(screen.getByLabelText("Auto-lock after (minutes)"), "10");
    type(screen.getByLabelText("RPC URL"), "http://127.0.0.1:48332");
    fireEvent.click(screen.getByRole("button", { name: "Save settings" }));
    expect(await screen.findByText("Settings saved.")).toBeInTheDocument();
    expect(save).toHaveBeenCalledWith({
      network: "testnet4",
      rpc_url: "http://127.0.0.1:48332",
      rpc_cookie: null,
      auto_lock_minutes: 10,
    });
  });
});

describe("startup", () => {
  it("shows a plain error and a retry when the backend fails", async () => {
    const { mock } = await renderApp((m) => {
      vi.spyOn(m, "appInfo").mockRejectedValueOnce(new Error("boom\n    at secret.rs:1"));
    });
    expect(await screen.findByRole("alert")).toHaveTextContent("Something went wrong.boom");
    expect(screen.getByRole("alert")).not.toHaveTextContent("secret.rs");
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("button", { name: /create a new wallet/i })).toBeInTheDocument();
    expect(mock.appInfo).toHaveBeenCalledTimes(2);
  });
});
