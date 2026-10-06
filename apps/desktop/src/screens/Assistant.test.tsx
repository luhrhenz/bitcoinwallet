import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import type { MockWalletApi } from "../lib/mock";
import { fundedWallet, nav, PASSWORD, renderApp, TB_ADDRESS, type, waitForDashboard } from "../test/helpers";

const KEY = "gsk_super_secret_key_123";

async function enableAssistant(mock: MockWalletApi, key: string | null = KEY) {
  await mock.setAssistantSettings({
    enabled: true,
    provider: "groq",
    base_url: "https://api.groq.com/openai/v1",
    model: "llama-3.3-70b-versatile",
    api_key: key,
    clear_api_key: false,
    live_price: false,
    consent: true,
  });
}

async function openAssistant(options: { unlocked?: boolean; enabled?: boolean; key?: string | null } = {}) {
  const rendered = await renderApp(async (mock) => {
    await fundedWallet(mock, { unlocked: options.unlocked ?? true });
    if (options.enabled ?? true) await enableAssistant(mock, options.key === undefined ? KEY : options.key);
  });
  await waitForDashboard();
  nav("Assistant");
  await screen.findByRole("heading", { name: "Assistant" });
  return rendered;
}

function ask(text: string) {
  type(screen.getByLabelText("Message"), text);
  fireEvent.click(screen.getByRole("button", { name: "Ask" }));
}

describe("assistant screen", () => {
  it("is off until set up, and points to Settings", async () => {
    await openAssistant({ enabled: false });
    expect(screen.getByText("The assistant is off.")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Set up the assistant" }));
    expect(await screen.findByRole("heading", { name: "Settings" })).toBeInTheDocument();
    expect(screen.getByRole("form", { name: "Assistant" })).toBeInTheDocument();
  });

  it("answers questions and shows which tools it used", async () => {
    await openAssistant();
    ask("What's my balance?");
    const conversation = screen.getByRole("region", { name: "Conversation" });
    expect(await within(conversation).findByText(/You have 0\.01000000 BTC/)).toBeInTheDocument();
    expect(within(conversation).getByText("What's my balance?")).toBeInTheDocument();
    expect(within(conversation).getByText("Looked at: balance")).toBeInTheDocument();
    expect(screen.getByLabelText("Message")).toHaveValue("");
  });

  it("shows a missing key as a friendly error", async () => {
    await openAssistant({ key: null });
    expect(screen.getByText("Add your Groq API key in Settings → Assistant.")).toBeInTheDocument();
    ask("balance");
    expect(await screen.findByRole("alert")).toHaveTextContent("Add your Groq API key in Settings → Assistant.");
    // The text stays, to send again.
    expect(screen.getByLabelText("Message")).toHaveValue("balance");
  });

  it("explains a free-tier rate limit", async () => {
    await openAssistant({ key: "ratelimited" });
    ask("balance");
    expect(await screen.findByRole("alert")).toHaveTextContent("free tier is busy");
  });

  it("prepares a payment card that only the user can confirm", async () => {
    const { mock } = await openAssistant();
    const confirm = vi.spyOn(mock, "confirmSend");
    ask(`pay 5000 sat to ${TB_ADDRESS}`);
    const card = await screen.findByRole("region", { name: "Payment preview" });
    expect(within(card).getByText("Total leaving the wallet")).toBeInTheDocument();
    expect(confirm).not.toHaveBeenCalled();

    fireEvent.click(within(card).getByRole("button", { name: "Send 0.00005000 BTC" }));
    expect(await screen.findByText("Sent.")).toBeInTheDocument();
    expect(confirm).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "View transaction" }));
    expect(await screen.findByRole("heading", { name: /sent/i })).toBeInTheDocument();
  });

  it("cancels a card, and a newer card replaces an older one", async () => {
    const { mock } = await openAssistant();
    const cancel = vi.spyOn(mock, "cancelSend");
    ask(`pay 5000 sat to ${TB_ADDRESS}`);
    await screen.findByRole("region", { name: "Payment preview" });
    ask(`pay 6000 sat to ${TB_ADDRESS}`);
    await screen.findByText("Replaced by a newer preview. Nothing was sent.");
    expect(screen.getAllByRole("region", { name: "Payment preview" })).toHaveLength(1);

    fireEvent.click(within(screen.getByRole("region", { name: "Payment preview" })).getByRole("button", { name: "Cancel" }));
    expect(await screen.findByText("Cancelled. Nothing was sent.")).toBeInTheDocument();
    expect(cancel).toHaveBeenCalledTimes(1);
  });

  it("asks for the password when the wallet was locked, then shows the preview", async () => {
    const { mock } = await openAssistant({ unlocked: false });
    const prepare = vi.spyOn(mock, "prepareSend");
    ask(`pay 5000 sat to ${TB_ADDRESS}`);
    const locked = await screen.findByRole("region", { name: "Locked payment request" });
    fireEvent.click(within(locked).getByRole("button", { name: "Unlock and review" }));
    const dialog = await screen.findByRole("dialog", { name: "Unlock wallet" });
    type(within(dialog).getByLabelText("Wallet password"), PASSWORD);
    fireEvent.click(within(dialog).getByRole("button", { name: "Unlock" }));
    expect(await screen.findByRole("region", { name: "Payment preview" })).toBeInTheDocument();
    expect(prepare).toHaveBeenCalledWith(TB_ADDRESS, 5000, null);
  });

  it("clears the conversation when the wallet locks", async () => {
    await openAssistant();
    ask("balance");
    await screen.findByText(/You have/);
    fireEvent.click(screen.getByRole("button", { name: /Lock$/ }));
    await waitFor(() => expect(screen.queryByText(/You have/)).not.toBeInTheDocument());
  });
});

describe("assistant settings", () => {
  async function openSettings(setup?: (mock: MockWalletApi) => Promise<void>) {
    const rendered = await renderApp(async (mock) => {
      await fundedWallet(mock);
      await setup?.(mock);
    });
    await waitForDashboard();
    nav("Settings");
    const form = await screen.findByRole("form", { name: "Assistant" });
    await within(form).findByLabelText("API key");
    return { ...rendered, form: within(form) };
  }

  it("asks for consent once before turning on", async () => {
    const { mock, form } = await openSettings();
    const save = vi.spyOn(mock, "setAssistantSettings");
    fireEvent.click(form.getByLabelText("Turn on the assistant"));
    expect(form.getByRole("note", { name: "Data notice" })).toHaveTextContent("balance, transaction history, addresses and contact names");
    fireEvent.click(form.getByRole("button", { name: "Save assistant settings" }));
    expect(form.getByText("Tick the box to agree before turning the assistant on.")).toBeInTheDocument();
    expect(save).not.toHaveBeenCalled();

    fireEvent.click(form.getByLabelText("I understand and agree"));
    fireEvent.click(form.getByRole("button", { name: "Save assistant settings" }));
    expect(await screen.findByText("Assistant settings saved.")).toBeInTheDocument();
    expect(save).toHaveBeenLastCalledWith(expect.objectContaining({ enabled: true, consent: true }));
    // Given once: no notice again.
    expect(form.queryByRole("note", { name: "Data notice" })).not.toBeInTheDocument();
  });

  it("keeps the API key write-only", async () => {
    const { mock, form } = await openSettings();
    type(form.getByLabelText("API key"), KEY);
    fireEvent.click(form.getByRole("button", { name: "Save assistant settings" }));
    await screen.findByText("Assistant settings saved.");
    // The field empties and the key never comes back from the backend.
    expect(form.getByLabelText("API key")).toHaveValue("");
    expect(form.getByLabelText("API key")).toHaveAttribute("type", "password");
    expect(form.getByText("A key is saved.")).toBeInTheDocument();
    const view = await mock.getAssistantSettings();
    expect(JSON.stringify(view)).not.toContain(KEY);
    expect(view.has_api_key).toBe(true);
    expect(document.body.innerHTML).not.toContain(KEY);

    fireEvent.click(form.getByRole("button", { name: "Remove key" }));
    await screen.findByText("API key removed.");
    expect((await mock.getAssistantSettings()).has_api_key).toBe(false);
  });

  it("fills the preset's URL and model, and refuses plain http", async () => {
    const { form } = await openSettings();
    fireEvent.click(form.getByLabelText("Google Gemini"));
    expect(form.getByLabelText("Provider URL")).toHaveValue("https://generativelanguage.googleapis.com/v1beta/openai");
    expect(form.getByLabelText("Model")).toHaveValue("gemini-2.5-flash");
    fireEvent.click(form.getByLabelText("Custom"));
    type(form.getByLabelText("Provider URL"), "http://llm.example.com/v1");
    fireEvent.click(form.getByRole("button", { name: "Save assistant settings" }));
    expect(await form.findByRole("alert")).toHaveTextContent("https://");
  });
});
