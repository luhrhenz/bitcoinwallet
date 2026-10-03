import { act, fireEvent, screen, within } from "@testing-library/react";
import {
  BC_ADDRESS,
  BCRT_ADDRESS,
  fundedWallet,
  nav,
  PASSWORD,
  renderApp,
  TB_ADDRESS,
  type,
  waitForDashboard,
} from "../test/helpers";

function fillForm(address: string, amount: string, unit: "BTC" | "sat" = "BTC") {
  type(screen.getByLabelText("Recipient address"), address);
  fireEvent.click(screen.getByRole("radio", { name: unit }));
  type(screen.getByLabelText("Amount"), amount);
}

function review() {
  fireEvent.click(screen.getByRole("button", { name: "Review" }));
}

async function openSend(options: { unlocked?: boolean } = {}) {
  const rendered = await renderApp((mock) => fundedWallet(mock, { unlocked: options.unlocked ?? true }));
  await waitForDashboard();
  nav("Send");
  await screen.findByRole("heading", { name: "Send" });
  return rendered;
}

describe("send form", () => {
  it("validates input and maps backend errors to the right field", async () => {
    await openSend();

    review();
    expect(screen.getByText("Enter the address you're sending to.")).toBeInTheDocument();
    expect(screen.getByText("Enter an amount.")).toBeInTheDocument();

    fillForm(BCRT_ADDRESS, "0.001");
    review();
    expect(
      await screen.findByText("That address is for a different network. On testnet4, addresses start with tb1."),
    ).toBeInTheDocument();

    fillForm(BC_ADDRESS, "0.001");
    review();
    expect(
      await screen.findByText("That address is for a different network. On testnet4, addresses start with tb1."),
    ).toBeInTheDocument();

    fillForm("tb1qnotanaddress", "0.001");
    review();
    expect(
      await screen.findByText("That isn't a valid Bitcoin address. Check it against the one you were given."),
    ).toBeInTheDocument();

    fillForm(TB_ADDRESS, "293", "sat");
    review();
    expect(await screen.findByText("That amount is too small to send. The minimum is 294 sat.")).toBeInTheDocument();

    fillForm(TB_ADDRESS, "2", "BTC");
    review();
    expect(
      await screen.findByText(/^Not enough funds to send this amount plus the network fee\. \(need 2\.\d{8} BTC, available 0\.01000000 BTC\)$/),
    ).toBeInTheDocument();

    fillForm(TB_ADDRESS, "0,001");
    review();
    expect(screen.getByText("Use a dot for decimals (0.001) and no thousands separators.")).toBeInTheDocument();

    type(screen.getByLabelText("Fee rate (optional)"), "-1");
    review();
    expect(screen.getByText("Enter a number of sat/vB, like 2 or 2.5.")).toBeInTheDocument();
  });

  it("converts between BTC and sat without rounding", async () => {
    await openSend();
    const amount = screen.getByLabelText("Amount");
    type(amount, "0.29");
    expect(screen.getByText("= 29,000,000 sat")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("radio", { name: "sat" }));
    expect(amount).toHaveValue("29000000");
    fireEvent.click(screen.getByRole("radio", { name: "BTC" }));
    expect(amount).toHaveValue("0.29");
  });
});

describe("send flow", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("previews, confirms, and shows the transaction going from unconfirmed to confirmed", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { mock } = await openSend();
    const confirm = vi.spyOn(mock, "confirmSend");

    fillForm(TB_ADDRESS, "0.0015");
    review();

    const preview = await screen.findByRole("region", { name: "Transaction preview" });
    // The full address, in groups of four, and every number that matters.
    expect(within(preview).getByText("Sending to").nextElementSibling).toHaveTextContent(TB_ADDRESS);
    expect(within(preview).getByText("tb1q")).toBeInTheDocument();
    expect(within(preview).getByText("xpjz")).toBeInTheDocument();
    expect(preview).toHaveTextContent("0.00150000 BTC");
    expect(preview).toHaveTextContent("282 sat · 2 sat/vB × 141 vB");
    expect(preview).toHaveTextContent("0.00849718 BTC");
    expect(preview).toHaveTextContent("150,282 sat");
    expect(confirm).not.toHaveBeenCalled();

    fireEvent.click(within(preview).getByRole("button", { name: "Send 0.00150000 BTC" }));
    expect(await screen.findByText("Sent. Your transaction is on its way.")).toBeInTheDocument();
    expect(await screen.findByText("Unconfirmed · waiting in the mempool")).toBeInTheDocument();
    expect(confirm).toHaveBeenCalledTimes(1);

    // A block is mined; the next 10-second poll sees it.
    mock.mineBlocks(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000);
    });
    expect(await screen.findByText(/^Confirmed ·/)).toHaveTextContent("Confirmed · 1 confirmation");

    mock.mineBlocks(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000);
    });
    expect(await screen.findByText(/^Confirmed ·/)).toHaveTextContent("Confirmed · 2 confirmations");
  });

  it("stops polling when the transaction screen closes", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { mock } = await openSend();
    fillForm(TB_ADDRESS, "0.0015");
    review();
    fireEvent.click(await screen.findByRole("button", { name: "Send 0.00150000 BTC" }));
    await screen.findByText("Unconfirmed · waiting in the mempool");

    // Wait for the screen's own first poll, then watch for any further ones.
    await screen.findByText(/^Checked just now/);
    const status = vi.spyOn(mock, "txStatus");
    nav("Overview");
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(status).not.toHaveBeenCalled();
  });

  it("cancel calls cancelSend and sends nothing", async () => {
    const { mock } = await openSend();
    const cancel = vi.spyOn(mock, "cancelSend");
    const confirm = vi.spyOn(mock, "confirmSend");
    fillForm(TB_ADDRESS, "0.0015");
    review();
    await screen.findByRole("region", { name: "Transaction preview" });

    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(await screen.findByText("Cancelled. Nothing was sent.")).toBeInTheDocument();
    expect(cancel).toHaveBeenCalledTimes(1);
    expect(cancel.mock.calls[0]?.[0]).toMatch(/^[0-9a-f]{32}$/);
    expect(confirm).not.toHaveBeenCalled();
    // Back on the form, with what was typed still there.
    expect(screen.getByLabelText("Recipient address")).toHaveValue(TB_ADDRESS);
  });

  it("goes back to the form when sending fails: the prepared payment is gone", async () => {
    const { mock } = await openSend();
    fillForm(TB_ADDRESS, "0.0015");
    review();
    const preview = await screen.findByRole("region", { name: "Transaction preview" });
    mock.setNodeOnline(false);
    fireEvent.click(within(preview).getByRole("button", { name: "Send 0.00150000 BTC" }));
    expect(await screen.findByText(/Can't reach your Bitcoin node/)).toBeInTheDocument();
    expect(screen.queryByRole("region", { name: "Transaction preview" })).toBeNull();
    // What was typed is still there, ready to review again.
    expect(screen.getByLabelText("Recipient address")).toHaveValue(TB_ADDRESS);
    mock.setNodeOnline(true);
    review();
    expect(await screen.findByRole("region", { name: "Transaction preview" })).toBeInTheDocument();
  });

  it("asks for the password first when the wallet is locked", async () => {
    const { mock } = await openSend({ unlocked: false });
    const prepare = vi.spyOn(mock, "prepareSend");
    fillForm(TB_ADDRESS, "0.0015");
    review();

    const dialog = await screen.findByRole("dialog", { name: "Unlock wallet" });
    expect(prepare).not.toHaveBeenCalled();
    type(within(dialog).getByLabelText("Wallet password"), PASSWORD);
    fireEvent.click(within(dialog).getByRole("button", { name: "Unlock" }));

    expect(await screen.findByRole("region", { name: "Transaction preview" })).toBeInTheDocument();
    expect(prepare).toHaveBeenCalledWith(TB_ADDRESS, 150_000, null);
  });

  it("discards an open preview when the wallet locks", async () => {
    const { mock } = await openSend();
    const cancel = vi.spyOn(mock, "cancelSend");
    fillForm(TB_ADDRESS, "0.0015");
    review();
    await screen.findByRole("region", { name: "Transaction preview" });

    fireEvent.click(screen.getByRole("button", { name: /Unlocked/ }));
    expect(await screen.findByText(/prepared transaction was discarded/)).toBeInTheDocument();
    expect(cancel).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("region", { name: "Transaction preview" })).toBeNull();
  });
});
