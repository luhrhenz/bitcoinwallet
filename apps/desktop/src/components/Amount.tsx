import { formatSat, formatSignedSat, splitBtc } from "../lib/amount";

/**
 * A BTC amount with all 8 decimals, trailing zeros dimmed: `0.0125`**`0000`** `BTC`.
 * Copying or reading it out gives the full `0.01250000 BTC`.
 */
export function Btc({ sat, signed = false, className }: { sat: number; signed?: boolean; className?: string }) {
  const { sign, significant, zeros } = splitBtc(sat);
  const shownSign = signed ? (sat > 0 ? "+" : sign) : sign;
  return (
    <span className={className ? `btc ${className}` : "btc"}>
      {shownSign}
      {significant}
      {zeros && <span className="btc__zeros">{zeros}</span>}
      <span className="btc__unit"> BTC</span>
    </span>
  );
}

export function Sat({ sat, signed = false }: { sat: number; signed?: boolean }) {
  return <span className="sat">{signed ? formatSignedSat(sat) : formatSat(sat)}</span>;
}
