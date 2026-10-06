import { useId, useRef, useState, type KeyboardEvent } from "react";
import { shortId } from "../lib/format";
import type { Contact } from "../lib/types";
import { AddressChunks } from "./Address";
import { Field, NO_ASSIST } from "./Field";
import { Icon } from "./Icon";

/** The saved contact whose name is `text` (ignoring case and surrounding spaces), if any. */
export function contactNamed(contacts: readonly Contact[], text: string): Contact | undefined {
  const wanted = text.trim().toLowerCase();
  return wanted === "" ? undefined : contacts.find((c) => c.name.toLowerCase() === wanted);
}

/**
 * The Send screen's recipient: an address, or the name of a contact (WAI-ARIA combobox with a
 * list of matching contacts; ↓/↑ to move, Enter to pick, Escape to close). The "Contacts" button
 * lists every contact. Once the text is a contact's name, the hint shows that contact's full
 * address, so the user sees where the money goes before the review, too.
 */
export function RecipientField({
  value,
  onChange,
  contacts,
  error,
  disabled,
  placeholder,
  hint,
}: {
  value: string;
  onChange: (text: string) => void;
  contacts: readonly Contact[];
  error: string | null;
  disabled: boolean;
  placeholder: string;
  hint: string;
}) {
  const listId = useId();
  const input = useRef<HTMLInputElement>(null);
  const [open, setOpen] = useState(false);
  const [showAll, setShowAll] = useState(false);
  const [active, setActive] = useState(-1);

  const query = value.trim().toLowerCase();
  const chosen = contactNamed(contacts, value);
  const matches = showAll ? [...contacts] : query === "" || chosen ? [] : contacts.filter((c) => c.name.toLowerCase().includes(query));
  const expanded = open && !disabled && matches.length > 0;

  function close() {
    setOpen(false);
    setShowAll(false);
    setActive(-1);
  }

  function pick(contact: Contact) {
    onChange(contact.name);
    close();
    input.current?.focus();
  }

  function onKeyDown(e: KeyboardEvent<HTMLInputElement>) {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      if (contacts.length === 0) return;
      e.preventDefault();
      if (!expanded) {
        // Nothing typed yet: ↓ opens the whole list.
        if (matches.length === 0) setShowAll(true);
        setOpen(true);
        setActive(0);
        return;
      }
      setActive((i) => (e.key === "ArrowDown" ? Math.min(i + 1, matches.length - 1) : Math.max(i - 1, 0)));
    } else if (e.key === "Enter" && expanded && active >= 0 && matches[active]) {
      e.preventDefault(); // pick, don't submit the form
      pick(matches[active]);
    } else if (e.key === "Escape" && expanded) {
      e.preventDefault();
      close();
    }
  }

  const optionId = (i: number) => `${listId}-${i}`;

  return (
    <div className="recipient">
      <Field
        ref={input}
        label="Recipient"
        value={value}
        onChange={(e) => {
          onChange(e.target.value);
          setShowAll(false);
          setOpen(true);
          setActive(-1);
        }}
        onKeyDown={onKeyDown}
        onBlur={close}
        error={error}
        placeholder={placeholder}
        mono
        disabled={disabled}
        role="combobox"
        aria-autocomplete="list"
        aria-expanded={expanded}
        aria-controls={listId}
        aria-activedescendant={expanded && active >= 0 ? optionId(active) : undefined}
        {...NO_ASSIST}
        hint={
          chosen ? (
            <span className="recipient__match">
              <span>
                <Icon name="contact" size={14} /> Contact <strong>{chosen.name}</strong>:
              </span>
              <AddressChunks address={chosen.address} />
            </span>
          ) : (
            hint
          )
        }
        addon={
          contacts.length > 0 ? (
            <button
              type="button"
              className="btn btn--quiet"
              aria-haspopup="listbox"
              aria-expanded={expanded}
              aria-controls={listId}
              disabled={disabled}
              // Keep focus in the input, where the arrow keys work.
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => {
                if (expanded) {
                  close();
                } else {
                  setShowAll(true);
                  setOpen(true);
                  setActive(-1);
                }
                input.current?.focus();
              }}
            >
              <Icon name="contact" size={16} />
              Contacts
            </button>
          ) : undefined
        }
      />
      <ul className="suggest" id={listId} role="listbox" aria-label="Contacts" hidden={!expanded}>
        {expanded &&
          matches.map((c, i) => (
            <li
              key={c.name}
              id={optionId(i)}
              role="option"
              aria-selected={i === active}
              className="suggest__option"
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => pick(c)}
            >
              <span className="suggest__name">{c.name}</span>
              <span className="suggest__addr mono">{shortId(c.address, 10, 8)}</span>
            </li>
          ))}
      </ul>
    </div>
  );
}
