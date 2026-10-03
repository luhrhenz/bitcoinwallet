import { useEffect, useRef, useState } from "react";
import { api } from "../lib/api";
import { formatSat } from "../lib/amount";
import { direction, formatDateTime, plural, timeAgo } from "../lib/format";
import type { SendPreview, TxRow, TxStatus } from "../lib/types";
import type { Go } from "../state/nav";
import { useWallet } from "../state/wallet";
import { AddressChunks } from "../components/Address";
import { Btc } from "../components/Amount";
import { CopyButton } from "../components/CopyButton";
import { ErrorNotice } from "../components/ErrorNotice";
import { Icon } from "../components/Icon";

/** How often the open screen asks for the status (PLAN §4.2). */
export const POLL_MS = 10_000;
/** Six blocks is the customary point where a payment is treated as final. */
const FINAL_CONFIRMATIONS = 6;

const statusKey = (s: TxStatus | null) =>
  s === null ? "none" : s.state === "confirmed" ? `c${s.height}:${s.confirmations}` : "u";

/** One transaction, with its status polled every 10 s while the screen is open. */
export function TxDetail({ txid, sent, go }: { txid: string; sent?: SendPreview; go: Go }) {
  const { history, info, refreshWallet } = useWallet();
  const row: TxRow | undefined = history?.find((tx) => tx.txid === txid);
  const [status, setStatus] = useState<TxStatus | null | undefined>(row?.status);
  const [pollError, setPollError] = useState<unknown>(null);
  const [checkedAt, setCheckedAt] = useState<number | null>(null);
  const lastKey = useRef<string | null>(null);
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
            <p className="notice__title">Sent. Your transaction is on its way.</p>
            <p className="notice__detail">
              It waits in the mempool until a miner puts it in a block. This screen updates on its own.
            </p>
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

      <dl className="lines lines--detail">
        <div className="lines__row lines__row--stack">
          <dt>Transaction ID</dt>
          <dd className="txid">
            <span className="mono break">{txid}</span>
            <CopyButton text={txid} label="Copy ID" />
          </dd>
        </div>
        {sent && (
          <div className="lines__row lines__row--stack">
            <dt>Sent to</dt>
            <dd>
              <AddressChunks address={sent.to} />
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
