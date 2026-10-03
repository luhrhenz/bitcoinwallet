import { useState, type FormEvent } from "react";
import { describeError } from "../lib/errors";
import { Field, NO_ASSIST } from "./Field";
import { Modal } from "./Modal";

/**
 * Asks for the wallet password. The password goes straight to the `unlock` command (Rust
 * decrypts the keystore and keeps the keys); the UI keeps nothing once the dialog closes.
 */
export function UnlockDialog({
  onUnlock,
  onDone,
}: {
  onUnlock: (password: string) => Promise<void>;
  onDone: (ok: boolean) => void;
}) {
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (busy) return;
    if (password === "") {
      setError("Enter your wallet password.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await onUnlock(password);
      setPassword("");
      onDone(true);
    } catch (err) {
      setBusy(false);
      setError(describeError(err).title);
    }
  }

  return (
    <Modal title="Unlock wallet" onClose={() => onDone(false)}>
      <form onSubmit={submit} className="stack">
        <p className="muted">
          Your password decrypts the keys on this computer so the wallet can sign. Viewing your balance and
          history doesn&apos;t need it.
        </p>
        <Field
          label="Wallet password"
          type="password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          error={error}
          data-autofocus
          readOnly={busy}
          {...NO_ASSIST}
        />
        <div className="actions">
          <button type="button" className="btn btn--ghost" onClick={() => onDone(false)}>
            Cancel
          </button>
          <button type="submit" className="btn btn--primary" disabled={busy}>
            {busy ? "Unlocking…" : "Unlock"}
          </button>
        </div>
      </form>
    </Modal>
  );
}
