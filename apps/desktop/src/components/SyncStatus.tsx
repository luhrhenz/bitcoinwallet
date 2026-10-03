import { timeAgo } from "../lib/format";
import { useWallet } from "../state/wallet";
import { ErrorNotice } from "./ErrorNotice";
import { Icon } from "./Icon";

/** "as of block N", the Sync button, and a progress bar fed by `sync-progress` events. */
export function SyncStatus({ compact = false }: { compact?: boolean }) {
  const { info, sync, runSync } = useWallet();
  const height = info.synced_height ?? 0;
  const progress = sync.progress;
  const start = sync.base ?? height;

  let percent = 0;
  if (progress) {
    const span = progress.tip_height - start;
    percent = span > 0 ? Math.min(100, Math.max(0, ((progress.height - start) / span) * 100)) : 100;
  }

  return (
    <section className={`sync ${compact ? "sync--compact" : ""}`} aria-label="Sync status">
      <div className="sync__row">
        <p className="sync__height">
          {sync.running ? (
            progress ? (
              <>
                Syncing block <strong>{progress.height}</strong> of {progress.tip_height}
              </>
            ) : (
              "Connecting to your node…"
            )
          ) : height === 0 ? (
            "Not synced yet"
          ) : (
            <>
              As of block <strong>{height}</strong>
              {sync.finishedAt && <span className="muted"> · synced {timeAgo(sync.finishedAt / 1000)}</span>}
            </>
          )}
        </p>
        <button type="button" className="btn btn--quiet" onClick={() => void runSync()} disabled={sync.running}>
          <Icon name="sync" size={16} className={sync.running ? "spin" : undefined} />
          {sync.running ? "Syncing…" : "Sync now"}
        </button>
      </div>
      {sync.running && (
        <div
          className="progress"
          role="progressbar"
          aria-label="Sync progress"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.round(percent)}
        >
          <span className="progress__fill" style={{ width: `${progress ? percent : 4}%` }} />
        </div>
      )}
      {!sync.running && sync.error !== null && (
        <ErrorNotice error={sync.error} network={info.network} className="sync__error">
          <p className="notice__detail">
            {height > 0
              ? `Balances below are from block ${height}, the last successful sync.`
              : "Nothing has been synced yet, so balances may be missing."}
          </p>
        </ErrorNotice>
      )}
    </section>
  );
}
