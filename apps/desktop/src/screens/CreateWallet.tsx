import { useEffect, useRef, useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { describeError } from "../lib/errors";
import { useWallet } from "../state/wallet";
import { ErrorNotice } from "../components/ErrorNotice";
import { Field, NO_ASSIST } from "../components/Field";
import { PasswordFields, passwordPairErrors, type PasswordPairState } from "../components/PasswordFields";
import { PhraseGrid, PhraseWarning } from "../components/Phrase";
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
 * Create → write the phrase down → check three words. The phrase lives only in this component's
 * state until the check passes; never in storage, the URL or a log.
 *
 * The passed check is recorded with `verifyBackup`, which needs the password: it is kept in a
 * ref (never rendered) for as long as the phrase, and cleared with it.
 */
export function CreateWallet({ onBack, onFinished }: { onBack: () => void; onFinished: () => void }) {
  const { refreshInfo, notify } = useWallet();
  const [step, setStep] = useState<Step>("password");
  const [wordCount, setWordCount] = useState<12 | 24>(12);
  const [password, setPassword] = useState(EMPTY_PASSWORD);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [mnemonic, setMnemonic] = useState<string[] | null>(null);
  const [positions, setPositions] = useState<number[]>([]);
  const [recording, setRecording] = useState(false);
  const passwordRef = useRef<string | null>(null);
  useEffect(
    () => () => {
      passwordRef.current = null;
    },
    [],
  );

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
      passwordRef.current = password.password; // for `verifyBackup` once the check passes
      setPassword(EMPTY_PASSWORD);
      setMnemonic(created.mnemonic);
      setPositions(pickPositions(CHECKED_WORDS, created.mnemonic.length));
      setStep("backup");
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  async function finish(answers: [number, string][]) {
    if (recording) return;
    setRecording(true);
    // The user just proved their copy: record it, so the reminder doesn't ask again.
    let notRecorded: string | null = null;
    const pw = passwordRef.current;
    if (pw === null) {
      notRecorded = "the password is no longer available";
    } else {
      try {
        await api.verifyBackup(pw, answers);
      } catch (err) {
        notRecorded = describeError(err).title;
      }
    }
    // The words and the password leave UI state for good.
    passwordRef.current = null;
    setMnemonic(null);
    setPositions([]);
    await refreshInfo().catch(() => undefined);
    if (notRecorded !== null) {
      notify(
        "info",
        `Your backup check passed, but btcw couldn't record it (${notRecorded}). The reminder stays until you verify the backup again.`,
      );
    }
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
        <Verify
          words={mnemonic}
          positions={positions}
          checking={recording}
          onShowAgain={() => setStep("backup")}
          onVerified={(answers) => void finish(answers)}
        />
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

      <PhraseWarning />

      {revealed ? (
        <PhraseGrid words={words} />
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
  checking,
  onShowAgain,
  onVerified,
}: {
  words: string[];
  positions: number[];
  /** Recording the passed check in Rust. */
  checking: boolean;
  onShowAgain: () => void;
  /** With the answers as `[1-based position, word]`, for `verifyBackup`. */
  onVerified: (answers: [number, string][]) => void;
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
      const checked = positions.map((i): [number, string] => [i + 1, normalized(i)]);
      setAnswers({});
      onVerified(checked);
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
        <button type="button" className="btn btn--ghost" onClick={onShowAgain} disabled={checking}>
          Show the words again
        </button>
        <button type="submit" className="btn btn--primary" disabled={checking}>
          {checking ? "Checking…" : "Check and finish"}
        </button>
      </div>
    </form>
  );
}
