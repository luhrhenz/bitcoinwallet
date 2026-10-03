import { chunk } from "../lib/format";

/**
 * An address in groups of four characters, so a person can compare it with the one they were
 * given group by group, like a hardware wallet screen. The gaps are CSS margins, not spaces:
 * selecting and copying still gives the exact address, and a screen reader reads it group by
 * group.
 */
export function AddressChunks({ address, size = "md" }: { address: string; size?: "md" | "lg" }) {
  return (
    <span className={`addr addr--${size}`}>
      {chunk(address).map((group, i) => (
        <span key={i} className="addr__group">
          {group}
        </span>
      ))}
    </span>
  );
}
