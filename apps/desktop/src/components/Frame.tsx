import type { ReactNode } from "react";
import { NETWORKS } from "../lib/network";
import type { NetworkName } from "../lib/types";
import { NetworkBadge, NetworkTape } from "./Network";

/** The window: network tape on the left, top bar, scrolling content. Every screen sits in one. */
export function Frame({
  network,
  nav,
  status,
  children,
}: {
  network: NetworkName;
  nav?: ReactNode;
  status?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className={`frame frame--${network}`}>
      <NetworkTape network={network} />
      <div className="frame__body">
        <header className="topbar">
          <span className="brand">
            <span className="brand__mark" aria-hidden="true" />
            <span className="brand__name">btcw</span>
          </span>
          {nav}
          <div className="topbar__end">
            <NetworkBadge network={network} />
            {status}
          </div>
        </header>
        {NETWORKS[network].real && (
          <p className="mainnet-banner" role="note">
            Mainnet: this wallet holds real bitcoin. Check every address and amount twice.
          </p>
        )}
        <main className="content" id="main">
          {children}
        </main>
      </div>
    </div>
  );
}

/** Screen title row. */
export function ScreenHeader({ title, lead, children }: { title: string; lead?: ReactNode; children?: ReactNode }) {
  return (
    <div className="screen-head">
      <div>
        <h1 className="screen-head__title">{title}</h1>
        {lead && <p className="screen-head__lead">{lead}</p>}
      </div>
      {children && <div className="screen-head__actions">{children}</div>}
    </div>
  );
}
