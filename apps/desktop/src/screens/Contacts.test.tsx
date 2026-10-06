import { fireEvent, screen, within } from "@testing-library/react";
import { BCRT_ADDRESS, fundedWallet, nav, renderApp, TB_ADDRESS, type, waitForDashboard } from "../test/helpers";

async function openContacts(setup?: Parameters<typeof renderApp>[0]) {
  const rendered = await renderApp(async (mock) => {
    await fundedWallet(mock); // locked: the address book needs no password
    await setup?.(mock);
  });
  await waitForDashboard();
  nav("Contacts");
  await screen.findByRole("heading", { name: "Contacts" });
  return rendered;
}

function addForm() {
  return within(screen.getByRole("form", { name: "Add a contact" }));
}

function fillAdd(name: string, address: string, note = "") {
  const form = addForm();
  type(form.getByLabelText("Name"), name);
  type(form.getByLabelText("Address"), address);
  type(form.getByLabelText("Note (optional)"), note);
  fireEvent.click(form.getByRole("button", { name: "Save contact" }));
}

describe("contacts screen", () => {
  it("adds contacts with the core's rules, while locked", async () => {
    const { mock } = await openContacts();
    expect(await screen.findByText("No contacts yet.")).toBeInTheDocument();
    const add = vi.spyOn(mock, "addContact");

    // Checked here first: nothing reaches the backend.
    fireEvent.click(addForm().getByRole("button", { name: "Save contact" }));
    expect(addForm().getByText("Enter a name.")).toBeInTheDocument();
    expect(addForm().getByText("Enter the contact's address.")).toBeInTheDocument();
    expect(add).not.toHaveBeenCalled();

    fillAdd("  Alice ", TB_ADDRESS, "rent");
    expect(await screen.findByText("Saved Alice.")).toBeInTheDocument();
    const list = screen.getByRole("list", { name: "Contacts" });
    expect(list).toHaveTextContent("Alice");
    expect(list).toHaveTextContent(TB_ADDRESS);
    expect(list).toHaveTextContent("rent");
    expect(addForm().getByLabelText("Name")).toHaveValue("");

    // The backend's refusals land on the right field.
    fillAdd("ALICE", TB_ADDRESS);
    expect(
      await addForm().findByText("That name, note or label can't be used. (A contact named `Alice` already exists)"),
    ).toBeInTheDocument();
    fillAdd("Bob", BCRT_ADDRESS);
    expect(
      await addForm().findByText("That address is for a different network. On testnet4, addresses start with tb1."),
    ).toBeInTheDocument();
    fillAdd("tb1qbob", TB_ADDRESS);
    expect(await addForm().findByText(/looks like a Bitcoin address/)).toBeInTheDocument();
    fillAdd("Bob", TB_ADDRESS);
    await screen.findByText("Saved Bob.");
    expect(within(screen.getByRole("list", { name: "Contacts" })).getAllByRole("listitem")).toHaveLength(2);

  });

  it("renames and removes contacts, with confirmation, while locked", async () => {
    const { mock } = await openContacts(async (m) => {
      await m.addContact("Alice", TB_ADDRESS, "rent");
      await m.addContact("Bob", TB_ADDRESS, null);
    });
    await screen.findByRole("list", { name: "Contacts" });

    // Rename: Escape cancels and puts focus back; a taken name is refused; then it works.
    fireEvent.click(screen.getByRole("button", { name: "Rename Alice" }));
    const field = screen.getByLabelText("New name for Alice");
    expect(field).toHaveFocus();
    fireEvent.keyDown(field, { key: "Escape" });
    expect(screen.queryByLabelText("New name for Alice")).toBeNull();
    expect(screen.getByRole("button", { name: "Rename Alice" })).toHaveFocus();

    fireEvent.click(screen.getByRole("button", { name: "Rename Alice" }));
    type(screen.getByLabelText("New name for Alice"), "bob");
    fireEvent.click(screen.getByRole("button", { name: "Save name" }));
    expect(await screen.findByText(/A contact named `Bob` already exists/)).toBeInTheDocument();
    type(screen.getByLabelText("New name for Alice"), "Alice B.");
    fireEvent.click(screen.getByRole("button", { name: "Save name" }));
    expect(await screen.findByText("Renamed Alice to Alice B.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Rename Alice B." })).toHaveFocus();
    expect((await mock.listContacts()).map((c) => c.name)).toEqual(["Alice B.", "Bob"]);

    // Remove asks first; Cancel keeps it.
    fireEvent.click(screen.getByRole("button", { name: "Remove Bob" }));
    let dialog = await screen.findByRole("dialog", { name: "Remove Bob?" });
    expect(within(dialog).getByRole("button", { name: "Cancel" })).toHaveFocus();
    fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect((await mock.listContacts()).length).toBe(2);

    fireEvent.click(screen.getByRole("button", { name: "Remove Bob" }));
    dialog = await screen.findByRole("dialog", { name: "Remove Bob?" });
    expect(dialog).toHaveTextContent(TB_ADDRESS);
    fireEvent.click(within(dialog).getByRole("button", { name: "Remove contact" }));
    expect(await screen.findByText("Removed Bob from your contacts.")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Remove Alice B." }));
    fireEvent.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Remove contact" }));
    expect(await screen.findByText("No contacts yet.")).toBeInTheDocument();
    expect(await mock.listContacts()).toEqual([]);
    // All of it watch-only: still locked.
    expect((await mock.appInfo()).unlocked).toBe(false);
  });

  it("opens Send with the contact's name filled in", async () => {
    await openContacts((mock) => mock.addContact("Alice", TB_ADDRESS, null).then(() => undefined));
    fireEvent.click(await screen.findByRole("button", { name: "Pay Alice" }));
    await screen.findByRole("heading", { name: "Send" });
    expect(screen.getByLabelText("Recipient")).toHaveValue("Alice");
    // The hint already shows where the money goes.
    expect(screen.getByLabelText("Recipient")).toHaveAccessibleDescription(new RegExp(`^Contact Alice:\\s*${TB_ADDRESS}$`));
  });
});
