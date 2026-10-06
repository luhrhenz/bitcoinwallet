// Amounts: integer satoshis everywhere, strings only at the edges.
//
// Every amount the backend sends is an integer number of satoshis (a u64 in Rust). The total
// supply is 2.1e15 sat, well inside JavaScript's exact-integer range (2^53 ≈ 9.007e15), so a
// plain `number` holds any real amount exactly. What must never happen is BTC arithmetic on
// floats: `0.1 + 0.2 === 0.30000000000000004` and `4.35 * 1e8 === 434999999.99999994`.
// So BTC strings are produced with integer division and parsed digit by digit (BigInt), the
// same way the CLI's `output::btc` does it.

export const SAT_PER_BTC = 100_000_000;
/** 21 million BTC, the most any amount can be. */
export const MAX_MONEY_SAT = 21_000_000 * SAT_PER_BTC;
/** Smallest P2WPKH output the network relays (btcw-core `tx::DUST_LIMIT_P2WPKH`). */
export const DUST_LIMIT_SAT = 294;

export type AmountUnit = "btc" | "sat";

function assertSat(sat: number): void {
  if (!Number.isSafeInteger(sat)) {
    throw new RangeError(`not an integer satoshi amount: ${sat}`);
  }
}

/** `1234567` → `"1,234,567"` (same as the CLI's `group_thousands`). */
export function groupThousands(n: number): string {
  assertSat(n);
  const digits = Math.abs(n).toString();
  const head = digits.length % 3 || 3;
  let out = digits.slice(0, head);
  for (let i = head; i < digits.length; i += 3) out += "," + digits.slice(i, i + 3);
  return n < 0 ? "-" + out : out;
}

/** `1_000_000` → `"0.01000000"`: always 8 decimals, integer math (CLI `output::btc`). */
export function formatBtc(sat: number): string {
  assertSat(sat);
  const abs = Math.abs(sat);
  const whole = Math.floor(abs / SAT_PER_BTC);
  const frac = (abs % SAT_PER_BTC).toString().padStart(8, "0");
  return `${sat < 0 ? "-" : ""}${whole}.${frac}`;
}

/** `1_000_000` → `"1,000,000 sat"` */
export function formatSat(sat: number): string {
  return `${groupThousands(sat)} sat`;
}

/** `"0.01000000 BTC (1,000,000 sat)"`, exactly like the CLI's `output::amount`. */
export function formatAmount(sat: number): string {
  return `${formatBtc(sat)} BTC (${formatSat(sat)})`;
}

/** `+0.01000000`, `-0.00002000`; zero has no sign (CLI `signed_amount`). */
export function formatSignedBtc(sat: number): string {
  assertSat(sat);
  const sign = sat > 0 ? "+" : sat < 0 ? "-" : "";
  return sign + formatBtc(Math.abs(sat));
}

/** `+1,000,000 sat`, `-2,000 sat`, `0 sat` */
export function formatSignedSat(sat: number): string {
  assertSat(sat);
  const sign = sat > 0 ? "+" : sat < 0 ? "-" : "";
  return `${sign}${groupThousands(Math.abs(sat))} sat`;
}

/**
 * A BTC amount split for display: the significant digits and the trailing zeros, so the UI can
 * dim `0.0125` **`0000`** without ever dropping a decimal place.
 */
export function splitBtc(sat: number): { sign: string; significant: string; zeros: string } {
  assertSat(sat);
  const text = formatBtc(Math.abs(sat));
  const trimmed = text.replace(/0+$/, "");
  // Keep at least one decimal digit visible ("1.0", not "1.").
  const significant = trimmed.endsWith(".") ? trimmed + "0" : trimmed;
  return {
    sign: sat < 0 ? "-" : "",
    significant,
    zeros: text.slice(significant.length),
  };
}

export type ParseResult = { ok: true; sat: number } | { ok: false; error: string };

const fail = (error: string): ParseResult => ({ ok: false, error });

function checkRange(sat: bigint): ParseResult {
  if (sat === 0n) return fail("Enter an amount greater than zero.");
  if (sat > BigInt(MAX_MONEY_SAT)) return fail("That's more than 21 million BTC.");
  return { ok: true, sat: Number(sat) };
}

/**
 * `"0.0015"` → 150 000 sat. Digits and at most one dot, at most 8 decimals; no exponent, no
 * sign, no grouping. Commas are rejected on purpose: in many countries `0,001` means one
 * thousandth, and silently reading it as `0001` would send a thousand times too much.
 */
export function parseBtc(input: string): ParseResult {
  const text = input.trim();
  if (text === "") return fail("Enter an amount.");
  if (text.startsWith("-")) return fail("The amount can't be negative.");
  if (text.includes(",")) return fail("Use a dot for decimals (0.001) and no thousands separators.");
  const match = /^(\d*)(?:\.(\d*))?$/.exec(text);
  if (!match || text === ".") return fail("Enter a number, like 0.0015.");
  const whole = match[1] ?? "";
  const frac = match[2] ?? "";
  if (frac.length > 8) {
    return fail("BTC has at most 8 decimal places (1 sat = 0.00000001 BTC).");
  }
  const sat = BigInt(whole || "0") * BigInt(SAT_PER_BTC) + BigInt(frac.padEnd(8, "0"));
  return checkRange(sat);
}

/**
 * `"150,000"` → 150 000 sat. Whole numbers only; `,`, `_` and spaces are accepted as
 * thousands separators when they split the number into proper groups of three.
 */
export function parseSat(input: string): ParseResult {
  const text = input.trim();
  if (text === "") return fail("Enter an amount.");
  if (text.startsWith("-")) return fail("The amount can't be negative.");
  if (text.includes(".")) {
    return fail("Satoshis are whole numbers. Switch to BTC to enter decimals.");
  }
  const plain = /^\d+$/.test(text);
  const grouped = /^\d{1,3}([,_   ]\d{3})+$/.test(text);
  if (!plain && !grouped) return fail("Enter a whole number of satoshis, like 150000.");
  return checkRange(BigInt(text.replace(/\D/g, "")));
}

export function parseAmount(input: string, unit: AmountUnit): ParseResult {
  return unit === "btc" ? parseBtc(input) : parseSat(input);
}

/**
 * The text to put back into an input after switching units: `"0.0015"` (no trailing zeros)
 * or `"150000"` (no separators), so it parses back to exactly the same amount.
 */
export function satToInput(sat: number, unit: AmountUnit): string {
  assertSat(sat);
  if (unit === "sat") return sat.toString();
  return formatBtc(sat).replace(/\.?0+$/, "");
}

/** Above this many sat/vB the UI asks for a correction: almost certainly a typo. */
export const MAX_FEE_RATE = 1000;

/** A typed fee rate in sat/vB (up to 3 decimals); empty means "automatic" (`null`). */
export function parseFeeRate(text: string): { ok: true; value: number | null } | { ok: false; error: string } {
  const t = text.trim();
  if (t === "") return { ok: true, value: null };
  if (!/^\d+(\.\d{1,3})?$/.test(t)) return { ok: false, error: "Enter a number of sat/vB, like 2 or 2.5." };
  const value = Number(t);
  if (value <= 0) return { ok: false, error: "The fee rate must be above zero." };
  if (value > MAX_FEE_RATE) {
    return { ok: false, error: `Above ${MAX_FEE_RATE} sat/vB is almost certainly a mistake.` };
  }
  return { ok: true, value };
}
