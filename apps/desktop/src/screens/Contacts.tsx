import { useCallback, useEffect, useRef, useState, type FormEvent, type KeyboardEvent, type RefObject } from "react";
import { createPortal } from "react-dom";
import { api } from "../lib/api";
import { errorText, toApiError } from "../lib/errors";
import { plural } from "../lib/format";
import { NETWORKS } from "../lib/network";
import type { Contact } from "../lib/types";
import type { Go } from "../state/nav";
import { useWallet } from "../state/wallet";
import { AddressChunks } from "../components/Address";
import { ErrorNotice } from "../components/ErrorNotice";
import { Field, NO_ASSIST } from "../components/Field";
import { ScreenHeader } from "../components/Frame";
import { Icon } from "../components/Icon";
import { Modal } from "../components/Modal";

const MAX_NAME_CHARS = 40;
const key = (name: string) => name.toLowerCase();
/** "Saved Alice B." not "Saved Alice B..": names may end in a full stop. */
const sentence = (text: string) => (/[.!?]$/.test(text) ? text : `${text}.`);

/**
 * The address book of the active network: list, add, rename, remove (with confirmation), and
 * "Pay", which opens Send with the contact's name filled in. Everything here is watch-only: no
 * password needed. Addresses are checked for the network by Rust when saved.
 */
export function Contacts({ go }: { go: Go }) {
  const { info, notify } = useWallet();
  const meta = NETWORKS[info.network];
  const [contacts, setContacts] = useState<Contact[] | null>(null);
  const [loadError, setLoadError] = useState<unknown>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [removing, setRemoving] = useState<Contact | null>(null);
  // After a rename or a removal, where keyboard focus goes (the list re-renders).
  const renameButtons = useRef(new Map<string, HTMLButtonElement>());
  const focusAfter = useRef<string | null>(null);
  const addName = useRef<HTMLInputElement>(null);

  const load = useCallback(async () => {
    try {
      setContacts(await api.listContacts());
      setLoadError(null);
    } catch (e) {
      setLoadError(e);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    if (focusAfter.current === null) return;
    const target = renameButtons.current.get(focusAfter.current);
    focusAfter.current = null;
    (target ?? addName.current)?.focus();
  });

  return (
    <div className="screen">
      <ScreenHeader
        title="Contacts"
        lead={
          contacts && contacts.length > 0
            ? `${plural(contacts.length, "contact", "contacts")} on ${meta.label}. Pay them by name on the Send screen.`
            : `People you pay often, saved with their ${meta.label} address.`
        }
      />
      {loadError !== null && !contacts && <ErrorNotice error={loadError} network={info.network} />}

      {contacts && contacts.length === 0 && (
        <div className="empty">
          <p>No contacts yet.</p>
          <p className="muted">
            Save an address you pay often, then type the name on the Send screen. The full address is always shown before
            you confirm a payment.
          </p>
        </div>
      )}

      {contacts && contacts.length > 0 && (
        <ul className="contactlist" aria-label="Contacts">
          {contacts.map((c) => (
            <li key={key(c.name)} className="contact">
              <div className="contact__main">
                <p className="contact__name">{c.name}</p>
                <p className="contact__addr">
                  <AddressChunks address={c.address} />
                </p>
                {c.note && <p className="contact__note">{c.note}</p>}
              </div>
              {renaming === key(c.name) ? (
                <RenameForm
                  contact={c}
                  onDone={async (renamed) => {
                    setRenaming(null);
                    focusAfter.current = key(renamed?.name ?? c.name);
                    if (renamed) {
                      await load();
                      notify("success", sentence(`Renamed ${c.name} to ${renamed.name}`));
                    }
                  }}
                />
              ) : (
                <div className="contact__actions">
                  <button
                    type="button"
                    className="btn btn--quiet"
                    aria-label={`Pay ${c.name}`}
                    onClick={() => go({ name: "send", to: c.name })}
                  >
                    <Icon name="out" size={16} />
                    Pay
                  </button>
                  <button
                    type="button"
                    className="btn btn--ghost"
                    aria-label={`Rename ${c.name}`}
                    ref={(node) => {
                      if (node) renameButtons.current.set(key(c.name), node);
                      else renameButtons.current.delete(key(c.name));
                    }}
                    onClick={() => setRenaming(key(c.name))}
                  >
                    Rename
                  </button>
                  <button
                    type="button"
                    className="btn btn--ghost btn--danger-text"
                    aria-label={`Remove ${c.name}`}
                    onClick={() => setRemoving(c)}
                  >
                    Remove
                  </button>
                </div>
              )}
            </li>
          ))}
        </ul>
      )}

      <AddContactForm
        nameRef={addName}
        placeholder={`${meta.addressPrefix}q…`}
        onAdded={async (contact) => {
          await load();
          notify("success", sentence(`Saved ${contact.name}`));
        }}
      />

      {removing && (
        <RemoveDialog
          contact={removing}
          onClose={async (removed) => {
            setRemoving(null);
            if (!removed) return;
            focusAfter.current = ""; // its row is gone: back to the add form
            await load();
            notify("info", `Removed ${removed.name} from your contacts.`);
          }}
        />
      )}
    </div>
  );
}

type AddErrors = { name: string | null; address: string | null; note: string | null };
const NO_ERRORS: AddErrors = { name: null, address: null, note: null };

function AddContactForm({
  nameRef,
  placeholder,
  onAdded,
}: {
  nameRef: RefObject<HTMLInputElement | null>;
  placeholder: string;
  onAdded: (contact: Contact) => Promise<void>;
}) {
  const { info } = useWallet();
  const [name, setName] = useState("");
  const [address, setAddress] = useState("");
  const [note, setNote] = useState("");
  const [errors, setErrors] = useState<AddErrors>(NO_ERRORS);
  const [formError, setFormError] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (busy) return;
    const next: AddErrors = { ...NO_ERRORS };
    if (name.trim() === "") next.name = "Enter a name.";
    else if ([...name.trim()].length > MAX_NAME_CHARS) next.name = `A name can be at most ${MAX_NAME_CHARS} characters.`;
    if (address.trim() === "") next.address = "Enter the contact's address.";
    else if (/\s/.test(address.trim())) next.address = "An address has no spaces. Copy it again from the source.";
    setErrors(next);
    setFormError(null);
    if (next.name || next.address) return;
    setBusy(true);
    try {
      const saved = await api.addContact(name, address.trim(), note.trim() === "" ? null : note);
      setName("");
      setAddress("");
      setNote("");
      setBusy(false);
      await onAdded(saved);
    } catch (err) {
      setBusy(false);
      const { code, message } = toApiError(err);
      const text = errorText(err, { network: info.network });
      if (code === "invalid_address" || code === "network_mismatch") setErrors((x) => ({ ...x, address: text }));
      else if (code === "contact" && /^a note/.test(message)) setErrors((x) => ({ ...x, note: text }));
      else if (code === "contact") setErrors((x) => ({ ...x, name: text }));
      else setFormError(err);
    }
  }

  return (
    <form className="panel stack" onSubmit={submit} noValidate aria-labelledby="add-contact-title">
      <h2 className="panel__legend" id="add-contact-title">
        Add a contact
      </h2>
      <Field
        ref={nameRef}
        label="Name"
        value={name}
        onChange={(e) => {
          setName(e.target.value);
          setErrors((x) => ({ ...x, name: null }));
        }}
        error={errors.name}
        hint="Up to 40 characters. It can't look like an address, so a recipient is always clearly one or the other."
        disabled={busy}
        {...NO_ASSIST}
      />
      <Field
        label="Address"
        value={address}
        onChange={(e) => {
          setAddress(e.target.value);
          setErrors((x) => ({ ...x, address: null }));
        }}
        error={errors.address}
        placeholder={placeholder}
        mono
        disabled={busy}
        {...NO_ASSIST}
      />
      <Field
        label="Note (optional)"
        value={note}
        onChange={(e) => {
          setNote(e.target.value);
          setErrors((x) => ({ ...x, note: null }));
        }}
        error={errors.note}
        hint="Only for you, e.g. what you usually pay them for."
        disabled={busy}
        {...NO_ASSIST}
      />
      <ErrorNotice error={formError} network={info.network} />
      <div className="actions">
        <button type="submit" className="btn btn--primary" disabled={busy}>
          {busy ? "Saving…" : "Save contact"}
        </button>
      </div>
    </form>
  );
}

function RenameForm({ contact, onDone }: { contact: Contact; onDone: (renamed: Contact | null) => void }) {
  const { info } = useWallet();
  const [name, setName] = useState(contact.name);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const input = useRef<HTMLInputElement>(null);

  useEffect(() => {
    input.current?.focus();
    input.current?.select();
  }, []);

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (busy) return;
    if (name.trim() === "") {
      setError("Enter a name.");
      return;
    }
    if (name.trim() === contact.name) {
      onDone(null);
      return;
    }
    setBusy(true);
    try {
      onDone(await api.renameContact(contact.name, name));
    } catch (err) {
      setBusy(false);
      setError(errorText(err, { network: info.network }));
    }
  }

  function onKeyDown(e: KeyboardEvent<HTMLFormElement>) {
    if (e.key === "Escape") {
      e.preventDefault();
      onDone(null);
    }
  }

  return (
    <form className="contact__rename" onSubmit={submit} onKeyDown={onKeyDown} noValidate>
      <Field
        ref={input}
        label={`New name for ${contact.name}`}
        value={name}
        onChange={(e) => {
          setName(e.target.value);
          setError(null);
        }}
        error={error}
        disabled={busy}
        {...NO_ASSIST}
      />
      <div className="actions">
        <button type="button" className="btn btn--ghost" onClick={() => onDone(null)} disabled={busy}>
          Cancel
        </button>
        <button type="submit" className="btn btn--primary" disabled={busy}>
          {busy ? "Saving…" : "Save name"}
        </button>
      </div>
    </form>
  );
}

function RemoveDialog({ contact, onClose }: { contact: Contact; onClose: (removed: Contact | null) => void }) {
  const { info } = useWallet();
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);

  async function remove() {
    if (busy) return;
    setBusy(true);
    try {
      onClose(await api.removeContact(contact.name));
    } catch (err) {
      setBusy(false);
      // Already gone (removed in a terminal, say): nothing left to do but refresh.
      if (toApiError(err).code === "contact") onClose(contact);
      else setError(err);
    }
  }

  return createPortal(
    <Modal title={`Remove ${contact.name}?`} onClose={() => onClose(null)}>
      <div className="stack">
        <p className="muted">
          This deletes the name and address from your address book. Payments you already made are not affected.
        </p>
        <p className="mono">
          <AddressChunks address={contact.address} />
        </p>
        <ErrorNotice error={error} network={info.network} />
        <div className="actions">
          <button type="button" className="btn btn--ghost" onClick={() => onClose(null)} data-autofocus disabled={busy}>
            Cancel
          </button>
          <button type="button" className="btn btn--danger" onClick={() => void remove()} disabled={busy}>
            {busy ? "Removing…" : "Remove contact"}
          </button>
        </div>
      </div>
    </Modal>,
    document.body,
  );
}
