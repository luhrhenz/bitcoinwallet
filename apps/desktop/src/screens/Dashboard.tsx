import { formatSat } from "../lib/amount";
import { sortNewestFirst } from "../lib/format";
import type { Go } from "../state/nav";
import { useWallet } from "../state/wallet";
import { Btc } from "../components/Amount";
import { BackupReminder } from "../components/BackupReminder";
import { ErrorNotice } from "../components/ErrorNotice";
import { Icon } from "../components/Icon";
import { SyncStatus } from "../components/SyncStatus";
import { TxList } from "../components/TxList";

const RECENT = 5;

/** Balances, sync state and recent activity. Works while locked (watch-only). */
export function Dashboard({ go }: { go: Go }) {
  const { balance, history, dataError, info } = useWallet();
  const recent = history ? sortNewestFirst(history).slice(0, RECENT) : null;

  return (
    <div className="screen">
      <BackupReminder />
      <section className="balance" aria-labelledby="balance-title">
        <div className="balance__head">
          <h1 className="eyebrow" id="balance-title">
            Balance
          </h1>
          <div className="balance__actions">
            <button type="button" className="btn btn--quiet" onClick={() => go({ name: "receive" })}>
              <Icon name="in" size={16} />
              Receive
            </button>
            <button type="button" className="btn btn--primary" onClick={() => go({ name: "send" })}>
              <Icon name="out" size={16} />
              Send
            </button>
          </div>
        </div>
        {balance ? (
          <>
            <p className="balance__total">
              <Btc sat={balance.total_sat} />
            </p>
            <p className="balance__sat">{formatSat(balance.total_sat)}</p>
            <dl className="balance__parts">
              <div>
                <dt>Confirmed</dt>
                <dd>
                  <Btc sat={balance.confirmed_sat} />
                </dd>
              </div>
              <div>
                <dt>Unconfirmed</dt>
                <dd>
                  <Btc sat={balance.unconfirmed_sat} />
                </dd>
              </div>
              {balance.immature_sat > 0 && (
                <div>
                  <dt>
                    Immature <span className="dt-note">mined; spendable after 100 confirmations</span>
                  </dt>
                  <dd>
                    <Btc sat={balance.immature_sat} />
                  </dd>
                </div>
              )}
            </dl>
          </>
        ) : dataError ? (
          <ErrorNotice error={dataError} network={info.network} />
        ) : (
          <p className="balance__total balance__total--loading" aria-busy="true">
            Loading…
          </p>
        )}
        <SyncStatus />
      </section>

      <section className="activity" aria-labelledby="recent-title">
        <div className="section-head">
          <h2 className="eyebrow" id="recent-title">
            Recent activity
          </h2>
          {history && history.length > RECENT && (
            <button type="button" className="link" onClick={() => go({ name: "history" })}>
              All {history.length} transactions
            </button>
          )}
        </div>
        {recent && recent.length > 0 && (
          <TxList rows={recent} label="Recent transactions" onOpen={(txid) => go({ name: "tx", txid })} />
        )}
        {recent && recent.length === 0 && (
          <div className="empty">
            <p>No transactions yet.</p>
            <p className="muted">
              Share a receive address to get your first coins. On {info.network === "regtest" ? "regtest" : "a test network"}, a
              faucet or your own node can send some.
            </p>
            <button type="button" className="btn btn--quiet" onClick={() => go({ name: "receive" })}>
              Show my address
            </button>
          </div>
        )}
      </section>
    </div>
  );
}
