// Small display helpers that aren't amounts (those live in amount.ts).

import type { TxRow, TxStatus } from "./types";

const pad = (n: number) => n.toString().padStart(2, "0");

/**
 * Unix seconds → `"2026-10-03 14:05"`, the CLI's date format. The CLI prints UTC (its column
 * says so); a desktop app shows the computer's local time, and `utc: true` gives the CLI form.
 */
export function formatDateTime(unixSecs: number, opts: { utc?: boolean } = {}): string {
  const d = new Date(unixSecs * 1000);
  if (opts.utc) {
    return `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())} ${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}`;
  }
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** "just now", "4 min ago", "3 h ago", "2 days ago" */
export function timeAgo(unixSecs: number, nowMs: number = Date.now()): string {
  const secs = Math.max(0, Math.floor(nowMs / 1000 - unixSecs));
  if (secs < 45) return "just now";
  const mins = Math.round(secs / 60);
  if (mins < 60) return `${mins} min ago`;
  const hours = Math.round(mins / 60);
  if (hours < 24) return `${hours} h ago`;
  const days = Math.round(hours / 24);
  return days === 1 ? "yesterday" : `${days} days ago`;
}

/** `1 confirmation`, `2 confirmations` */
export function plural(n: number, one: string, many: string): string {
  return n === 1 ? `1 ${one}` : `${n.toLocaleString("en-US")} ${many}`;
}

export function statusLabel(status: TxStatus): string {
  return status.state === "confirmed"
    ? plural(status.confirmations, "confirmation", "confirmations")
    : "Unconfirmed";
}

/** When a transaction happened: its block's time, or when the node first saw it. */
export function txTime(tx: TxRow): number | null {
  return tx.status.state === "confirmed" ? tx.status.block_time : tx.status.first_seen;
}

/**
 * Newest first: unconfirmed (most recently seen first), then confirmed by height. The core
 * already sorts this way (PLAN §5.7); sorting again keeps the UI right whatever arrives.
 */
export function sortNewestFirst(rows: readonly TxRow[]): TxRow[] {
  const rank = (tx: TxRow): [number, number] =>
    tx.status.state === "unconfirmed"
      ? [1, tx.status.first_seen ?? Number.MAX_SAFE_INTEGER]
      : [0, tx.status.height];
  return [...rows].sort((a, b) => {
    const [ga, va] = rank(a);
    const [gb, vb] = rank(b);
    return gb - ga || vb - va;
  });
}

/** From the wallet's point of view (CLI `direction`). */
export function direction(netSat: number): "received" | "sent" | "self" {
  return netSat > 0 ? "received" : netSat < 0 ? "sent" : "self";
}

/** `tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx` → `["tb1q", "w508", …]` for checking by eye. */
export function chunk(text: string, size = 4): string[] {
  const out: string[] = [];
  for (let i = 0; i < text.length; i += size) out.push(text.slice(i, i + size));
  return out;
}

/** `c39f7885…7c84e1a91` */
export function shortId(id: string, head = 8, tail = 8): string {
  return id.length <= head + tail + 1 ? id : `${id.slice(0, head)}…${id.slice(-tail)}`;
}
