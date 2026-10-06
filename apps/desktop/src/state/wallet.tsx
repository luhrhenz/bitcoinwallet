// Wallet state shared by every screen: what the backend told us last, and the actions that
// change it.

import { createContext, useCallback, useContext, useMemo, useRef, useState, type ReactNode } from "react";
import { api } from "../lib/api";
import type { AppInfo, BalanceView, Settings, SyncProgress, SyncReport, TxRow } from "../lib/types";
import { UnlockDialog } from "../components/UnlockDialog";
import { useAutoLock } from "./useAutoLock";

export interface SyncState {
  running: boolean;
  progress: SyncProgress | null;
  /** Height this run started from, so a progress bar measures this run, not the whole chain. */
  base: number | null;
  /** The last sync's failure, cleared by the next success. */
  error: unknown;
  lastReport: SyncReport | null;
  /** When the last successful sync finished (ms). */
  finishedAt: number | null;
}

export interface Notice {
  id: number;
  tone: "info" | "success" | "error";
  text: string;
}

export interface WalletContextValue {
  info: AppInfo;
  settings: Settings;
  balance: BalanceView | null;
  history: TxRow[] | null;
  /** Failure of the last balance/history refresh. */
  dataError: unknown;
  sync: SyncState;
  notice: Notice | null;
  notify(tone: Notice["tone"], text: string): void;
  dismissNotice(): void;
  refreshInfo(): Promise<AppInfo>;
  refreshWallet(): Promise<void>;
  /** Sync with the node, streaming progress. Resolves to null if it failed (see `sync.error`). */
  runSync(): Promise<SyncReport | null>;
  unlock(password: string): Promise<void>;
  lock(reason?: "manual" | "auto"): Promise<void>;
  /** Ask for the password (dialog). True once unlocked, false if the user cancelled. */
  requestUnlock(): Promise<boolean>;
  /** Save settings; resolves to the app info after the change (the network may differ). */
  saveSettings(next: Settings): Promise<AppInfo>;
}

const WalletContext = createContext<WalletContextValue | null>(null);

export function useWallet(): WalletContextValue {
  const value = useContext(WalletContext);
  if (!value) throw new Error("useWallet outside <WalletProvider>");
  return value;
}

const IDLE_SYNC: SyncState = { running: false, progress: null, base: null, error: null, lastReport: null, finishedAt: null };

export function WalletProvider({
  initialInfo,
  initialSettings,
  children,
}: {
  initialInfo: AppInfo;
  initialSettings: Settings;
  children: ReactNode;
}) {
  const [info, setInfo] = useState(initialInfo);
  const [settings, setSettings] = useState(initialSettings);
  const [balance, setBalance] = useState<BalanceView | null>(null);
  const [history, setHistory] = useState<TxRow[] | null>(null);
  const [dataError, setDataError] = useState<unknown>(null);
  const [sync, setSync] = useState<SyncState>(IDLE_SYNC);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [unlockOpen, setUnlockOpen] = useState(false);

  const infoRef = useRef(info);
  infoRef.current = info;
  const syncing = useRef<Promise<SyncReport | null> | null>(null);
  const unlockWaiters = useRef<((ok: boolean) => void)[]>([]);
  const noticeId = useRef(0);

  const notify = useCallback((tone: Notice["tone"], text: string) => {
    noticeId.current += 1;
    setNotice({ id: noticeId.current, tone, text });
  }, []);
  const dismissNotice = useCallback(() => setNotice(null), []);

  const refreshInfo = useCallback(async () => {
    const next = await api.appInfo();
    infoRef.current = next; // callers may act on it before the next render
    setInfo(next);
    return next;
  }, []);

  const refreshWallet = useCallback(async () => {
    if (!infoRef.current.wallet_exists) return;
    try {
      const [nextBalance, nextHistory, nextInfo] = await Promise.all([api.balance(), api.history(), api.appInfo()]);
      setBalance(nextBalance);
      setHistory(nextHistory);
      infoRef.current = nextInfo;
      setInfo(nextInfo);
      setDataError(null);
    } catch (e) {
      setDataError(e);
    }
  }, []);

  const runSync = useCallback(() => {
    // One sync at a time; a second request joins the running one.
    if (syncing.current) return syncing.current;
    const run = (async () => {
      setSync((s) => ({ ...s, running: true, progress: null, base: null }));
      let unlisten: (() => void) | null = null;
      try {
        unlisten = await api.onSyncProgress((progress) =>
          setSync((s) => ({
            ...s,
            progress,
            // The wallet's synced height, or (first sync) wherever the first event lands.
            base: s.base ?? Math.min(infoRef.current.synced_height || progress.height, progress.height),
          })),
        );
        const report = await api.sync();
        setSync({ running: false, progress: null, base: null, error: null, lastReport: report, finishedAt: Date.now() });
        await refreshWallet();
        return report;
      } catch (e) {
        setSync((s) => ({ ...s, running: false, progress: null, base: null, error: e }));
        return null;
      } finally {
        unlisten?.();
        syncing.current = null;
      }
    })();
    syncing.current = run;
    return run;
  }, [refreshWallet]);

  const settleUnlock = useCallback((ok: boolean) => {
    setUnlockOpen(false);
    const waiters = unlockWaiters.current;
    unlockWaiters.current = [];
    for (const resolve of waiters) resolve(ok);
  }, []);

  const unlock = useCallback(
    async (password: string) => {
      await api.unlock(password);
      await refreshInfo();
    },
    [refreshInfo],
  );

  const requestUnlock = useCallback(() => {
    if (infoRef.current.unlocked) return Promise.resolve(true);
    return new Promise<boolean>((resolve) => {
      unlockWaiters.current.push(resolve);
      setUnlockOpen(true);
    });
  }, []);

  const lock = useCallback(
    async (reason: "manual" | "auto" = "manual") => {
      try {
        await api.lock();
        await refreshInfo();
        if (reason === "auto") {
          const minutes = settings.auto_lock_minutes;
          notify(
            "info",
            `Locked after ${minutes === 1 ? "1 minute" : `${minutes} minutes`} without activity. Viewing still works; sending needs your password.`,
          );
        } else {
          notify("info", "Wallet locked. Sending needs your password again.");
        }
      } catch {
        notify("error", "The wallet couldn't be locked. Close the app to be sure the keys are out of memory.");
      }
    },
    [notify, refreshInfo, settings.auto_lock_minutes],
  );

  useAutoLock(
    info.unlocked,
    settings.auto_lock_minutes,
    () => {
      void lock("auto");
    },
    () => {
      // Best effort: on failure the backend just locks a little earlier.
      void api.keepAlive().catch(() => undefined);
    },
  );

  const saveSettings = useCallback(
    async (next: Settings) => {
      const networkChanged = next.network !== settings.network;
      await api.setSettings(next);
      setSettings(await api.getSettings());
      if (networkChanged) {
        // A different network is a different wallet: forget everything shown for the old one.
        setBalance(null);
        setHistory(null);
        setDataError(null);
        setSync(IDLE_SYNC);
      }
      return refreshInfo();
    },
    [refreshInfo, settings.network],
  );

  const value = useMemo<WalletContextValue>(
    () => ({
      info,
      settings,
      balance,
      history,
      dataError,
      sync,
      notice,
      notify,
      dismissNotice,
      refreshInfo,
      refreshWallet,
      runSync,
      unlock,
      lock,
      requestUnlock,
      saveSettings,
    }),
    [info, settings, balance, history, dataError, sync, notice, notify, dismissNotice, refreshInfo, refreshWallet, runSync, unlock, lock, requestUnlock, saveSettings],
  );

  return (
    <WalletContext.Provider value={value}>
      <div className="app-root" inert={unlockOpen}>
        {children}
      </div>
      {unlockOpen && <UnlockDialog onUnlock={unlock} onDone={settleUnlock} />}
    </WalletContext.Provider>
  );
}
