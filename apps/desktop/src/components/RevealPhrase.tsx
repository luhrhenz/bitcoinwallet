import { useEffect, useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { describeError } from "../lib/errors";
import { Field, NO_ASSIST } from "./Field";
import { PhraseGrid, PhraseWarning } from "./Phrase";

/** How long the words stay in this component's state once decrypted. */
export const REVEAL_TIMEOUT_MS = 60_000;

/**
 * "Show recovery phrase": password → "Reveal" → numbered grid. The words live only in this
 * component's state and are dropped after `REVEAL_TIMEOUT_MS`, on "Hide", or on unmount.
 */
export function RevealPhrase({ onClose }: { onClose?: () => void }) {
  const [password, setPassword] = useState("");
  const [words, setWords] = useState<string[] | null>(null);
  const [shown, setShown] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [timedOut, setTimedOut] = useState(false);

  useEffect(() => {
    if (!words) return;
    const timer = setTimeout(() => {
      setWords(null);
      setShown(false);
      setTimedOut(true);
    }, REVEAL_TIMEOUT_MS);
    return () => clearTimeout(timer);
  }, [words]);

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
      const { mnemonic } = await api.revealPhrase(password);
      setPassword(""); // used once; Rust has decrypted the phrase
      setTimedOut(false);
      setShown(false);
      setWords(mnemonic);
    } catch (err) {
      setError(describeError(err).title);
    } finally {
      setBusy(false);
    }
  }

  function hide() {
    setWords(null);
    setShown(false);
  }

  if (!words) {
    return (
      <form className="stack" onSubmit={submit} noValidate>
        {timedOut && (
          <p className="muted small" role="status">
            The words were hidden again after a minute. Enter your password to see them again.
          </p>
        )}
        <Field
          label="Wallet password"
          type="password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          error={error}
          hint="The phrase is stored encrypted with this password; btcw decrypts it only to show it to you."
          readOnly={busy}
          autoFocus
          data-autofocus
          {...NO_ASSIST}
        />
        <div className="actions">
          {onClose && (
            <button type="button" className="btn btn--ghost" onClick={onClose}>
              Cancel
            </button>
          )}
          <button type="submit" className="btn btn--primary" disabled={busy}>
            {busy ? "Decrypting…" : "Continue"}
          </button>
        </div>
      </form>
    );
  }

  return (
    <div className="stack">
      <PhraseWarning />
      {shown ? (
        <PhraseGrid words={words} />
      ) : (
        <div className="phrase phrase--hidden">
          <p>Make sure nobody can see your screen, then reveal the words.</p>
          <button type="button" className="btn btn--primary" onClick={() => setShown(true)}>
            Reveal the {words.length} words
          </button>
        </div>
      )}
      <p className="muted small">They are hidden again after a minute, or as soon as you leave this screen.</p>
      <div className="actions">
        <button type="button" className="btn btn--ghost" onClick={onClose ?? hide}>
          {onClose ? "Close" : "Hide the words"}
        </button>
      </div>
    </div>
  );
}
