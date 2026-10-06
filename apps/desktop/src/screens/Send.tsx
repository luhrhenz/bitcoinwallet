import { useEffect, useRef, useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { formatBtc, formatSat, parseAmount, parseFeeRate, satToInput, type AmountUnit } from "../lib/amount";
import { errorText, toApiError } from "../lib/errors";
import { NETWORKS } from "../lib/network";
import type { Contact, PreparedSend } from "../lib/types";
import type { Go } from "../state/nav";
import { useWallet } from "../state/wallet";
import { Btc } from "../components/Amount";
import { BackupReminder } from "../components/BackupReminder";
import { ErrorNotice } from "../components/ErrorNotice";
import { Field, NO_ASSIST } from "../components/Field";
import { ScreenHeader } from "../components/Frame";
import { Icon } from "../components/Icon";
import { PaymentPreview, sendLabel } from "../components/PaymentPreview";
import { contactNamed, RecipientField } from "../components/RecipientField";

type FieldErrors = { to: string | null; amount: string | null; fee: string | null };
const NO_ERRORS: FieldErrors = { to: null, amount: null, fee: null };
/**
 * Send: form → (unlock) → preview → confirm or cancel → tx detail. The PSBT is built and signed
 * in Rust; the UI only sees the preview and an opaque id. The recipient may be a contact's name.
 */
export function Send({ go, initialTo = "" }: { go: Go; initialTo?: string }) {
  const wallet = useWallet();
  const { info, balance } = wallet;
  const meta = NETWORKS[info.network];

  const [to, setTo] = useState(initialTo);
  const [contacts, setContacts] = useState<Contact[]>([]);
  const [amountText, setAmountText] = useState("");
  const [unit, setUnit] = useState<AmountUnit>("btc");
  const [feeText, setFeeText] = useState("");
  const [errors, setErrors] = useState<FieldErrors>(NO_ERRORS);
  const [formError, setFormError] = useState<unknown>(null);
  const [busy, setBusy] = useState<null | "preparing" | "sending">(null);
  const [prepared, setPrepared] = useState<PreparedSend | null>(null);

  // The open preview, readable from cleanup code. Cleared before a successful send, so leaving
  // the screen never cancels a sent transaction.
  const open = useRef<PreparedSend | null>(null);
  const sending = useRef(false);
  const setPreview = (next: PreparedSend | null) => {
    open.current = next;
    setPrepared(next);
  };

  // The address book, for the recipient picker. Without it, addresses still work.
  useEffect(() => {
    let alive = true;
    api
      .listContacts()
      .then((list) => alive && setContacts(list))
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, []);

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
    const text = to.trim();
    // Contact names may have spaces ("Alice B."); a pasted address with spaces in it is a mistake.
    const addressLike = text.length > 40 || /^(bc1|tb1|bcrt1)/i.test(text);
    if (text === "") next.to = "Enter the address you're sending to, or a contact's name.";
    else if (/\s/.test(text) && addressLike && !contactNamed(contacts, text)) {
      next.to = "An address has no spaces. Copy it again from the source.";
    }
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
        if (code === "invalid_address" || code === "network_mismatch" || code === "contact") {
          setErrors((x) => ({ ...x, to: text }));
        }
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
      // The backend drops a prepared payment after any failed send: back to the form to review
      // it again.
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
      <div className="screen">
        <ScreenHeader title="Review and send" />
        <PaymentPreview
          preview={prepared.preview}
          label="Transaction preview"
          confirmLabel={sendLabel(prepared.preview)}
          sending={busy === "sending"}
          onConfirm={() => void confirm()}
          onCancel={() => void cancel()}
        />
      </div>
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
      <ScreenHeader
        title="Send"
        lead={`Pay a ${meta.label} address or one of your contacts. You'll see the fee and check everything before anything is sent.`}
      />
      <BackupReminder />
      {!info.unlocked && (
        <p className="inline-note">
          <Icon name="lock" size={16} /> The wallet is locked. You&apos;ll be asked for your password when you review.
        </p>
      )}
      <form className="panel stack" onSubmit={review} noValidate>
        <RecipientField
          value={to}
          onChange={(text) => {
            setTo(text);
            setErrors((x) => ({ ...x, to: null }));
          }}
          contacts={contacts}
          error={errors.to}
          placeholder={contacts.length > 0 ? `${meta.addressPrefix}q… or a contact's name` : `${meta.addressPrefix}q…`}
          hint={`A ${meta.label} address, or the name of a saved contact.`}
          disabled={busy !== null}
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
