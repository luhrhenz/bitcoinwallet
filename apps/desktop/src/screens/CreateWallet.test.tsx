import { fireEvent, screen, within } from "@testing-library/react";
import { PASSWORD, renderApp, type, waitForDashboard } from "../test/helpers";

async function startCreate() {
  fireEvent.click(await screen.findByRole("button", { name: /create a new wallet/i }));
}

function fillPasswords(password: string, confirm: string) {
  type(screen.getByLabelText("New wallet password"), password);
  type(screen.getByLabelText("Repeat the password"), confirm);
}

describe("create wallet", () => {
  it("shows the phrase once, requires the backup check, then forgets the words", async () => {
    const { mock } = await renderApp();
    const create = vi.spyOn(mock, "createWallet");
    await startCreate();
    fillPasswords(PASSWORD, PASSWORD);
    fireEvent.click(screen.getByRole("button", { name: "Create wallet" }));

    // Backup: hidden until asked for, with the warning, then a numbered grid of 12 words.
    fireEvent.click(await screen.findByRole("button", { name: "Show the 12 words" }));
    expect(screen.getByText("Anyone with these words can take your coins.")).toBeInTheDocument();
    const grid = screen.getByRole("list", { name: "Recovery phrase" });
    const words = within(grid)
      .getAllByRole("listitem")
      .map((item) => item.querySelector(".phrase__word")?.textContent ?? "");
    expect(words).toHaveLength(12);
    expect(create).toHaveBeenCalledWith(12, PASSWORD);

    // Can't continue until the user says the words are written down.
    const next = screen.getByRole("button", { name: "Continue to the check" });
    expect(next).toBeDisabled();
    fireEvent.click(screen.getByRole("checkbox", { name: /written all 12 words/i }));
    fireEvent.click(next);

    // Verify: three positions; a wrong word keeps the user here.
    const inputs = screen.getAllByLabelText(/^Word #\d+$/);
    expect(inputs).toHaveLength(3);
    const positionOf = (input: HTMLElement) =>
      Number(/#(\d+)/.exec(document.querySelector(`label[for="${input.id}"]`)?.textContent ?? "")?.[1]) - 1;
    for (const input of inputs) {
      expect(input).toHaveAttribute("autocomplete", "off");
      expect(input).toHaveAttribute("spellcheck", "false");
      type(input, words[positionOf(input)] ?? "");
    }
    const wrong = inputs[1]!;
    type(wrong, "zzzz");
    fireEvent.click(screen.getByRole("button", { name: "Check and finish" }));
    expect(await screen.findByText(/doesn't match\. Check your paper copy/)).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Check your backup" })).toBeInTheDocument();

    // Correct (case and spaces don't matter) → the wallet opens.
    type(wrong, `  ${(words[positionOf(wrong)] ?? "").toUpperCase()} `);
    fireEvent.click(screen.getByRole("button", { name: "Check and finish" }));
    await waitForDashboard();

    // The words are gone: not on screen, not in storage, not in the URL.
    expect(screen.queryByRole("list", { name: "Recovery phrase" })).toBeNull();
    for (const word of words) expect(screen.queryByText(word)).toBeNull();
    expect(localStorage.length).toBe(0);
    expect(sessionStorage.length).toBe(0);
    for (const word of words) expect(window.location.href).not.toContain(word);
  });

  it("can show the words again from the check", async () => {
    await renderApp();
    await startCreate();
    fillPasswords(PASSWORD, PASSWORD);
    fireEvent.click(screen.getByRole("button", { name: "Create wallet" }));
    fireEvent.click(await screen.findByRole("button", { name: "Show the 12 words" }));
    fireEvent.click(screen.getByRole("checkbox"));
    fireEvent.click(screen.getByRole("button", { name: "Continue to the check" }));
    fireEvent.click(screen.getByRole("button", { name: "Show the words again" }));
    expect(screen.getByRole("heading", { name: "Write down your recovery phrase" })).toBeInTheDocument();
  });

  it("explains short and mismatched passwords before calling the backend", async () => {
    const { mock } = await renderApp();
    const create = vi.spyOn(mock, "createWallet");
    await startCreate();

    fillPasswords("short", "short");
    fireEvent.click(screen.getByRole("button", { name: "Create wallet" }));
    expect(screen.getByText("Use at least 8 characters.")).toBeInTheDocument();

    fillPasswords(PASSWORD, `${PASSWORD}!`);
    fireEvent.click(screen.getByRole("button", { name: "Create wallet" }));
    expect(screen.getByText("The passwords don't match.")).toBeInTheDocument();
    expect(screen.queryByText("Use at least 8 characters.")).toBeNull();
    expect(create).not.toHaveBeenCalled();
  });

  it("shows the backend's weak_password error in plain words", async () => {
    const { mock } = await renderApp();
    vi.spyOn(mock, "createWallet").mockRejectedValueOnce({
      code: "weak_password",
      message: "password must be at least 8 characters",
    });
    await startCreate();
    fillPasswords(PASSWORD, PASSWORD);
    fireEvent.click(screen.getByRole("button", { name: "Create wallet" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Choose a password with at least 8 characters.");
  });
});
