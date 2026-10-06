import { NETWORKS } from "../lib/network";
import type { NetworkName } from "../lib/types";

/**
 * The network badge, on every screen so test coins are never mistaken for real ones. Mainnet
 * is red and says "real bitcoin".
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
