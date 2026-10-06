import { useId, useState, type FormEvent } from "react";
import { NETWORKS } from "../lib/network";
import type { AppInfo, NetworkName, Settings } from "../lib/types";
import { useWallet } from "../state/wallet";
import { ErrorNotice } from "../components/ErrorNotice";
import { Field, NO_ASSIST } from "../components/Field";
import { ScreenHeader } from "../components/Frame";
import { Icon } from "../components/Icon";
import { NetworkBadge } from "../components/Network";
import { RevealPhrase } from "../components/RevealPhrase";
import { VerifyBackupDialog } from "../components/VerifyBackup";

const MIN_AUTO_LOCK = 1;
const MAX_AUTO_LOCK = 60;

interface Draft {
  network: NetworkName;
  rpcUrl: string;
  cookie: string;
  autoLock: string;
}

const draftOf = (s: Settings): Draft => ({
  network: s.network,
  rpcUrl: s.rpc_url ?? "",
  cookie: s.rpc_cookie ?? "",
  autoLock: String(s.auto_lock_minutes),
});

/** Network, node connection, auto-lock and the recovery phrase. Also reachable before a wallet exists. */
export function SettingsScreen({
  onboarding = false,
  onDone,
}: {
  onboarding?: boolean;
  /** Called after a successful save (with the new app info) or on Back. */
  onDone?: (info: AppInfo | null) => void;
}) {
  const wallet = useWallet();
  const { info, settings } = wallet;
  const [draft, setDraft] = useState<Draft>(() => draftOf(settings));
  const [errors, setErrors] = useState<{ rpcUrl?: string; autoLock?: string; mainnet?: string }>({});
  const [error, setError] = useState<unknown>(null);
  const [saving, setSaving] = useState(false);
  const [mainnetConfirm, setMainnetConfirm] = useState("");

  const current = NETWORKS[settings.network];
  const chosen = NETWORKS[draft.network];
  const networkChanged = draft.network !== settings.network;
  const dirty = JSON.stringify(draft) !== JSON.stringify(draftOf(settings));

  const update = (patch: Partial<Draft>) => {
    setDraft((d) => ({ ...d, ...patch }));
    setErrors({});
    setError(null);
  };

  async function save(e: FormEvent) {
    e.preventDefault();
    if (saving) return;
    const next: typeof errors = {};
    const minutes = Number(draft.autoLock);
    if (!/^\d+$/.test(draft.autoLock.trim()) || minutes < MIN_AUTO_LOCK || minutes > MAX_AUTO_LOCK) {
      next.autoLock = `Choose a whole number of minutes from ${MIN_AUTO_LOCK} to ${MAX_AUTO_LOCK}.`;
    }
    const url = draft.rpcUrl.trim();
    if (url !== "" && !/^https?:\/\/\S+$/.test(url)) {
      next.rpcUrl = `Use a full URL, like ${chosen.defaultRpcUrl}.`;
    }
    if (networkChanged && chosen.real && mainnetConfirm.trim() !== "MAINNET") {
      next.mainnet = "Type MAINNET to confirm that this wallet will hold real bitcoin.";
    }
    setErrors(next);
    if (Object.keys(next).length > 0) return;

    setSaving(true);
    try {
      const newInfo = await wallet.saveSettings({
        network: draft.network,
        rpc_url: url === "" ? null : url,
        rpc_cookie: draft.cookie.trim() === "" ? null : draft.cookie.trim(),
        auto_lock_minutes: minutes,
        // Typing MAINNET above is the runtime opt-in; it stays given.
        mainnet_opt_in: settings.mainnet_opt_in || (networkChanged && chosen.real),
      });
      setSaving(false);
      setMainnetConfirm("");
      wallet.notify("success", networkChanged ? `Switched to ${chosen.label}.` : "Settings saved.");
      onDone?.(newInfo);
    } catch (err) {
      setSaving(false);
      setError(err);
    }
  }

  return (
    <div className="screen">
      <ScreenHeader title="Settings" />
      <form className="stack" onSubmit={save} noValidate>
        <fieldset className="panel stack" disabled={saving}>
          <legend className="panel__legend">Network</legend>
          <div className="netpick">
            {info.networks.map((name) => {
              const meta = NETWORKS[name];
              return (
                <label key={name} className={`netpick__option ${draft.network === name ? "netpick__option--on" : ""}`}>
                  <input
                    type="radio"
                    name="network"
                    value={name}
                    checked={draft.network === name}
                    onChange={() => update({ network: name })}
                  />
                  <NetworkBadge network={name} />
                  <span className="netpick__text">{meta.coins}</span>
                </label>
              );
            })}
          </div>
          {networkChanged ? (
            <div className="notice notice--info" role="note">
              <Icon name="alert" className="notice__icon" />
              <div className="notice__body">
                <p className="notice__title">
                  {chosen.label} has its own wallet, separate from your {current.label} wallet.
                </p>
                <p className="notice__detail">
                  Each network keeps its wallet in its own folder. After switching, btcw opens the {chosen.label} wallet,
                  or asks you to create or restore one if there isn&apos;t one yet. Your {current.label} wallet stays as it
                  is, and comes back when you switch back.
                </p>
              </div>
            </div>
          ) : (
            <p className="muted small">
              Each network has its own separate wallet. Coins on one network can never be sent to another.
            </p>
          )}
          {networkChanged && chosen.real && (
            <div className="notice notice--danger" role="note">
              <Icon name="alert" className="notice__icon" />
              <div className="notice__body">
                <p className="notice__title">Mainnet holds real bitcoin. Mistakes cost real money.</p>
                <Field
                  label="Type MAINNET to confirm"
                  value={mainnetConfirm}
                  onChange={(e) => setMainnetConfirm(e.target.value)}
                  error={errors.mainnet}
                  {...NO_ASSIST}
                />
              </div>
            </div>
          )}
        </fieldset>

        <fieldset className="panel stack" disabled={saving}>
          <legend className="panel__legend">Your Bitcoin node</legend>
          <p className="muted small">
            btcw reads the chain and broadcasts transactions only through your own node (bitcoind). Leave these empty to use
            the defaults for {chosen.label}.
          </p>
          <Field
            label="RPC URL"
            value={draft.rpcUrl}
            onChange={(e) => update({ rpcUrl: e.target.value })}
            placeholder={chosen.defaultRpcUrl}
            error={errors.rpcUrl}
            mono
            {...NO_ASSIST}
          />
          <Field
            label="Cookie file"
            value={draft.cookie}
            onChange={(e) => update({ cookie: e.target.value })}
            placeholder={chosen.defaultCookie}
            hint="bitcoind writes this file when it starts; btcw uses it to log in without a password."
            mono
            {...NO_ASSIST}
          />
        </fieldset>

        <fieldset className="panel stack" disabled={saving}>
          <legend className="panel__legend">Security</legend>
          <Field
            label="Auto-lock after (minutes)"
            inputMode="numeric"
            value={draft.autoLock}
            onChange={(e) => update({ autoLock: e.target.value })}
            error={errors.autoLock}
            hint="With no clicks or typing for this long, btcw locks: the keys leave memory and sending needs the password again. Viewing still works."
            className="field--narrow"
            {...NO_ASSIST}
          />
          {!onboarding && info.wallet_exists && (
            <div className="row">
              <span className="muted small">
                {info.unlocked ? "The wallet is unlocked." : "The wallet is locked."}
              </span>
              <button type="button" className="btn btn--quiet" onClick={() => void wallet.lock("manual")} disabled={!info.unlocked}>
                <Icon name="lock" size={16} />
                Lock now
              </button>
            </div>
          )}
        </fieldset>

        <ErrorNotice error={error} network={draft.network} />
        <div className="actions">
          {onboarding && (
            <button type="button" className="btn btn--ghost" onClick={() => onDone?.(null)} disabled={saving}>
              Back
            </button>
          )}
          {dirty && !onboarding && (
            <button type="button" className="btn btn--ghost" onClick={() => setDraft(draftOf(settings))} disabled={saving}>
              Discard changes
            </button>
          )}
          <button type="submit" className="btn btn--primary" disabled={saving || !dirty}>
            {saving ? "Saving…" : networkChanged ? `Switch to ${chosen.label}` : "Save settings"}
          </button>
        </div>
      </form>
      {!onboarding && info.wallet_exists && <RecoveryPhrasePanel />}
    </div>
  );
}

/** The backup state, "Verify backup", and "Show recovery phrase" (password, any time). */
function RecoveryPhrasePanel() {
  const { info } = useWallet();
  const titleId = useId();
  const [revealing, setRevealing] = useState(false);
  const [verifying, setVerifying] = useState(false);

  return (
    <section className="panel stack" aria-labelledby={titleId}>
      <h2 className="panel__legend" id={titleId}>
        Recovery phrase
      </h2>
      <div className="row">
        <span className="muted small">
          {info.backup_verified
            ? "Backup verified: you've shown that your paper copy is complete and in order."
            : "Backup not verified yet. Check your paper copy so you know it can restore this wallet."}
        </span>
        {!info.backup_verified && (
          <button type="button" className="btn btn--quiet" onClick={() => setVerifying(true)}>
            Verify backup
          </button>
        )}
      </div>
      {revealing ? (
        <RevealPhrase onClose={() => setRevealing(false)} />
      ) : (
        <div className="row">
          <span className="muted small">See the words again, for example to make a second copy. It needs your password.</span>
          <button type="button" className="btn btn--quiet" onClick={() => setRevealing(true)}>
            Show recovery phrase
          </button>
        </div>
      )}
      {verifying && (
        <VerifyBackupDialog
          onClose={() => setVerifying(false)}
          onShowWords={() => {
            setVerifying(false);
            setRevealing(true);
          }}
        />
      )}
    </section>
  );
}
