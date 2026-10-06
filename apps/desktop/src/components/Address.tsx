import { chunk } from "../lib/format";

/**
 * An address in groups of four characters, for checking by eye. The gaps are CSS margins, so
 * copying still gives the exact address.
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
