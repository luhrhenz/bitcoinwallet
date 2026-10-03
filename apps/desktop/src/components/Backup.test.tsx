import { act, fireEvent, screen, within } from "@testing-library/react";
import type { MockWalletApi } from "../lib/mock";
import { fundedWallet, nav, PASSWORD, renderApp, type, waitForDashboard } from "../test/helpers";

const ABANDON = `${"abandon ".repeat(11)}about`;

function reminder() {
  return screen.queryByRole("region", { name: "Check your recovery phrase backup" });
}

/** The words, read through the mock (the test plays the user's paper copy). */
async function paperCopy(mock: MockWalletApi): Promise<string[]> {
  return (await mock.revealPhrase(PASSWORD)).mnemonic;
}

function wordInputs(dialog: HTMLElement) {
  return within(dialog).getAllByLabelText(/^Word #\d+$/);
}

function positionOf(input: HTMLElement): number {
  return Number(/#(\d+)/.exec(document.querySelector(`label[for="${input.id}"]`)?.textContent ?? "")?.[1]);
}

describe("backup reminder", () => {
  it("shows on the dashboard and the send screen while the backup is unverified", async () => {
    await renderApp((mock) => fundedWallet(mock));
    await waitForDashboard();
    expect(reminder()).toBeInTheDocument();
    expect(within(reminder()!).getByRole("button", { name: "Verify backup" })).toBeInTheDocument();
    expect(within(reminder()!).getByRole("button", { name: "Show the words again" })).toBeInTheDocument();

    nav("Send");
    await screen.findByRole("heading", { name: "Send" });
    expect(reminder()).toBeInTheDocument();
    nav("History");
    await screen.findByRole("heading", { name: "History" });
    expect(reminder()).toBeNull();
  });

  it("is not shown for a restored wallet (typing the phrase in was the check)", async () => {
    await renderApp(async (mock) => {
      await mock.restoreWallet(ABANDON, PASSWORD, null);
    });
    await waitForDashboard();
    expect(reminder()).toBeNull();
  });
});

describe("verify backup", () => {
  it("asks for the password, then three words; explains a mismatch; success removes the reminder", async () => {
    const { mock } = await renderApp((m) => fundedWallet(m));
    await waitForDashboard();
    const words = await paperCopy(mock);
    const verify = vi.spyOn(mock, "verifyBackup");

    fireEvent.click(within(reminder()!).getByRole("button", { name: "Verify backup" }));
    const dialog = await screen.findByRole("dialog", { name: "Verify your backup" });

    // Wrong password: caught before any word is typed.
    type(within(dialog).getByLabelText("Wallet password"), "not my password");
    fireEvent.click(within(dialog).getByRole("button", { name: "Continue" }));
    expect(await within(dialog).findByText("That password is not correct. Try again.")).toBeInTheDocument();
    expect(within(dialog).queryAllByLabelText(/^Word #/)).toHaveLength(0);

    type(within(dialog).getByLabelText("Wallet password"), PASSWORD);
    fireEvent.click(within(dialog).getByRole("button", { name: "Continue" }));
    await within(dialog).findAllByLabelText(/^Word #\d+$/);
    const inputs = wordInputs(dialog);
    expect(inputs).toHaveLength(3);
    // Typed like passwords, unless the user asks to see them.
    for (const input of inputs) {
      expect(input).toHaveAttribute("type", "password");
      expect(input).toHaveAttribute("autocomplete", "off");
      expect(input).toHaveAttribute("spellcheck", "false");
    }
    fireEvent.click(within(dialog).getByRole("checkbox", { name: "Show the words as I type" }));
    for (const input of inputs) expect(input).toHaveAttribute("type", "text");

    // One wrong word: the position is named (never a word), the dialog stays.
    for (const input of inputs) type(input, words[positionOf(input) - 1] ?? "");
    const wrong = inputs[1]!;
    const wrongPosition = positionOf(wrong);
    type(wrong, words[wrongPosition - 1] === "zoo" ? "zebra" : "zoo");
    fireEvent.click(within(dialog).getByRole("button", { name: "Check" }));
    expect(
      await within(dialog).findByText(new RegExp(`^Word #${wrongPosition} doesn't match your recovery phrase\\. Check your paper copy`)),
    ).toBeInTheDocument();
    expect(within(dialog).getByText("Doesn't match.")).toBeInTheDocument();
    expect(reminder()).toBeInTheDocument();
    expect((await mock.appInfo()).backup_verified).toBe(false);

    // Fixed (case and spaces don't matter): verified, the dialog closes, the reminder goes.
    type(wrong, `  ${(words[wrongPosition - 1] ?? "").toUpperCase()} `);
    fireEvent.click(within(dialog).getByRole("button", { name: "Check" }));
    expect(await screen.findByText(/^Backup verified/)).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(reminder()).toBeNull();
    expect((await mock.appInfo()).backup_verified).toBe(true);
    // Rust gets 1-based positions and trimmed words.
    const answers = verify.mock.calls.at(-1)?.[1] ?? [];
    expect(verify.mock.calls.at(-1)?.[0]).toBe(PASSWORD);
    expect(answers).toHaveLength(3);
    for (const [position, word] of answers) expect(word.toLowerCase()).toBe(words[position - 1]);
  });

  it("can switch to showing the words", async () => {
    await renderApp((m) => fundedWallet(m));
    await waitForDashboard();
    fireEvent.click(within(reminder()!).getByRole("button", { name: "Verify backup" }));
    const dialog = await screen.findByRole("dialog", { name: "Verify your backup" });
    type(within(dialog).getByLabelText("Wallet password"), PASSWORD);
    fireEvent.click(within(dialog).getByRole("button", { name: "Continue" }));
    fireEvent.click(await within(dialog).findByRole("button", { name: "Show the words again" }));
    expect(await screen.findByRole("dialog", { name: "Your recovery phrase" })).toBeInTheDocument();
  });
});

describe("show recovery phrase", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  async function openSettingsReveal() {
    const rendered = await renderApp((m) => fundedWallet(m));
    await waitForDashboard();
    nav("Settings");
    fireEvent.click(await screen.findByRole("button", { name: "Show recovery phrase" }));
    return rendered;
  }

  it("needs the password, keeps the words hidden until Reveal, and forgets them on leaving", async () => {
    const { mock } = await openSettingsReveal();
    const words = await paperCopy(mock);

    type(screen.getByLabelText("Wallet password"), "not my password");
    fireEvent.click(screen.getByRole("button", { name: "Continue" }));
    expect(await screen.findByText("That password is not correct. Try again.")).toBeInTheDocument();

    type(screen.getByLabelText("Wallet password"), PASSWORD);
    fireEvent.click(screen.getByRole("button", { name: "Continue" }));
    // Decrypted, with the same warning as at creation, but not on screen yet.
    expect(await screen.findByText("Anyone with these words can take your coins.")).toBeInTheDocument();
    expect(screen.queryByRole("list", { name: "Recovery phrase" })).toBeNull();
    for (const word of words) expect(screen.queryByText(word)).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Reveal the 12 words" }));
    const grid = screen.getByRole("list", { name: "Recovery phrase" });
    const shown = within(grid)
      .getAllByRole("listitem")
      .map((item) => item.querySelector(".phrase__word")?.textContent ?? "");
    expect(shown).toEqual(words);
    // Not copyable: no copy button, and copy events are cancelled.
    expect(screen.queryByRole("button", { name: /copy/i })).toBeNull();
    expect(fireEvent.copy(grid)).toBe(false);

    // Leaving the screen drops them; coming back asks for the password again.
    nav("Overview");
    await screen.findByRole("heading", { name: /balance/i });
    nav("Settings");
    await screen.findByRole("heading", { name: "Settings" });
    expect(screen.queryByRole("list", { name: "Recovery phrase" })).toBeNull();
    for (const word of words) expect(screen.queryByText(word)).toBeNull();
    expect(screen.getByRole("button", { name: "Show recovery phrase" })).toBeInTheDocument();
    expect(localStorage.length).toBe(0);
    expect(sessionStorage.length).toBe(0);
  });

  it("hides the words again after a minute", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { mock } = await openSettingsReveal();
    const words = await paperCopy(mock);
    type(screen.getByLabelText("Wallet password"), PASSWORD);
    fireEvent.click(screen.getByRole("button", { name: "Continue" }));
    fireEvent.click(await screen.findByRole("button", { name: "Reveal the 12 words" }));
    expect(screen.getByRole("list", { name: "Recovery phrase" })).toBeInTheDocument();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(60_000);
    });
    expect(screen.queryByRole("list", { name: "Recovery phrase" })).toBeNull();
    for (const word of words) expect(screen.queryByText(word)).toBeNull();
    expect(screen.getByText(/hidden again after a minute/)).toBeInTheDocument();
    expect(screen.getByLabelText("Wallet password")).toHaveValue("");
  });

  it("opens from the reminder too", async () => {
    await renderApp((m) => fundedWallet(m));
    await waitForDashboard();
    fireEvent.click(within(reminder()!).getByRole("button", { name: "Show the words again" }));
    const dialog = await screen.findByRole("dialog", { name: "Your recovery phrase" });
    type(within(dialog).getByLabelText("Wallet password"), PASSWORD);
    fireEvent.click(within(dialog).getByRole("button", { name: "Continue" }));
    fireEvent.click(await within(dialog).findByRole("button", { name: "Reveal the 12 words" }));
    expect(within(dialog).getByRole("list", { name: "Recovery phrase" })).toBeInTheDocument();
    fireEvent.click(within(dialog).getByRole("button", { name: "Close" }));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.queryByRole("list", { name: "Recovery phrase" })).toBeNull();
  });
});
