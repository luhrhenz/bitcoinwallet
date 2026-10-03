import { useId, useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { describeError, toApiError } from "../lib/errors";
import { plural } from "../lib/format";
import { useWallet } from "../state/wallet";
import { Btc } from "../components/Amount";
import { ErrorNotice } from "../components/ErrorNotice";
import { Field, NO_ASSIST } from "../components/Field";
import { PasswordFields, passwordPairErrors, type PasswordPairState } from "../components/PasswordFields";
import { Steps } from "../components/Steps";

const VALID_COUNTS = [12, 15, 18, 21, 24];
const EMPTY_PASSWORD: PasswordPairState = { password: "", confirm: "", touched: false };

const wordsOf = (phrase: string) => phrase.trim().toLowerCase().split(/\s+/).filter(Boolean);

function wordCountHint(count: number): { text: string; ok: boolean } {
  if (count === 0) return { text: "12 or 24 words (15, 18 and 21 work too), separated by spaces.", ok: false };
  if (VALID_COUNTS.includes(count)) return { text: `${count} words.`, ok: true };
  return { text: `${plural(count, "word", "words")}. A recovery phrase has 12, 15, 18, 21 or 24.`, ok: false };
}

/** Restore from a phrase, then scan the chain with a progress bar. */
export function RestoreWallet({ onBack, onFinished }: { onBack: () => void; onFinished: () => void }) {
  const wallet = useWallet();
  const phraseId = useId();
  const [stage, setStage] = useState<"form" | "scan">("form");
  const [phrase, setPhrase] = useState("");
  const [birthday, setBirthday] = useState("");
  const [password, setPassword] = useState(EMPTY_PASSWORD);
  const [phraseError, setPhraseError] = useState<string | null>(null);
  const [birthdayError, setBirthdayError] = useState<string | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);
  const [scanDone, setScanDone] = useState(false);

  const words = wordsOf(phrase);
  const hint = wordCountHint(words.length);

  async function restore(e: FormEvent) {
    e.preventDefault();
    if (busy) return;
    let ok = passwordPairErrors({ ...password, touched: true }).valid;
    if (!ok) setPassword({ ...password, touched: true });
    if (!VALID_COUNTS.includes(words.length)) {
      setPhraseError(words.length === 0 ? "Type your recovery phrase." : hint.text);
      ok = false;
    } else {
      setPhraseError(null);
    }
    let height: number | null = null;
    if (birthday.trim() !== "") {
      if (!/^\d+$/.test(birthday.trim())) {
        setBirthdayError("A block height is a whole number, like 118000.");
        ok = false;
      } else {
        height = Number(birthday.trim());
        setBirthdayError(null);
      }
    } else {
      setBirthdayError(null);
    }
    if (!ok) return;

    setBusy(true);
    setError(null);
    try {
      await api.restoreWallet(words.join(" "), password.password, height);
    } catch (err) {
      setBusy(false);
      const { code } = toApiError(err);
      if (code === "invalid_mnemonic") {
        const { title, detail } = describeError(err);
        setPhraseError(detail ? `${title} ${capitalize(detail)}.` : title);
      } else {
        setError(err);
      }
      return;
    }
    // Restored: the phrase and password leave UI state now.
    setPhrase("");
    setPassword(EMPTY_PASSWORD);
    setBusy(false);
    setStage("scan");
    await wallet.refreshInfo().catch(() => undefined);
    await wallet.runSync();
    setScanDone(true);
  }

  if (stage === "scan") {
    return <Scan done={scanDone} onRetry={() => void wallet.runSync()} onFinished={onFinished} />;
  }

  return (
    <div className="flow">
      <Steps steps={["Phrase and password", "Scan the chain"]} current={0} />
      <form className="panel stack" onSubmit={restore} noValidate>
        <div>
          <h1 className="flow__title">Restore from a recovery phrase</h1>
          <p className="lead">
            Type the words in order. They stay on this computer: btcw encrypts them with your new password and finds
            your coins through your own node.
          </p>
        </div>
        <div className={`field ${phraseError ? "field--invalid" : ""}`}>
          <label className="field__label" htmlFor={phraseId}>
            Recovery phrase
          </label>
          <textarea
            id={phraseId}
            className="input input--mono textarea"
            rows={4}
            value={phrase}
            onChange={(e) => setPhrase(e.target.value)}
            aria-describedby={`${phraseId}-hint${phraseError ? ` ${phraseId}-error` : ""}`}
            aria-invalid={phraseError ? true : undefined}
            disabled={busy}
            {...NO_ASSIST}
          />
          <p className={`field__hint ${hint.ok ? "field__hint--ok" : ""}`} id={`${phraseId}-hint`} aria-live="polite">
            {hint.text}
          </p>
          {phraseError && (
            <p className="field__error" id={`${phraseId}-error`}>
              {phraseError}
            </p>
          )}
        </div>
        <Field
          label="Wallet birthday (optional)"
          inputMode="numeric"
          value={birthday}
          onChange={(e) => setBirthday(e.target.value)}
          error={birthdayError}
          disabled={busy}
          placeholder="Scan the whole chain"
          hint="The block height when this wallet was first used. Scanning starts there, which is much faster. Leave it empty if you're not sure: scanning from the start is slower but never misses coins."
          {...NO_ASSIST}
        />
        <PasswordFields
          state={password}
          onChange={setPassword}
          disabled={busy}
          purpose="Encrypts the phrase on this computer. It doesn't have to match any earlier password."
        />
        <ErrorNotice error={error} network={wallet.info.network} />
        <div className="actions">
          <button type="button" className="btn btn--ghost" onClick={onBack} disabled={busy}>
            Back
          </button>
          <button type="submit" className="btn btn--primary" disabled={busy}>
            {busy ? "Restoring…" : "Restore wallet"}
          </button>
        </div>
      </form>
    </div>
  );
}

const capitalize = (text: string) => text.charAt(0).toUpperCase() + text.slice(1);

function Scan({ done, onRetry, onFinished }: { done: boolean; onRetry: () => void; onFinished: () => void }) {
  const { sync, balance, history, info } = useWallet();
  const progress = sync.progress;
  const base = sync.base ?? 0;
  const span = progress ? progress.tip_height - base : 0;
  const percent = progress ? (span > 0 ? ((progress.height - base) / span) * 100 : 100) : 0;
  const failed = done && !sync.running && sync.error !== null;

  return (
    <div className="flow">
      <Steps steps={["Phrase and password", "Scan the chain"]} current={1} />
      <section className="panel stack" aria-labelledby="scan-title">
        <div>
          <h1 className="flow__title" id="scan-title">
            {!done || sync.running ? "Scanning the chain for your coins" : failed ? "The scan didn't finish" : "Wallet restored"}
          </h1>
          <p className="lead">
            {!done || sync.running
              ? "Your wallet is restored. btcw is now reading blocks from your node to find the payments that belong to it."
              : failed
                ? "Your wallet is restored, but reading the chain stopped before the end."
                : "btcw read the chain through your node and found what belongs to this wallet. Later payments show up whenever it syncs."}
          </p>
        </div>
        {(sync.running || !done) && (
          <>
            <div
              className="progress progress--lg"
              role="progressbar"
              aria-label="Scan progress"
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={Math.round(percent)}
            >
              <span className="progress__fill" style={{ width: `${progress ? Math.max(2, percent) : 2}%` }} />
            </div>
            <p className="muted" aria-live="polite">
              {progress ? `Block ${progress.height} of ${progress.tip_height}` : "Connecting to your node…"}
            </p>
          </>
        )}
        {failed && (
          <ErrorNotice error={sync.error} network={info.network}>
            <p className="notice__detail">You can open the wallet now and sync later from the overview.</p>
          </ErrorNotice>
        )}
        {done && !sync.running && !failed && (
          <div className="summary">
            <p>
              Found {plural(history?.length ?? 0, "transaction", "transactions")}, up to block{" "}
              {sync.lastReport?.tip_height ?? info.synced_height ?? 0}.
            </p>
            {balance && (
              <p className="summary__balance">
                <Btc sat={balance.total_sat} />
              </p>
            )}
          </div>
        )}
        <div className="actions">
          {failed && (
            <button type="button" className="btn btn--ghost" onClick={onRetry}>
              Try again
            </button>
          )}
          <button type="button" className="btn btn--primary" onClick={onFinished} disabled={!done || sync.running}>
            Open wallet
          </button>
        </div>
      </section>
    </div>
  );
}
