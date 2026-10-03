// The one place that turns a failed command into words for people.
//
// Every command rejects with `ApiError { code, message }`, where `code` is
// `WalletError::code()` from btcw-core (crates/btcw-core/src/error.rs). Screens never show
// `message` on its own and never show a stack trace: they call `describeError`, which gives a
// plain-language title per code plus, where it helps, the core's own message as detail. The
// core's messages are written for people and never contain secrets (error.rs rule), so they
// are safe to show.

import type { ApiError, NetworkName } from "./types";
import { DUST_LIMIT_SAT, groupThousands } from "./amount";
import { NETWORKS } from "./network";

export interface FriendlyError {
  code: string;
  /** One sentence: what happened and, when possible, what to do. */
  title: string;
  /** Extra specifics from the backend (amounts, positions, node's reason), or null. */
  detail: string | null;
}

export interface ErrorContext {
  network?: NetworkName;
}

export function isApiError(value: unknown): value is ApiError {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as ApiError).code === "string" &&
    typeof (value as ApiError).message === "string"
  );
}

/** First line only, trimmed and capped: never a stack trace, never a wall of text. */
function clean(text: string): string {
  const first = text.split("\n", 1)[0]?.trim() ?? "";
  return first.length > 240 ? `${first.slice(0, 239)}…` : first;
}

/** Anything a promise can reject with, as an `ApiError`. */
export function toApiError(value: unknown): ApiError {
  if (isApiError(value)) return { code: value.code, message: clean(value.message) };
  if (typeof value === "string") return { code: "unknown", message: clean(value) };
  if (value instanceof Error) return { code: "unknown", message: clean(value.message) };
  return { code: "unknown", message: "" };
}

type Describe = (message: string, ctx: ErrorContext) => { title: string; detail?: string | null };

const DESCRIPTIONS: Record<string, Describe> = {
  wallet_not_found: () => ({
    title: "There is no wallet on this network yet. Create one or restore one from its recovery phrase.",
  }),
  wallet_exists: () => ({
    title: "A wallet already exists on this network. Switch networks in Settings to create another.",
  }),
  wallet_in_use: () => ({
    title:
      "This wallet is open in another btcw window or terminal. Close it there, then try again.",
  }),
  wrong_password: () => ({ title: "That password is not correct. Try again." }),
  weak_password: () => ({ title: "Choose a password with at least 8 characters." }),
  invalid_mnemonic: (message) => ({
    title: "That recovery phrase isn't valid. Check each word and the order.",
    detail: message.replace(/^invalid mnemonic:\s*/, ""),
  }),
  invalid_address: () => ({
    title: "That isn't a valid Bitcoin address. Check it against the one you were given.",
  }),
  network_mismatch: (message, ctx) => {
    if (!ctx.network) return { title: "That address belongs to a different Bitcoin network.", detail: message };
    const meta = NETWORKS[ctx.network];
    return {
      title: `That address is for a different network. On ${meta.label}, addresses start with ${meta.addressPrefix}.`,
    };
  },
  mainnet_disabled: () => ({
    title: "Mainnet is switched off in this build. Use testnet4, signet or regtest.",
  }),
  dust_amount: () => ({
    title: `That amount is too small to send. The minimum is ${groupThousands(DUST_LIMIT_SAT)} sat.`,
  }),
  insufficient_funds: (message) => ({
    title: "Not enough funds to send this amount plus the network fee.",
    detail: message.replace(/^insufficient funds:\s*/, ""),
  }),
  tx_build: (message) => ({
    title: "The transaction couldn't be built.",
    detail: message.replace(/^could not build transaction:\s*/, ""),
  }),
  sign: (message) => ({ title: "The transaction couldn't be signed.", detail: message }),
  tx_not_found: () => ({ title: "This transaction isn't in your wallet." }),
  rpc: (message) => ({
    title:
      "Can't reach your Bitcoin node. Check that bitcoind is running and that the node settings are right.",
    detail: message.replace(/^bitcoin node RPC error:\s*/, ""),
  }),
  persist: (message) => ({ title: "The wallet file couldn't be saved.", detail: message }),
  keystore: (message) => ({
    title: "The encrypted recovery phrase file couldn't be read.",
    detail: message,
  }),
  config: (message) => ({
    title: "That setting isn't valid.",
    detail: message.replace(/^configuration error:\s*/, ""),
  }),
  io: (message) => ({ title: "A wallet file couldn't be read or written.", detail: message }),
  locked: () => ({ title: "The wallet is locked. Enter your password to continue." }),
};

export function describeError(error: unknown, ctx: ErrorContext = {}): FriendlyError {
  const { code, message } = toApiError(error);
  const describe = DESCRIPTIONS[code];
  if (describe) {
    const { title, detail } = describe(message, ctx);
    return { code, title, detail: detail ? detail : null };
  }
  return {
    code,
    title: "Something went wrong.",
    detail: message || null,
  };
}

/** Title and detail in one string, for compact places like field errors. */
export function errorText(error: unknown, ctx: ErrorContext = {}): string {
  const { title, detail } = describeError(error, ctx);
  return detail ? `${title} (${detail})` : title;
}
