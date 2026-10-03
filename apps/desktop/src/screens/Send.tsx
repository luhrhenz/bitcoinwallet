import { useEffect, useRef, useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { formatBtc, formatSat, parseAmount, satToInput, type AmountUnit } from "../lib/amount";
import { errorText, toApiError } from "../lib/errors";
import { NETWORKS } from "../lib/network";
import type { PreparedSend, SendPreview } from "../lib/types";
import type { Go } from "../state/nav";
import { useWallet } from "../state/wallet";
import { AddressChunks } from "../components/Address";
import { Btc, Sat } from "../components/Amount";
import { BackupReminder } from "../components/BackupReminder";
import { ErrorNotice } from "../components/ErrorNotice";
import { Field, NO_ASSIST } from "../components/Field";
import { ScreenHeader } from "../components/Frame";
import { Icon } from "../components/Icon";

type FieldErrors = { to: string | null; amount: string | null; fee: string | null };
const NO_ERRORS: FieldErrors = { to: null, amount: null, fee: null };
const MAX_FEE_RATE = 1000;

/** `3` → "3", `2.5` → "2.5", `1.234567` → "1.23" */
const rate = (satVb: number) => String(Math.round(satVb * 100) / 100);

function parseFeeRate(text: string): { ok: true; value: number | null } | { ok: false; error: string } {
  const t = text.trim();
  if (t === "") return { ok: true, value: null };
  if (!/^\d+(\.\d{1,3})?$/.test(t)) return { ok: false, error: "Enter a number of sat/vB, like 2 or 2.5." };
  const value = Number(t);
  if (value <= 0) return { ok: false, error: "The fee rate must be above zero." };
  if (value > MAX_FEE_RATE) {
    return { ok: false, error: `Above ${MAX_FEE_RATE} sat/vB is almost certainly a mistake.` };
  }
  return { ok: true, value };
}

/**
 * Send: form → (unlock) → preview → confirm or cancel → tx detail. The transaction itself
 * (PSBT) is built and signed in Rust; the UI only sees the preview and an opaque id.
 */
export function Send({ go }: { go: Go }) {
  const wallet = useWallet();
  const { info, balance } = wallet;
  const meta = NETWORKS[info.network];

  const [to, setTo] = useState("");
  const [amountText, setAmountText] = useState("");
  const [unit, setUnit] = useState<AmountUnit>("btc");
  const [feeText, setFeeText] = useState("");
  const [errors, setErrors] = useState<FieldErrors>(NO_ERRORS);
  const [formError, setFormError] = useState<unknown>(null);
  const [busy, setBusy] = useState<null | "preparing" | "sending">(null);
  const [prepared, setPrepared] = useState<PreparedSend | null>(null);

  // The open preview, readable from cleanup code. Cleared *before* anything that should not
  // cancel it (a successful send) so leaving the screen never cancels a sent transaction.
  const open = useRef<PreparedSend | null>(null);
  const sending = useRef(false);
  const setPreview = (next: PreparedSend | null) => {
    open.current = next;
    setPrepared(next);
  };

  // Leaving the screen with a preview open releases it (reserved coins, change address).
  useEffect(
    () => () => {
      const pending = open.current;
      open.current = null;
      if (pending) void api.cancelSend(pending.id).catch(() => undefined);
    },
    [],
  );

  // Locked while a preview is open (auto-lock, "Lock now"): discard it; the user reviews again.
  useEffect(() => {
    if (info.unlocked || sending.current) return;
    const pending = open.current;
    if (!pending) return;
    setPreview(null);
    void api.cancelSend(pending.id).catch(() => undefined);
    wallet.notify(
      "info",
      "The wallet locked, so the prepared transaction was discarded. Nothing was sent; review it again to continue.",
    );
  }, [info.unlocked]); // only the lock state matters here

  const parsedAmount = amountText.trim() === "" ? null : parseAmount(amountText, unit);

  function switchUnit(next: AmountUnit) {
    if (next === unit) return;
    if (parsedAmount?.ok) setAmountText(satToInput(parsedAmount.sat, next));
    setUnit(next);
    setErrors((e) => ({ ...e, amount: null }));
  }

  function validate(): { amountSat: number; feeRate: number | null } | null {
    const next: FieldErrors = { ...NO_ERRORS };
    if (to.trim() === "") next.to = "Enter the address you're sending to.";
    else if (/\s/.test(to.trim())) next.to = "An address has no spaces. Copy it again from the source.";
    const amount = parseAmount(amountText, unit);
    if (!amount.ok) next.amount = amount.error;
    const fee = parseFeeRate(feeText);
    if (!fee.ok) next.fee = fee.error;
    setErrors(next);
    if (!amount.ok || !fee.ok || next.to) return null;
    return { amountSat: amount.sat, feeRate: fee.value };
  }

  async function review(e: FormEvent) {
    e.preventDefault();
    if (busy) return;
    const input = validate();
    if (!input) return;
    setFormError(null);
    // Two tries: the second after an unlock, if the backend says the wallet is locked.
    for (let attempt = 0; attempt < 2; attempt++) {
      if (!(await wallet.requestUnlock())) return;
      setBusy("preparing");
      try {
        setPreview(await api.prepareSend(to.trim(), input.amountSat, input.feeRate));
        setBusy(null);
        void wallet.refreshWallet(); // preparing syncs first; show the balance it used
        return;
      } catch (err) {
        setBusy(null);
        void wallet.refreshWallet();
        const { code } = toApiError(err);
        if (code === "locked") {
          await wallet.refreshInfo().catch(() => undefined);
          continue;
        }
        const text = errorText(err, { network: info.network });
        if (code === "invalid_address" || code === "network_mismatch") setErrors((x) => ({ ...x, to: text }));
        else if (code === "dust_amount" || code === "insufficient_funds") setErrors((x) => ({ ...x, amount: text }));
        else setFormError(err);
        return;
      }
    }
  }

  async function confirm() {
    const pending = open.current;
    if (!pending || busy) return;
    setBusy("sending");
    sending.current = true;
    try {
      const { txid } = await api.confirmSend(pending.id);
      open.current = null; // sent: nothing to cancel any more
      void wallet.refreshWallet();
      go({ name: "tx", txid, sent: pending.preview });
    } catch (err) {
      sending.current = false;
      setBusy(null);
      const { code } = toApiError(err);
      if (code === "locked") {
        // The lock effect discards the preview once the UI learns the wallet is locked.
        await wallet.refreshInfo().catch(() => undefined);
        return;
      }
      // The backend drops a prepared payment after any failed send (and an expired one): back
      // to the form, with what was typed still there, to review it again.
      setPreview(null);
      setFormError(err);
      void wallet.refreshWallet();
    }
  }

  async function cancel() {
    const pending = open.current;
    if (!pending) return;
    setPreview(null);
    try {
      await api.cancelSend(pending.id);
    } catch {
      // Nothing to undo on our side; the backend forgets unconfirmed previews on its own.
    }
    wallet.notify("info", "Cancelled. Nothing was sent.");
  }

  if (prepared) {
    return (
      <Preview
        preview={prepared.preview}
        sending={busy === "sending"}
        onConfirm={() => void confirm()}
        onCancel={() => void cancel()}
      />
    );
  }

  const spendable = balance ? balance.total_sat - balance.immature_sat : null;
  const equivalent =
    parsedAmount?.ok === true
      ? unit === "btc"
        ? `= ${formatSat(parsedAmount.sat)}`
        : `= ${formatBtc(parsedAmount.sat)} BTC`
      : null;

  return (
    <div className="screen">
      <ScreenHeader title="Send" lead={`Pay a ${meta.label} address. You'll see the fee and check everything before anything is sent.`} />
      <BackupReminder />
      {!info.unlocked && (
        <p className="inline-note">
          <Icon name="lock" size={16} /> The wallet is locked. You&apos;ll be asked for your password when you review.
        </p>
      )}
      <form className="panel stack" onSubmit={review} noValidate>
        <Field
          label="Recipient address"
          value={to}
          onChange={(e) => {
            setTo(e.target.value);
            setErrors((x) => ({ ...x, to: null }));
          }}
          error={errors.to}
          placeholder={`${meta.addressPrefix}q…`}
          mono
          disabled={busy !== null}
          {...NO_ASSIST}
        />
        <Field
          label="Amount"
          inputMode="decimal"
          value={amountText}
          onChange={(e) => {
            setAmountText(e.target.value);
            setErrors((x) => ({ ...x, amount: null }));
          }}
          error={errors.amount}
          placeholder={unit === "btc" ? "0.00000000" : "0"}
          mono
          disabled={busy !== null}
          {...NO_ASSIST}
          hint={
            <>
              {equivalent && <span className="mono">{equivalent}</span>}
              {equivalent && spendable !== null && " · "}
              {spendable !== null && (
                <span>
                  Balance <Btc sat={spendable} />
                </span>
              )}
            </>
          }
          addon={
            <fieldset className="segmented segmented--inline" disabled={busy !== null}>
              <legend className="sr-only">Unit</legend>
              {(["btc", "sat"] as const).map((u) => (
                <label key={u} className="segmented__option">
                  <input type="radio" name="unit" value={u} checked={unit === u} onChange={() => switchUnit(u)} />
                  <span>{u === "btc" ? "BTC" : "sat"}</span>
                </label>
              ))}
            </fieldset>
          }
        />
        <Field
          label="Fee rate (optional)"
          inputMode="decimal"
          value={feeText}
          onChange={(e) => {
            setFeeText(e.target.value);
            setErrors((x) => ({ ...x, fee: null }));
          }}
          error={errors.fee}
          placeholder="Automatic"
          mono
          disabled={busy !== null}
          addon={<span className="field__unit">sat/vB</span>}
          hint="Leave empty to use your node's estimate. A higher rate confirms sooner and costs more."
          {...NO_ASSIST}
        />
        <ErrorNotice error={formError} network={info.network} />
        <div className="actions">
          <button type="submit" className="btn btn--primary" disabled={busy !== null}>
            {busy === "preparing" ? "Preparing…" : "Review"}
          </button>
        </div>
      </form>
    </div>
  );
}

function Preview({
  preview,
  sending,
  onConfirm,
  onCancel,
}: {
  preview: SendPreview;
  sending: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const { info } = useWallet();
  const meta = NETWORKS[info.network];
  const feeShare = preview.amount_sat > 0 ? preview.fee_sat / preview.amount_sat : 0;
  const warnings: string[] = [];
  if (meta.real) warnings.push("This sends real bitcoin. A transaction can't be undone once it is broadcast.");
  if (feeShare >= 0.1) {
    warnings.push(`The fee is ${Math.round(feeShare * 100)}% of the amount you're sending.`);
  }
  if (preview.fee_rate_sat_vb > 100) {
    warnings.push(`${rate(preview.fee_rate_sat_vb)} sat/vB is a high fee rate. Check that it's what you meant.`);
  }

  return (
    <div className="screen">
      <ScreenHeader title="Review and send" />
      <section className="panel review" aria-label="Transaction preview">
        <div className="review__to">
          <p className="eyebrow">Sending to</p>
          <p className="review__address">
            <AddressChunks address={preview.to} size="lg" />
          </p>
          <p className="muted small">
            Compare every group of four with the address you were given, on paper or on the recipient&apos;s screen.
            Malware can swap a copied address for its own; looking is the only check.
          </p>
        </div>
        <dl className="lines">
          <div className="lines__row">
            <dt>Amount</dt>
            <dd>
              <Btc sat={preview.amount_sat} />
              <Sat sat={preview.amount_sat} />
            </dd>
          </div>
          <div className="lines__row">
            <dt>Network fee</dt>
            <dd>
              <Btc sat={preview.fee_sat} />
              <span className="sat">
                {formatSat(preview.fee_sat)} · {rate(preview.fee_rate_sat_vb)} sat/vB × {preview.vsize} vB
              </span>
            </dd>
          </div>
          <div className="lines__row">
            <dt>Change</dt>
            <dd>
              {preview.change_sat === null ? (
                <span className="lines__note">None: the small remainder goes to the fee</span>
              ) : (
                <>
                  <Btc sat={preview.change_sat} />
                  <span className="lines__note">back to a new address in this wallet</span>
                </>
              )}
            </dd>
          </div>
          <div className="lines__row lines__row--total">
            <dt>Total leaving the wallet</dt>
            <dd>
              <Btc sat={preview.total_sat} />
              <Sat sat={preview.total_sat} />
            </dd>
          </div>
        </dl>
        {warnings.length > 0 && (
          <div className="notice notice--warning" role="note">
            <Icon name="alert" className="notice__icon" />
            <div className="notice__body">
              {warnings.map((w) => (
                <p key={w} className="notice__title">
                  {w}
                </p>
              ))}
            </div>
          </div>
        )}
        <div className="actions">
          <button type="button" className="btn btn--ghost" onClick={onCancel} disabled={sending}>
            Cancel
          </button>
          <button type="button" className="btn btn--primary btn--send" onClick={onConfirm} disabled={sending}>
            {sending ? "Sending…" : `Send ${formatBtc(preview.amount_sat)} BTC`}
          </button>
        </div>
      </section>
    </div>
  );
}
