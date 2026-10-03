import { plural, sortNewestFirst } from "../lib/format";
import type { Go } from "../state/nav";
import { useWallet } from "../state/wallet";
import { ErrorNotice } from "../components/ErrorNotice";
import { ScreenHeader } from "../components/Frame";
import { SyncStatus } from "../components/SyncStatus";
import { TxList } from "../components/TxList";

/** Every wallet transaction, newest first. */
export function History({ go }: { go: Go }) {
  const { history, dataError, info } = useWallet();
  const rows = history ? sortNewestFirst(history) : null;

  return (
    <div className="screen">
      <ScreenHeader
        title="History"
        lead={rows ? `${plural(rows.length, "transaction", "transactions")}, newest first.` : undefined}
      />
      <SyncStatus compact />
      {dataError !== null && !rows && <ErrorNotice error={dataError} network={info.network} />}
      {rows && rows.length > 0 && (
        <TxList rows={rows} label="All transactions" onOpen={(txid) => go({ name: "tx", txid })} />
      )}
      {rows && rows.length === 0 && (
        <div className="empty">
          <p>No transactions yet.</p>
          <button type="button" className="btn btn--quiet" onClick={() => go({ name: "receive" })}>
            Show my receive address
          </button>
        </div>
      )}
    </div>
  );
}
