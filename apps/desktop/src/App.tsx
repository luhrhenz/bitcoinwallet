// The app shell and screen routing. Screens talk to the backend only through `api` (lib/api.ts).

import { useCallback, useEffect, useState } from "react";
import { api } from "./lib/api";
import type { MockControls } from "./lib/mock";
import type { AppInfo, Settings } from "./lib/types";
import type { Route } from "./state/nav";
import { useWallet, WalletProvider } from "./state/wallet";
import { ErrorNotice } from "./components/ErrorNotice";
import { Frame } from "./components/Frame";
import { Icon } from "./components/Icon";
import { MockPanel } from "./components/MockPanel";
import { Assistant } from "./screens/Assistant";
import { Contacts } from "./screens/Contacts";
import { CreateWallet } from "./screens/CreateWallet";
import { Dashboard } from "./screens/Dashboard";
import { History } from "./screens/History";
import { Receive } from "./screens/Receive";
import { RestoreWallet } from "./screens/RestoreWallet";
import { Send } from "./screens/Send";
import { SettingsScreen } from "./screens/Settings";
import { TxDetail } from "./screens/TxDetail";
import { Welcome } from "./screens/Welcome";

type Boot =
  | { state: "loading" }
  | { state: "error"; error: unknown }
  | { state: "ready"; info: AppInfo; settings: Settings };

/** `mock` is passed only when the in-memory backend is in use; it adds the demo panel. */
export function App({ mock }: { mock?: MockControls }) {
  const [boot, setBoot] = useState<Boot>({ state: "loading" });

  const load = useCallback(async () => {
    setBoot({ state: "loading" });
    try {
      const [info, settings] = await Promise.all([api.appInfo(), api.getSettings()]);
      setBoot({ state: "ready", info, settings });
    } catch (error) {
      setBoot({ state: "error", error });
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  if (boot.state === "loading") {
    return (
      <div className="splash" aria-busy="true">
        <span className="brand__mark" aria-hidden="true" />
        <p>Opening btcw…</p>
      </div>
    );
  }
  if (boot.state === "error") {
    return (
      <div className="splash">
        <ErrorNotice error={boot.error} />
        <button type="button" className="btn btn--primary" onClick={() => void load()}>
          Try again
        </button>
      </div>
    );
  }
  return (
    <WalletProvider initialInfo={boot.info} initialSettings={boot.settings}>
      <Root />
      {mock && <MockPanel mock={mock} />}
    </WalletProvider>
  );
}

function Root() {
  const { info } = useWallet();
  const [mode, setMode] = useState<"onboarding" | "main">(info.wallet_exists ? "main" : "onboarding");

  // A network with no wallet (e.g. just switched to in Settings) starts at Welcome.
  useEffect(() => {
    if (!info.wallet_exists) setMode("onboarding");
  }, [info.wallet_exists]);

  return mode === "onboarding" ? (
    <Onboarding key={info.network} onFinished={() => setMode("main")} />
  ) : (
    <Main key={info.network} />
  );
}

function Onboarding({ onFinished }: { onFinished: () => void }) {
  const { info } = useWallet();
  const [step, setStep] = useState<"welcome" | "create" | "restore" | "settings">("welcome");
  const toWelcome = () => setStep("welcome");

  return (
    <Frame network={info.network}>
      <Notices />
      {step === "welcome" && (
        <Welcome onCreate={() => setStep("create")} onRestore={() => setStep("restore")} onSettings={() => setStep("settings")} />
      )}
      {step === "create" && <CreateWallet onBack={toWelcome} onFinished={onFinished} />}
      {step === "restore" && <RestoreWallet onBack={toWelcome} onFinished={onFinished} />}
      {step === "settings" && (
        <SettingsScreen
          onboarding
          onDone={(next) => (next?.wallet_exists ? onFinished() : toWelcome())}
        />
      )}
    </Frame>
  );
}

const NAV: { name: "dashboard" | "receive" | "send" | "history" | "contacts" | "assistant" | "settings"; label: string }[] = [
  { name: "dashboard", label: "Overview" },
  { name: "receive", label: "Receive" },
  { name: "send", label: "Send" },
  { name: "history", label: "History" },
  { name: "contacts", label: "Contacts" },
  { name: "assistant", label: "Assistant" },
  { name: "settings", label: "Settings" },
];

function Main() {
  const wallet = useWallet();
  const { info } = wallet;
  const [route, setRoute] = useState<Route>({ name: "dashboard" });

  // On open: refresh what's on disk, then sync with the node.
  const { refreshWallet, runSync } = wallet;
  useEffect(() => {
    void refreshWallet().then(() => runSync());
  }, [refreshWallet, runSync]);

  const { dismissNotice } = wallet;
  const go = useCallback(
    (next: Route) => {
      dismissNotice();
      setRoute(next);
      document.documentElement.scrollTop = 0;
    },
    [dismissNotice],
  );

  const active = route.name === "tx" ? "history" : route.name;

  return (
    <Frame
      network={info.network}
      nav={
        <nav className="nav" aria-label="Main">
          {NAV.map((item) => (
            <button
              key={item.name}
              type="button"
              className="nav__item"
              aria-current={active === item.name ? "page" : undefined}
              onClick={() => go({ name: item.name })}
            >
              {item.label}
            </button>
          ))}
        </nav>
      }
      status={<LockState />}
    >
      <Notices />
      {route.name === "dashboard" && <Dashboard go={go} />}
      {route.name === "receive" && <Receive />}
      {route.name === "send" && <Send go={go} initialTo={route.to} />}
      {route.name === "history" && <History go={go} />}
      {route.name === "contacts" && <Contacts go={go} />}
      {route.name === "assistant" && <Assistant go={go} />}
      {route.name === "settings" && <SettingsScreen />}
      {route.name === "tx" && <TxDetail key={route.txid} txid={route.txid} sent={route.sent} go={go} />}
    </Frame>
  );
}

/** Locked/unlocked indicator that is also the lock and unlock button. */
function LockState() {
  const { info, lock, requestUnlock } = useWallet();
  return info.unlocked ? (
    <button type="button" className="lockstate lockstate--open" onClick={() => void lock("manual")}>
      <Icon name="unlock" size={16} />
      <span className="lockstate__state">Unlocked</span>
      <span className="lockstate__action">Lock</span>
    </button>
  ) : (
    <button type="button" className="lockstate" onClick={() => void requestUnlock()}>
      <Icon name="lock" size={16} />
      <span className="lockstate__state">Locked</span>
      <span className="lockstate__action">Unlock</span>
    </button>
  );
}

function Notices() {
  const { notice, dismissNotice } = useWallet();
  if (!notice) return null;
  return (
    <div
      key={notice.id}
      className={`toast toast--${notice.tone}`}
      role={notice.tone === "error" ? "alert" : "status"}
    >
      <p>{notice.text}</p>
      <button type="button" className="toast__close" onClick={dismissNotice} aria-label="Dismiss">
        <Icon name="close" size={16} />
      </button>
    </div>
  );
}
