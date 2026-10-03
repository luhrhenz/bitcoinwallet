import { useState } from "react";
import type { MockControls } from "../lib/mock";
import { useWallet } from "../state/wallet";

/**
 * Demo controls, shown only when the in-memory mock backend is running (`npm run dev:mock`
 * or a plain browser). The real app never renders this.
 */
export function MockPanel({ mock }: { mock: MockControls }) {
  const { info } = useWallet();
  const [online, setOnline] = useState(mock.isNodeOnline());
  const [open, setOpen] = useState(false);
  const [last, setLast] = useState<string | null>(null);

  function receive(sat: number) {
    try {
      mock.simulateIncoming(sat);
      setLast("A payment is in the mempool. Sync to see it.");
    } catch {
      setLast("Create or restore a wallet first.");
    }
  }

  function mine(toWallet = false) {
    const tip = mock.mineBlocks(1, { toWallet });
    setLast(`Mined block ${tip}${toWallet ? " paying the wallet" : ""}. Sync to see it.`);
  }

  return (
    <aside className={`mockpanel ${open ? "mockpanel--open" : ""}`} aria-label="Mock backend controls">
      <button type="button" className="mockpanel__toggle" aria-expanded={open} onClick={() => setOpen(!open)}>
        Demo mode: mock backend, not a real wallet
      </button>
      {open && (
        <div className="mockpanel__body">
          <p className="mockpanel__note">
            Everything here lives in this page&apos;s memory: no keys, no node, no network. Never send real coins to an
            address shown in demo mode.
          </p>
          <div className="mockpanel__grid">
            <button type="button" className="btn btn--quiet" disabled={!info.wallet_exists} onClick={() => receive(1_000_000)}>
              Receive 0.01 BTC
            </button>
            <button type="button" className="btn btn--quiet" disabled={!info.wallet_exists} onClick={() => receive(50_000)}>
              Receive 50,000 sat
            </button>
            <button type="button" className="btn btn--quiet" onClick={() => mine()}>
              Mine a block
            </button>
            {info.network === "regtest" && (
              <button type="button" className="btn btn--quiet" disabled={!info.wallet_exists} onClick={() => mine(true)}>
                Mine to wallet
              </button>
            )}
            <button
              type="button"
              className="btn btn--quiet"
              aria-pressed={!online}
              onClick={() => {
                mock.setNodeOnline(!online);
                setOnline(!online);
              }}
            >
              {online ? "Stop the node" : "Start the node"}
            </button>
          </div>
          {last && (
            <p className="mockpanel__last" aria-live="polite">
              {last}
            </p>
          )}
        </div>
      )}
    </aside>
  );
}
