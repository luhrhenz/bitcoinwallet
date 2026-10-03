import { NETWORKS } from "../lib/network";
import type { NetworkName } from "../lib/types";

/**
 * The network badge (PLAN §4.2): on every screen, so test coins are never mistaken for real
 * ones. Each network has its own color; mainnet is red and says "real bitcoin".
 */
export function NetworkBadge({ network }: { network: NetworkName }) {
  const meta = NETWORKS[network];
  return (
    <span className={`net-badge net-badge--${network}`} title={meta.coins} data-network={network}>
      <span className="net-badge__dot" aria-hidden="true" />
      <span className="sr-only">Network: </span>
      {meta.badge}
    </span>
  );
}

/**
 * The tape down the left edge of the window: the network color, always in view, whatever is
 * scrolled. Hatched for test networks (a test environment), solid red for mainnet.
 */
export function NetworkTape({ network }: { network: NetworkName }) {
  const meta = NETWORKS[network];
  const text = meta.real ? `${meta.badge} · REAL BITCOIN · ` : `${meta.badge} · TEST COINS · `;
  return (
    <div className={`net-tape net-tape--${network}`} aria-hidden="true">
      <span className="net-tape__text">{text.repeat(12)}</span>
    </div>
  );
}
