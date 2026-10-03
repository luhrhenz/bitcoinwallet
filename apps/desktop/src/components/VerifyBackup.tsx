import { useState, type FormEvent } from "react";
import { createPortal } from "react-dom";
import { api } from "../lib/api";
import { describeError, listPositions, mismatchPositions, toApiError } from "../lib/errors";
import { useWallet } from "../state/wallet";
import { Field, NO_ASSIST } from "./Field";
import { Modal } from "./Modal";
import { RevealPhrase } from "./RevealPhrase";

/**
 * "Verify backup": password, then three words from the user's paper copy at positions Rust picks.
 * The words are typed like passwords (anyone watching the screen would learn part of the
 * phrase), with a toggle to show them. On success Rust marks the backup verified.
 *
 * The password stays in this dialog's state between the two steps (both commands need it)
 * and is gone when the dialog closes.
 */
export function VerifyBackupDialog({ onClose, onShowWords }: { onClose: () => void; onShowWords: () => void }) {
  const { refreshInfo, notify } = useWallet();
  const [password, setPassword] = useState("");
  const [positions, setPositions] = useState<number[] | null>(null);
  const [answers, setAnswers] = useState<Record<number, string>>({});
  const [show, setShow] = useState(false);
  const [wrong, setWrong] = useState<number[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function start(e: FormEvent) {
    e.preventDefault();
    if (busy) return;
    if (password === "") {
      setError("Enter your wallet password.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      setPositions(await api.backupChallenge(password));
    } catch (err) {
      setError(describeError(err).title);
    } finally {
      setBusy(false);
    }
  }

  async function check(e: FormEvent) {
    e.preventDefault();
    if (busy || !positions) return;
    if (positions.some((p) => (answers[p] ?? "").trim() === "")) {
      setError(`Fill in all ${positions.length} words.`);
      return;
    }
    setBusy(true);
    setError(null);
    setWrong([]);
    try {
      await api.verifyBackup(
        password,
        positions.map((p): [number, string] => [p, (answers[p] ?? "").trim()]),
      );
      setAnswers({});
      setPassword("");
      await refreshInfo().catch(() => undefined);
      notify("success", "Backup verified: your copy of the recovery phrase is correct. Keep it somewhere safe and private.");
      onClose();
    } catch (err) {
      setBusy(false);
      const { code, message } = toApiError(err);
      if (code === "backup_mismatch") {
        const bad = mismatchPositions(message);
        setWrong(bad);
        setError(
          `${bad.length === 1 ? "Word" : "Words"} ${listPositions(bad)} ${bad.length === 1 ? "doesn't" : "don't"} match your recovery phrase. ` +
            "Check your paper copy. If it says something else there, the copy needs fixing: show the words again and correct it.",
        );
      } else {
        setError(describeError(err).title);
      }
    }
  }

  return createPortal(
    <Modal title="Verify your backup" onClose={onClose}>
      {positions === null ? (
        <form className="stack" onSubmit={start} noValidate>
          <p className="muted">
            Get your paper copy of the recovery phrase. btcw will ask for three of its words to make sure the copy is
            complete and in the right order.
          </p>
          <Field
            label="Wallet password"
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            error={error}
            readOnly={busy}
            data-autofocus
            {...NO_ASSIST}
          />
          <div className="actions">
            <button type="button" className="btn btn--ghost" onClick={onClose}>
              Cancel
            </button>
            <button type="submit" className="btn btn--primary" disabled={busy}>
              {busy ? "Checking…" : "Continue"}
            </button>
          </div>
        </form>
      ) : (
        <form className="stack" onSubmit={check} noValidate>
          <p className="muted">
            From your paper copy, type words {listPositions(positions)}. Positions count from 1, as numbered when the
            phrase was shown.
          </p>
          <div className="verify-grid">
            {positions.map((p, i) => (
              <Field
                key={p}
                label={`Word #${p}`}
                type={show ? "text" : "password"}
                value={answers[p] ?? ""}
                onChange={(e) => setAnswers({ ...answers, [p]: e.target.value })}
                error={wrong.includes(p) ? "Doesn't match." : null}
                readOnly={busy}
                mono
                autoFocus={i === 0}
                {...NO_ASSIST}
              />
            ))}
          </div>
          <label className="check">
            <input type="checkbox" checked={show} onChange={(e) => setShow(e.target.checked)} />
            <span>Show the words as I type</span>
          </label>
          {error && (
            <p className="field__error" role="alert">
              {error}
            </p>
          )}
          <div className="actions">
            <button type="button" className="btn btn--ghost" onClick={onShowWords}>
              Show the words again
            </button>
            <button type="submit" className="btn btn--primary" disabled={busy}>
              {busy ? "Checking…" : "Check"}
            </button>
          </div>
        </form>
      )}
    </Modal>,
    document.body,
  );
}

/** "Show the words again" as a dialog (from the reminder or the backup check). */
export function RevealPhraseDialog({ onClose }: { onClose: () => void }) {
  return createPortal(
    <Modal title="Your recovery phrase" onClose={onClose} wide>
      <RevealPhrase onClose={onClose} />
    </Modal>,
    document.body,
  );
}
