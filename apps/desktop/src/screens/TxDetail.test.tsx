import { act, fireEvent, screen, within } from "@testing-library/react";
import type { MockWalletApi } from "../lib/mock";
import { fundedWallet, nav, PASSWORD, renderApp, TB_ADDRESS, type, waitForDashboard } from "../test/helpers";

/** A funded wallet that paid 100,000 sat to TB_ADDRESS at 2 sat/vB (282 sat, 141 vB), unconfirmed. */
async function walletWithPayment(mock: MockWalletApi, { unlocked = true, label = null as string | null } = {}) {
  await fundedWallet(mock, { unlocked: true });
  await mock.sync();
  const { id } = await mock.prepareSend(TB_ADDRESS, 100_000, 2);
  const { txid } = await mock.confirmSend(id);
  if (label) await mock.setLabel(txid, label);
  if (!unlocked) await mock.lock();
  return txid;
}

/** Open a transaction from the History list by its position (newest first). */
async function openFromHistory(index: number) {
  nav("History");
  const list = await screen.findByRole("list", { name: "All transactions" });
  fireEvent.click(within(list).getAllByRole("button")[index]!);
}

describe("labels", () => {
  it("are added, edited and removed on the transaction screen and shown in the lists", async () => {
    const { mock } = await renderApp((m) => fundedWallet(m));
    await waitForDashboard();
    const [incoming] = await mock.history();
    await openFromHistory(0);
    await screen.findByRole("heading", { name: "Received" });
    expect(screen.getByText("No label")).toBeInTheDocument();

    // Locked: labels need no password.
    fireEvent.click(screen.getByRole("button", { name: "Add a label" }));
    const field = screen.getByLabelText("Label for this transaction");
    expect(field).toHaveFocus();
    fireEvent.click(screen.getByRole("button", { name: "Save label" }));
    expect(screen.getByText("Enter a label.")).toBeInTheDocument();
    type(field, "x".repeat(101));
    fireEvent.click(screen.getByRole("button", { name: "Save label" }));
    expect(await screen.findByText(/a label can be at most 100 characters/i)).toBeInTheDocument();
    type(field, "  October salary ");
    fireEvent.click(screen.getByRole("button", { name: "Save label" }));
    expect(await screen.findByText("October salary")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Edit label" })).toHaveFocus();
    expect((await mock.history())[0]!.label).toBe("October salary");

    // In History and on the dashboard.
    nav("History");
    expect(await screen.findByRole("list", { name: "All transactions" })).toHaveTextContent("October salary");
    nav("Overview");
    expect(await screen.findByRole("list", { name: "Recent transactions" })).toHaveTextContent("October salary");

    // Edit, then remove.
    await openFromHistory(0);
    fireEvent.click(await screen.findByRole("button", { name: "Edit label" }));
    expect(screen.getByLabelText("Label for this transaction")).toHaveValue("October salary");
    type(screen.getByLabelText("Label for this transaction"), "Salary");
    fireEvent.click(screen.getByRole("button", { name: "Save label" }));
    expect(await screen.findByText("Salary")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Remove label" }));
    expect(await screen.findByText("No label")).toBeInTheDocument();
    expect((await mock.history()).find((r) => r.txid === incoming!.txid)!.label).toBeNull();
    expect((await mock.appInfo()).unlocked).toBe(false);
  });
});

describe("speed up", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("is offered only for an unconfirmed outgoing payment", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { mock } = await renderApp(async (m) => void (await walletWithPayment(m)));
    await waitForDashboard();
    // Newest first: the payment, then the incoming funds.
    await openFromHistory(1);
    await screen.findByRole("heading", { name: "Received" });
    expect(screen.queryByRole("button", { name: "Speed up" })).toBeNull();

    await openFromHistory(0);
    await screen.findByRole("heading", { name: "Sent" });
    expect(await screen.findByRole("button", { name: "Speed up" })).toBeInTheDocument();

    // Once it confirms (the screen's next poll), the button goes away.
    mock.mineBlocks(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000);
    });
    await screen.findByText(/^Confirmed ·/);
    expect(screen.queryByRole("button", { name: "Speed up" })).toBeNull();
  });

  it("enforces the minimum, previews the replacement, and opens it after sending", async () => {
    const { mock } = await renderApp(async (m) => void (await walletWithPayment(m, { label: "rent" })));
    await waitForDashboard();
    const [payment] = await mock.history();
    const original = payment!.txid;
    const prepare = vi.spyOn(mock, "prepareFeeBump");
    const confirm = vi.spyOn(mock, "confirmSend");
    await openFromHistory(0);
    fireEvent.click(await screen.findByRole("button", { name: "Speed up" }));

    // Pre-filled with the minimum (old 2 sat/vB + 1); lower is refused before Rust is asked.
    const rate = await screen.findByLabelText("New fee rate");
    expect(rate).toHaveValue("3");
    expect(rate).toHaveFocus();
    expect(screen.getByText(/^At least 3 sat\/vB/)).toBeInTheDocument();
    type(rate, "2.5");
    fireEvent.click(screen.getByRole("button", { name: "Review" }));
    expect(screen.getByText("Replacing this payment needs at least 3 sat/vB.")).toBeInTheDocument();
    expect(prepare).not.toHaveBeenCalled();

    type(rate, "5");
    fireEvent.click(screen.getByRole("button", { name: "Review" }));
    const preview = await screen.findByRole("region", { name: "Speed-up preview" });
    expect(prepare).toHaveBeenCalledWith(original, 5);
    expect(within(preview).getByText("Replaces").nextElementSibling).toHaveTextContent(original);
    expect(within(preview).getByText("Sending to").nextElementSibling).toHaveTextContent(TB_ADDRESS);
    expect(preview).toHaveTextContent("0.00100000 BTC");
    expect(preview).toHaveTextContent("705 sat · 5 sat/vB × 141 vB");
    expect(preview).toHaveTextContent("423 sat more than the 282 sat it pays now");
    expect(confirm).not.toHaveBeenCalled();

    fireEvent.click(within(preview).getByRole("button", { name: "Send replacement" }));
    expect(await screen.findByText("Sped up. The replacement is on its way.")).toBeInTheDocument();
    const replacement = (await confirm.mock.results[0]!.value) as { txid: string };
    expect(replacement.txid).not.toBe(original);
    expect(screen.getByText(replacement.txid)).toBeInTheDocument();
    expect(screen.getByText("Replaces").parentElement).toHaveTextContent(original);
    // The wallet now has only the replacement, and it kept the label.
    const history = await mock.history();
    expect(history.map((r) => r.txid)).not.toContain(original);
    expect(history.find((r) => r.txid === replacement.txid)!.label).toBe("rent");
    expect(await screen.findByText("rent")).toBeInTheDocument();
  });

  it("asks for the password first when locked, and cancel sends nothing", async () => {
    const { mock } = await renderApp(async (m) => void (await walletWithPayment(m, { unlocked: false })));
    await waitForDashboard();
    const cancel = vi.spyOn(mock, "cancelSend");
    const confirm = vi.spyOn(mock, "confirmSend");
    await openFromHistory(0);
    fireEvent.click(await screen.findByRole("button", { name: "Speed up" }));
    await screen.findByLabelText("New fee rate");
    expect(screen.getByText(/You'll be asked for your password when you review/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Review" }));

    const dialog = await screen.findByRole("dialog", { name: "Unlock wallet" });
    type(within(dialog).getByLabelText("Wallet password"), PASSWORD);
    fireEvent.click(within(dialog).getByRole("button", { name: "Unlock" }));
    const preview = await screen.findByRole("region", { name: "Speed-up preview" });

    fireEvent.click(within(preview).getByRole("button", { name: "Cancel" }));
    expect(await screen.findByText("Cancelled. Nothing was sent.")).toBeInTheDocument();
    expect(cancel).toHaveBeenCalledTimes(1);
    expect(confirm).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "Speed up" })).toBeInTheDocument();
  });

  it("comes back with a clear reason when the payment confirmed before Send", async () => {
    const { mock } = await renderApp(async (m) => void (await walletWithPayment(m)));
    await waitForDashboard();
    await openFromHistory(0);
    fireEvent.click(await screen.findByRole("button", { name: "Speed up" }));
    await screen.findByLabelText("New fee rate");
    fireEvent.click(screen.getByRole("button", { name: "Review" }));
    const preview = await screen.findByRole("region", { name: "Speed-up preview" });
    mock.mineBlocks(1);
    fireEvent.click(within(preview).getByRole("button", { name: "Send replacement" }));
    expect(await screen.findByText(/it is already confirmed/)).toBeInTheDocument();
    expect(screen.queryByRole("region", { name: "Speed-up preview" })).toBeNull();
    expect(await screen.findByText(/^Confirmed ·/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Speed up" })).toBeNull();
  });
});
