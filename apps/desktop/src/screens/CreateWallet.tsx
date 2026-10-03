import { useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { useWallet } from "../state/wallet";
import { ErrorNotice } from "../components/ErrorNotice";
import { Field, NO_ASSIST } from "../components/Field";
import { Icon } from "../components/Icon";
import { PasswordFields, passwordPairErrors, type PasswordPairState } from "../components/PasswordFields";
import { Steps } from "../components/Steps";

type Step = "password" | "backup" | "verify";

const STEPS = ["Password", "Write it down", "Check"];
const EMPTY_PASSWORD: PasswordPairState = { password: "", confirm: "", touched: false };
const CHECKED_WORDS = 3;

/** Three distinct random word positions (0-based, ascending) for the backup check. */
export function pickPositions(count: number, total: number): number[] {
  const picked = new Set<number>();
  const random = new Uint32Array(1);
  while (picked.size < Math.min(count, total)) {
    crypto.getRandomValues(random);
    picked.add((random[0] ?? 0) % total);
  }
  return [...picked].sort((a, b) => a - b);
}

/**
 * Create → show the phrase once → check three words. The phrase is the only secret the UI ever
 * receives (PLAN §4.2): it lives in this component's state from `createWallet` until the check
 * passes, and is never written to storage, the URL or a log.
 */
export function CreateWallet({ onBack, onFinished }: { onBack: () => void; onFinished: () => void }) {
  const { refreshInfo } = useWallet();
  const [step, setStep] = useState<Step>("password");
  const [wordCount, setWordCount] = useState<12 | 24>(12);
  const [password, setPassword] = useState(EMPTY_PASSWORD);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [mnemonic, setMnemonic] = useState<string[] | null>(null);
  const [positions, setPositions] = useState<number[]>([]);

  async function create(e: FormEvent) {
    e.preventDefault();
    if (busy) return;
    if (!passwordPairErrors({ ...password, touched: true }).valid) {
      setPassword({ ...password, touched: true });
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const created = await api.createWallet(wordCount, password.password);
      setPassword(EMPTY_PASSWORD); // the backend has it now; the UI doesn't need it
      setMnemonic(created.mnemonic);
      setPositions(pickPositions(CHECKED_WORDS, created.mnemonic.length));
      setStep("backup");
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  async function finish() {
    // The words leave UI state for good; nothing can show them again.
    setMnemonic(null);
    setPositions([]);
    await refreshInfo().catch(() => undefined);
    onFinished();
  }

  return (
    <div className="flow">
      <Steps steps={STEPS} current={step === "password" ? 0 : step === "backup" ? 1 : 2} />

      {step === "password" && (
        <form className="panel stack" onSubmit={create} noValidate>
          <div>
            <h1 className="flow__title">Create a new wallet</h1>
            <p className="lead">
              First, a password. It encrypts the recovery phrase on this computer and is needed to send coins.
              Viewing your balance and history doesn&apos;t need it.
            </p>
          </div>
          <fieldset className="segmented" disabled={busy}>
            <legend className="field__label">Recovery phrase length</legend>
            {([12, 24] as const).map((n) => (
              <label key={n} className="segmented__option">
                <input type="radio" name="words" value={n} checked={wordCount === n} onChange={() => setWordCount(n)} />
                <span>{n} words</span>
              </label>
            ))}
            <p className="field__hint">12 words are plenty; 24 if you prefer the extra margin.</p>
          </fieldset>
          <PasswordFields
            state={password}
            onChange={setPassword}
            disabled={busy}
            purpose="Different from any password you use elsewhere."
          />
          <ErrorNotice error={error} />
          <div className="actions">
            <button type="button" className="btn btn--ghost" onClick={onBack} disabled={busy}>
              Back
            </button>
            <button type="submit" className="btn btn--primary" disabled={busy}>
              {busy ? "Creating…" : "Create wallet"}
            </button>
          </div>
        </form>
      )}

      {step === "backup" && mnemonic && <Backup words={mnemonic} onContinue={() => setStep("verify")} />}

      {step === "verify" && mnemonic && (
        <Verify words={mnemonic} positions={positions} onShowAgain={() => setStep("backup")} onVerified={finish} />
      )}
    </div>
  );
}

function Backup({ words, onContinue }: { words: string[]; onContinue: () => void }) {
  const [revealed, setRevealed] = useState(false);
  const [written, setWritten] = useState(false);

  return (
    <section className="panel stack" aria-labelledby="backup-title">
      <div>
        <h1 className="flow__title" id="backup-title">
          Write down your recovery phrase
        </h1>
        <p className="lead">
          These {words.length} words are your wallet. With them, you can restore your coins on any computer.
        </p>
      </div>

      <div className="notice notice--warning" role="note">
        <Icon name="alert" className="notice__icon" />
        <div className="notice__body">
          <p className="notice__title">Anyone with these words can take your coins.</p>
          <ul className="notice__list">
            <li>Write them on paper, in order, and keep the paper somewhere safe and private.</li>
            <li>Don&apos;t take a screenshot or photo, and don&apos;t paste them into a file, email or chat.</li>
            <li>btcw will never show them again. Lose the paper and this computer, and the coins are gone.</li>
          </ul>
        </div>
      </div>

      {revealed ? (
        <ol className="phrase" aria-label="Recovery phrase">
          {words.map((word, i) => (
            <li key={i} className="phrase__item">
              <span className="phrase__n" aria-hidden="true">
                {i + 1}
              </span>
              <span className="phrase__word">{word}</span>
            </li>
          ))}
        </ol>
      ) : (
        <div className="phrase phrase--hidden">
          <p>Make sure nobody can see your screen, then show the words.</p>
          <button type="button" className="btn btn--primary" onClick={() => setRevealed(true)}>
            Show the {words.length} words
          </button>
        </div>
      )}

      <label className="check">
        <input type="checkbox" checked={written} disabled={!revealed} onChange={(e) => setWritten(e.target.checked)} />
        <span>I&apos;ve written all {words.length} words down on paper, in order.</span>
      </label>
      <div className="actions">
        <button type="button" className="btn btn--primary" disabled={!written} onClick={onContinue}>
          Continue to the check
        </button>
      </div>
    </section>
  );
}

function Verify({
  words,
  positions,
  onShowAgain,
  onVerified,
}: {
  words: string[];
  positions: number[];
  onShowAgain: () => void;
  onVerified: () => void;
}) {
  const [answers, setAnswers] = useState<Record<number, string>>({});
  const [wrong, setWrong] = useState<number[]>([]);
  const [missing, setMissing] = useState(false);

  function check(e: FormEvent) {
    e.preventDefault();
    const normalized = (i: number) => (answers[i] ?? "").trim().toLowerCase();
    if (positions.some((i) => normalized(i) === "")) {
      setMissing(true);
      return;
    }
    setMissing(false);
    const mismatched = positions.filter((i) => normalized(i) !== words[i]);
    setWrong(mismatched);
    if (mismatched.length === 0) {
      setAnswers({});
      onVerified();
    }
  }

  const ordinal = positions.map((i) => `#${i + 1}`);
  return (
    <form className="panel stack" onSubmit={check} noValidate aria-labelledby="verify-title">
      <div>
        <h1 className="flow__title" id="verify-title">
          Check your backup
        </h1>
        <p className="lead">
          From your paper copy, type words {ordinal.slice(0, -1).join(", ")} and {ordinal.at(-1)}. This makes sure the
          backup is complete before any coins depend on it.
        </p>
      </div>
      <div className="verify-grid">
        {positions.map((i) => (
          <Field
            key={i}
            label={`Word #${i + 1}`}
            value={answers[i] ?? ""}
            onChange={(e) => setAnswers({ ...answers, [i]: e.target.value })}
            error={wrong.includes(i) ? "Doesn't match your phrase." : null}
            mono
            {...NO_ASSIST}
          />
        ))}
      </div>
      {missing && (
        <p className="field__error" role="alert">
          Fill in all {positions.length} words.
        </p>
      )}
      {wrong.length > 0 && (
        <p className="field__error" role="alert">
          {wrong.length === 1 ? `Word #${(wrong[0] ?? 0) + 1} doesn't` : "Some words don't"} match. Check your paper
          copy; if it is wrong, look at the words again and fix it.
        </p>
      )}
      <div className="actions">
        <button type="button" className="btn btn--ghost" onClick={onShowAgain}>
          Show the words again
        </button>
        <button type="submit" className="btn btn--primary">
          Check and finish
        </button>
      </div>
    </form>
  );
}
