import { direction, formatDateTime, shortId, statusLabel, txTime } from "../lib/format";
import type { TxRow } from "../lib/types";
import { Btc } from "./Amount";
import { Icon } from "./Icon";

const LABEL = { received: "Received", sent: "Sent", self: "To yourself" } as const;
const ICON = { received: "in", sent: "out", self: "self" } as const;

/** Transactions, newest first; each row opens its detail screen. */
export function TxList({ rows, onOpen, label }: { rows: TxRow[]; onOpen: (txid: string) => void; label: string }) {
  return (
    <ul className="txlist" aria-label={label}>
      {rows.map((tx) => {
        const dir = direction(tx.net_sat);
        const time = txTime(tx);
        const pending = tx.status.state === "unconfirmed";
        return (
          <li key={tx.txid}>
            <button type="button" className={`txrow txrow--${dir}`} onClick={() => onOpen(tx.txid)}>
              <span className="txrow__icon">
                <Icon name={ICON[dir]} size={16} />
              </span>
              <span className="txrow__main">
                <span className="txrow__kind">{LABEL[dir]}</span>
                <span className="txrow__meta">
                  {time ? formatDateTime(time) : "Time unknown"} · <span className="mono">{shortId(tx.txid)}</span>
                </span>
              </span>
              <span className="txrow__amount">
                <Btc sat={tx.net_sat} signed />
              </span>
              <span className={`txrow__status ${pending ? "txrow__status--pending" : ""}`}>
                {statusLabel(tx.status)}
              </span>
              <Icon name="chevron" size={16} className="txrow__chevron" />
            </button>
          </li>
        );
      })}
    </ul>
  );
}
