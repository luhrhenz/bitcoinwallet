import { useEffect, useRef, useState, type FormEvent, type KeyboardEvent } from "react";
import { api } from "../lib/api";
import { formatSat, parseFeeRate } from "../lib/amount";
import { errorText, toApiError } from "../lib/errors";
import { direction, formatDateTime, plural, shortId, timeAgo } from "../lib/format";
import type { PreparedSend, SendPreview, TxRow, TxStatus } from "../lib/types";
import type { Go } from "../state/nav";
import { useWallet } from "../state/wallet";
import { AddressChunks } from "../components/Address";
import { Btc } from "../components/Amount";
import { CopyButton } from "../components/CopyButton";
import { ErrorNotice } from "../components/ErrorNotice";
import { Field, NO_ASSIST } from "../components/Field";
import { ScreenHeader } from "../components/Frame";
import { Icon } from "../components/Icon";
import { PaymentPreview } from "../components/PaymentPreview";

/** How often the open screen asks for the status. */
export const POLL_MS = 10_000;
/** Six blocks is the customary point where a payment is treated as final. */
const FINAL_CONFIRMATIONS = 6;

const statusKey = (s: TxStatus | null) =>
  s === null ? "none" : s.state === "confirmed" ? `c${s.height}:${s.confirmations}` : "u";

/**
 * One transaction, its status polled while the screen is open. Its label can be edited here,
 * and an unconfirmed outgoing payment can be sped up (RBF).
 */
export function TxDetail({ txid, sent, go }: { txid: string; sent?: SendPreview; go: Go }) {
  const wallet = useWallet();
  const { history, info, refreshWallet } = wallet;
  const row: TxRow | undefined = history?.find((tx) => tx.txid === txid);
  const [status, setStatus] = useState<TxStatus | null | undefined>(row?.status);
  const [pollError, setPollError] = useState<unknown>(null);
  const [checkedAt, setCheckedAt] = useState<number | null>(null);
  const lastKey = useRef<string | null>(null);
  /** Ask for the status now instead of at the next 10-second tick. */
  const pollNow = useRef<() => void>(() => undefined);
  const refresh = useRef(refreshWallet);
  refresh.current = refreshWallet;

  useEffect(() => {
    let alive = true;
    async function poll() {
      try {
        const next = await api.txStatus(txid);
        if (!alive) return;
        setStatus(next);
        setPollError(null);
        setCheckedAt(Date.now());
        const key = statusKey(next);
        // A new confirmation also changes balances and history: refresh them once.
        if (lastKey.current !== null && lastKey.current !== key) void refresh.current();
        lastKey.current = key;
      } catch (e) {
        if (alive) setPollError(e);
      }
    }
    pollNow.current = () => void poll();
    void poll();
    const timer = setInterval(() => void poll(), POLL_MS);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, [txid]);

  const net = row?.net_sat ?? (sent ? -sent.total_sat : null);
  const fee = row?.fee_sat ?? sent?.fee_sat ?? null;
  const dir = net === null ? null : direction(net);
  const title = dir === "received" ? "Received" : dir === "sent" ? "Sent" : dir === "self" ? "Sent to yourself" : "Transaction";

  // ── Speed up ──
  const current = status === undefined ? (row?.status ?? null) : status;
  // Only a payment this wallet sent (it spent coins) that hasn't confirmed can be replaced.
  const outgoing = dir === "sent" && (row ? row.sent_sat > 0 : sent !== undefined);
  const canSpeedUp = outgoing && current?.state === "unconfirmed";
  const [bump, setBump] = useState<PreparedSend | null>(null);
  const [bumpBusy, setBumpBusy] = useState(false);
  const [bumpError, setBumpError] = useState<unknown>(null);
  // The open replacement, readable from cleanup code; cleared before a successful send.
  const openBump = useRef<PreparedSend | null>(null);
  const sendingBump = useRef(false);
  const setBumpPreview = (next: PreparedSend | null) => {
    openBump.current = next;
    setBump(next);
  };

  // Leaving the screen with a speed-up preview open releases it.
  useEffect(
    () => () => {
      const pending = openBump.current;
      openBump.current = null;
      if (pending) void api.cancelSend(pending.id).catch(() => undefined);
    },
    [],
  );

  // Locked, or the payment confirmed, while the preview is open: discard it.
  const confirmedNow = current?.state === "confirmed";
  useEffect(() => {
    const pending = openBump.current;
    if (!pending || sendingBump.current) return;
    if (info.unlocked && !confirmedNow) return;
    setBumpPreview(null);
    void api.cancelSend(pending.id).catch(() => undefined);
    wallet.notify(
      "info",
      confirmedNow
        ? "This payment confirmed while you were reviewing, so it can't be sped up any more. Nothing else was sent."
        : "The wallet locked, so the prepared speed-up was discarded. Nothing was sent.",
    );
  }, [info.unlocked, confirmedNow]); // only these matter here

  async function confirmBump() {
    const pending = openBump.current;
    if (!pending || bumpBusy) return;
    setBumpBusy(true);
    sendingBump.current = true;
    try {
      const { txid: replacement } = await api.confirmSend(pending.id);
      openBump.current = null; // sent: nothing to cancel any more
      void refreshWallet();
      go({ name: "tx", txid: replacement, sent: pending.preview });
    } catch (err) {
      sendingBump.current = false;
      setBumpBusy(false);
      if (toApiError(err).code === "locked") {
        // The lock effect discards the preview once the UI learns the wallet is locked.
        await wallet.refreshInfo().catch(() => undefined);
        return;
      }
      // Rust drops the prepared replacement after any failed send. Check the status at once:
      // the payment may have just confirmed.
      setBumpPreview(null);
      setBumpError(err);
      void refreshWallet();
      pollNow.current();
    }
  }

  async function cancelBump() {
    const pending = openBump.current;
    if (!pending) return;
    setBumpPreview(null);
    await api.cancelSend(pending.id).catch(() => undefined);
    wallet.notify("info", "Cancelled. Nothing was sent.");
  }

  if (bump) {
    return (
      <div className="screen">
        <ScreenHeader
          title="Review the speed-up"
          lead="A replacement for your payment, paying a higher fee. Check it, then send it."
        />
        <PaymentPreview
          preview={bump.preview}
          label="Speed-up preview"
          confirmLabel="Send replacement"
          sending={bumpBusy}
          originalFeeSat={fee}
          onConfirm={() => void confirmBump()}
          onCancel={() => void cancelBump()}
        />
      </div>
    );
  }

  return (
    <div className="screen">
      <button type="button" className="link back" onClick={() => go({ name: "history" })}>
        <Icon name="back" size={16} />
        All transactions
      </button>

      {sent && (
        <div className="notice notice--success" role="status">
          <Icon name="check" className="notice__icon" />
          <div className="notice__body">
            {sent.replaces ? (
              <>
                <p className="notice__title">Sped up. The replacement is on its way.</p>
                <p className="notice__detail">
                  It replaces <span className="mono">{shortId(sent.replaces)}</span>, which nodes drop from their
                  mempools now. Only one of the two can confirm. This screen updates on its own.
                </p>
              </>
            ) : (
              <>
                <p className="notice__title">Sent. Your transaction is on its way.</p>
                <p className="notice__detail">
                  It waits in the mempool until a miner puts it in a block. This screen updates on its own.
                </p>
              </>
            )}
          </div>
        </div>
      )}

      <section className="txhead" aria-labelledby="tx-title">
        <h1 className="eyebrow" id="tx-title">
          {title}
        </h1>
        {net !== null && (
          <p className={`txhead__amount txhead__amount--${dir}`}>
            <Btc sat={net} signed />
          </p>
        )}
        {net !== null && <p className="balance__sat">{formatSat(Math.abs(net))}</p>}
      </section>

      <StatusPanel status={status} />
      <p className="muted small poll-note" aria-live="polite">
        {checkedAt ? `Checked ${timeAgo(checkedAt / 1000)}. ` : ""}Updates every {POLL_MS / 1000} seconds while this
        screen is open.
      </p>
      {pollError !== null && (
        <ErrorNotice error={pollError} network={info.network}>
          <p className="notice__detail">Showing the last known status; trying again in {POLL_MS / 1000} seconds.</p>
        </ErrorNotice>
      )}

      {canSpeedUp && (
        <SpeedUp
          txid={txid}
          error={bumpError}
          onError={setBumpError}
          onPrepared={(prepared) => {
            setBumpError(null);
            setBumpPreview(prepared);
          }}
        />
      )}
      {!canSpeedUp && bumpError !== null && <ErrorNotice error={bumpError} network={info.network} />}

      <dl className="lines lines--detail">
        {row && <LabelRow txid={txid} label={row.label} />}
        <div className="lines__row lines__row--stack">
          <dt>Transaction ID</dt>
          <dd className="txid">
            <span className="mono break">{txid}</span>
            <CopyButton text={txid} label="Copy ID" />
          </dd>
        </div>
        {sent?.replaces && (
          <div className="lines__row lines__row--stack">
            <dt>Replaces</dt>
            <dd>
              <span className="mono break small">{sent.replaces}</span>
            </dd>
          </div>
        )}
        {sent && (
          <div className="lines__row lines__row--stack">
            <dt>Sent to</dt>
            <dd>
              <AddressChunks address={sent.to} />
              {sent.contact && (
                <span className="lines__note">
                  Contact <strong>{sent.contact}</strong>
                </span>
              )}
            </dd>
          </div>
        )}
        {sent && (
          <div className="lines__row">
            <dt>Amount</dt>
            <dd>
              <Btc sat={sent.amount_sat} />
            </dd>
          </div>
        )}
        {row && row.received_sat > 0 && (
          <div className="lines__row">
            <dt>{row.sent_sat > 0 ? "Change back" : "Received"}</dt>
            <dd>
              <Btc sat={row.received_sat} />
            </dd>
          </div>
        )}
        {row && row.sent_sat > 0 && (
          <div className="lines__row">
            <dt>Coins spent</dt>
            <dd>
              <Btc sat={row.sent_sat} />
            </dd>
          </div>
        )}
        <div className="lines__row">
          <dt>Network fee</dt>
          <dd>
            {fee !== null ? (
              <>
                <Btc sat={fee} />
                <span className="sat">{formatSat(fee)}</span>
              </>
            ) : (
              <span className="muted">Not known: the sender paid it from coins outside this wallet</span>
            )}
          </dd>
        </div>
      </dl>
    </div>
  );
}

/**
 * "Speed up": a button, then a fee-rate form pre-filled with the minimum that can replace the
 * payment. Review asks for the password first when locked.
 */
function SpeedUp({
  txid,
  error,
  onError,
  onPrepared,
}: {
  txid: string;
  error: unknown;
  onError: (error: unknown) => void;
  onPrepared: (prepared: PreparedSend) => void;
}) {
  const wallet = useWallet();
  const { info } = wallet;
  const [stage, setStage] = useState<"closed" | "loading" | "form">("closed");
  const [min, setMin] = useState<number | null>(null);
  const [rateText, setRateText] = useState("");
  const [fieldError, setFieldError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const rateInput = useRef<HTMLInputElement>(null);
  const openButton = useRef<HTMLButtonElement>(null);
  const titleId = `speedup-${txid.slice(0, 8)}`;

  useEffect(() => {
    if (stage === "form") rateInput.current?.focus();
  }, [stage]);

  async function open() {
    setStage("loading");
    onError(null);
    try {
      const minimum = await api.minFeeBumpRate(txid);
      setMin(minimum);
      setRateText(String(minimum));
      setFieldError(null);
      setStage("form");
    } catch (err) {
      setStage("closed");
      onError(err);
    }
  }

  function close() {
    setStage("closed");
    setFieldError(null);
    onError(null);
    requestAnimationFrame(() => openButton.current?.focus());
  }

  async function review(e: FormEvent) {
    e.preventDefault();
    if (busy || min === null) return;
    const parsed = parseFeeRate(rateText);
    if (!parsed.ok) {
      setFieldError(parsed.error);
      return;
    }
    if (parsed.value === null) {
      setFieldError(`Enter a fee rate of at least ${min} sat/vB.`);
      return;
    }
    if (parsed.value < min) {
      setFieldError(`Replacing this payment needs at least ${min} sat/vB.`);
      return;
    }
    onError(null);
    // Two tries: the second after an unlock, if the backend says the wallet is locked.
    for (let attempt = 0; attempt < 2; attempt++) {
      if (!(await wallet.requestUnlock())) return;
      setBusy(true);
      try {
        const prepared = await api.prepareFeeBump(txid, parsed.value);
        setBusy(false);
        onPrepared(prepared);
        void wallet.refreshWallet();
        return;
      } catch (err) {
        setBusy(false);
        void wallet.refreshWallet();
        if (toApiError(err).code === "locked") {
          await wallet.refreshInfo().catch(() => undefined);
          continue;
        }
        onError(err);
        return;
      }
    }
  }

  function onKeyDown(e: KeyboardEvent<HTMLFormElement>) {
    if (e.key === "Escape" && !busy) {
      e.preventDefault();
      close();
    }
  }

  return (
    <section className="panel speedup" aria-labelledby={titleId}>
      <div className="speedup__head">
        <Icon name="bolt" className="speedup__icon" />
        <div>
          <h2 className="speedup__title" id={titleId}>
            Taking too long?
          </h2>
          <p className="muted small">
            Speed it up: send a replacement that pays a higher fee. The recipient gets the same amount; the extra fee comes
            out of your change.
          </p>
        </div>
      </div>
      {stage !== "form" ? (
        <div className="actions actions--start">
          <button
            ref={openButton}
            type="button"
            className="btn btn--primary"
            onClick={() => void open()}
            disabled={stage === "loading"}
          >
            <Icon name="bolt" size={16} />
            {stage === "loading" ? "Checking…" : "Speed up"}
          </button>
        </div>
      ) : (
        <form className="stack stack--tight" onSubmit={review} onKeyDown={onKeyDown} noValidate>
          <Field
            ref={rateInput}
            label="New fee rate"
            inputMode="decimal"
            className="field--narrow"
            value={rateText}
            onChange={(e) => {
              setRateText(e.target.value);
              setFieldError(null);
            }}
            error={fieldError}
            mono
            disabled={busy}
            addon={<span className="field__unit">sat/vB</span>}
            hint={`At least ${min} sat/vB: what it pays now plus 1 sat/vB, as nodes require for a replacement (BIP125).`}
            {...NO_ASSIST}
          />
          {!info.unlocked && (
            <p className="inline-note">
              <Icon name="lock" size={16} /> The wallet is locked. You&apos;ll be asked for your password when you review.
            </p>
          )}
          <div className="actions actions--start">
            <button type="submit" className="btn btn--primary" disabled={busy}>
              {busy ? "Preparing…" : "Review"}
            </button>
            <button type="button" className="btn btn--ghost" onClick={close} disabled={busy}>
              Cancel
            </button>
          </div>
        </form>
      )}
      <ErrorNotice error={error} network={info.network} />
    </section>
  );
}

/** The transaction's label: shown, added, edited or removed (watch-only, no password). */
function LabelRow({ txid, label }: { txid: string; label: string | null }) {
  const { info, refreshWallet } = useWallet();
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState(label ?? "");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const input = useRef<HTMLInputElement>(null);
  const editButton = useRef<HTMLButtonElement>(null);
  const refocus = useRef(false);

  useEffect(() => {
    if (editing) input.current?.focus();
    else if (refocus.current) {
      refocus.current = false;
      editButton.current?.focus();
    }
  }, [editing]);

  function stop() {
    refocus.current = true;
    setEditing(false);
    setError(null);
  }

  async function save(e: FormEvent) {
    e.preventDefault();
    if (busy) return;
    if (text.trim() === "") {
      setError(label ? "Enter a label, or use Remove label." : "Enter a label.");
      return;
    }
    setBusy(true);
    try {
      await api.setLabel(txid, text);
      await refreshWallet();
      setBusy(false);
      stop();
    } catch (err) {
      setBusy(false);
      setError(errorText(err, { network: info.network }));
    }
  }

  async function clear() {
    if (busy) return;
    setBusy(true);
    try {
      await api.clearLabel(txid);
      await refreshWallet();
      setBusy(false);
      editButton.current?.focus(); // "Remove label" is gone; "Add a label" takes its place
    } catch (err) {
      setBusy(false);
      setError(errorText(err, { network: info.network }));
    }
  }

  return (
    <div className="lines__row lines__row--stack">
      <dt>Label</dt>
      <dd>
        {editing ? (
          <form
            className="labelform"
            onSubmit={save}
            onKeyDown={(e) => {
              if (e.key === "Escape") {
                e.preventDefault();
                stop();
              }
            }}
            noValidate
          >
            <Field
              ref={input}
              label="Label for this transaction"
              value={text}
              onChange={(e) => {
                setText(e.target.value);
                setError(null);
              }}
              error={error}
              hint="Up to 100 characters. Kept in this wallet on this computer only."
              disabled={busy}
              {...NO_ASSIST}
            />
            <div className="actions actions--start">
              <button type="submit" className="btn btn--primary" disabled={busy}>
                {busy ? "Saving…" : "Save label"}
              </button>
              <button type="button" className="btn btn--ghost" onClick={stop} disabled={busy}>
                Cancel
              </button>
            </div>
          </form>
        ) : (
          <div className="labelview">
            {label ? (
              <span className="labelview__text">
                <Icon name="tag" size={14} />
                {label}
              </span>
            ) : (
              <span className="muted">No label</span>
            )}
            <span className="labelview__actions">
              <button
                ref={editButton}
                type="button"
                className="link"
                onClick={() => {
                  setText(label ?? "");
                  setEditing(true);
                }}
                disabled={busy}
              >
                {label ? "Edit label" : "Add a label"}
              </button>
              {label && (
                <button type="button" className="link" onClick={() => void clear()} disabled={busy}>
                  Remove label
                </button>
              )}
            </span>
            {error && <span className="field__error">{error}</span>}
          </div>
        )}
      </dd>
    </div>
  );
}

function StatusPanel({ status }: { status: TxStatus | null | undefined }) {
  if (status === undefined) {
    return (
      <section className="status" aria-label="Status">
        <p className="status__title">Checking status…</p>
      </section>
    );
  }
  if (status === null) {
    return (
      <section className="status status--missing" aria-label="Status">
        <p className="status__title">Not in this wallet</p>
        <p className="muted">This transaction isn&apos;t one of this wallet&apos;s. Check that you&apos;re on the right network.</p>
      </section>
    );
  }
  const confirmations = status.state === "confirmed" ? status.confirmations : 0;
  const filled = Math.min(confirmations, FINAL_CONFIRMATIONS);
  return (
    <section className={`status status--${status.state}`} aria-label="Status">
      <div className="status__row">
        <p className="status__title">
          {status.state === "confirmed" ? (
            <>
              Confirmed · {plural(status.confirmations, "confirmation", "confirmations")}
            </>
          ) : (
            "Unconfirmed · waiting in the mempool"
          )}
        </p>
        <span className="pips" aria-hidden="true">
          {Array.from({ length: FINAL_CONFIRMATIONS }, (_, i) => (
            <span key={i} className={i < filled ? "pip pip--on" : "pip"} />
          ))}
        </span>
      </div>
      <p className="muted">
        {status.state === "confirmed"
          ? `In block ${status.height}, mined ${formatDateTime(status.block_time)}. ${
              status.confirmations >= FINAL_CONFIRMATIONS
                ? "Six or more confirmations: treated as final."
                : `Usually treated as final after ${FINAL_CONFIRMATIONS} confirmations.`
            }`
          : `${status.first_seen ? `First seen ${formatDateTime(status.first_seen)}. ` : ""}It confirms when a miner includes it in a block.`}
      </p>
    </section>
  );
}
