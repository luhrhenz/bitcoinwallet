import { formatBtc, formatSat } from "../lib/amount";
import { NETWORKS } from "../lib/network";
import type { SendPreview } from "../lib/types";
import { useWallet } from "../state/wallet";
import { AddressChunks } from "./Address";
import { Btc, Sat } from "./Amount";
import { Icon } from "./Icon";

/** `3` → "3", `2.5` → "2.5", `1.234567` → "1.23" */
export const rate = (satVb: number) => String(Math.round(satVb * 100) / 100);

/**
 * What the user checks before a payment or fee bump is signed: the full address (with any
 * contact name next to it), every amount, and for a bump the transaction it replaces.
 */
export function PaymentPreview({
  preview,
  label,
  confirmLabel,
  sending,
  originalFeeSat = null,
  onConfirm,
  onCancel,
}: {
  preview: SendPreview;
  /** Accessible name of the card (tests and screen readers find it by this). */
  label: string;
  confirmLabel: string;
  sending: boolean;
  /** Fee bump only: what the transaction being replaced pays, to show the difference. */
  originalFeeSat?: number | null;
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
  const extraFee = preview.replaces !== null && originalFeeSat !== null ? preview.fee_sat - originalFeeSat : null;

  return (
    <section className="panel review" aria-label={label}>
      {preview.replaces !== null && (
        <div className="review__replaces">
          <p className="eyebrow">Replaces</p>
          <p className="mono break">{preview.replaces}</p>
          <p className="muted small">
            Same recipient, same amount, a higher fee taken from your change. Both spend the same coins, so only one of
            them can ever confirm: the recipient is paid once.
          </p>
        </div>
      )}
      <div className="review__to">
        <p className="eyebrow">Sending to</p>
        <p className="review__address">
          <AddressChunks address={preview.to} size="lg" />
        </p>
        {preview.contact !== null && (
          <p className="review__contact">
            <Icon name="contact" size={16} />
            <span>
              Contact <strong>{preview.contact}</strong>
            </span>
          </p>
        )}
        <p className="muted small">
          Compare every group of four with the address you were given, on paper or on the recipient&apos;s screen.
          Malware can swap a copied address for its own; looking is the only check.
          {preview.contact !== null &&
            " The name comes from your address book; the address above is where the money goes."}
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
            {extraFee !== null && originalFeeSat !== null && (
              <span className="lines__note review__extra">
                {formatSat(extraFee)} more than the {formatSat(originalFeeSat)} it pays now
              </span>
            )}
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
                <span className="lines__note">
                  {preview.replaces !== null ? "back to this wallet" : "back to a new address in this wallet"}
                </span>
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
          {sending ? "Sending…" : confirmLabel}
        </button>
      </div>
    </section>
  );
}

/** The confirm button's text for an ordinary payment. */
export const sendLabel = (preview: SendPreview) => `Send ${formatBtc(preview.amount_sat)} BTC`;
