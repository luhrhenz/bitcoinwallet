import { useId, useState } from "react";
import { useWallet } from "../state/wallet";
import { Icon } from "./Icon";
import { RevealPhraseDialog, VerifyBackupDialog } from "./VerifyBackup";

/** Shown on the dashboard and the send screen until the backup has been verified. */
export function BackupReminder() {
  const { info } = useWallet();
  const titleId = useId();
  const [dialog, setDialog] = useState<null | "verify" | "reveal">(null);
  if (info.backup_verified !== false) return null;

  return (
    <>
      <section className="notice notice--info reminder" aria-labelledby={titleId}>
        <Icon name="check" className="notice__icon" />
        <div className="notice__body">
          <h2 className="notice__title" id={titleId}>
            Check your recovery phrase backup
          </h2>
          <p className="notice__detail">
            The words you wrote down are the only way to get these coins back if this computer is lost or breaks.
            Checking three of them takes a minute.
          </p>
          <div className="notice__actions">
            <button type="button" className="btn btn--primary" onClick={() => setDialog("verify")}>
              Verify backup
            </button>
            <button type="button" className="btn btn--quiet" onClick={() => setDialog("reveal")}>
              Show the words again
            </button>
          </div>
        </div>
      </section>
      {dialog === "verify" && (
        <VerifyBackupDialog onClose={() => setDialog(null)} onShowWords={() => setDialog("reveal")} />
      )}
      {dialog === "reveal" && <RevealPhraseDialog onClose={() => setDialog(null)} />}
    </>
  );
}
