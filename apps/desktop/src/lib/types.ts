// Mirrors crates/btcw-core/src/types.rs (serde output). Keep the two in sync.
// Amounts are integer satoshis.

export type Keychain = "external" | "internal";

export interface AddressRow {
  index: number;
  address: string;
  keychain: Keychain;
  used: boolean;
}

export interface BalanceView {
  confirmed_sat: number;
  unconfirmed_sat: number;
  immature_sat: number;
  total_sat: number;
}

export type TxStatus =
  | { state: "unconfirmed"; first_seen: number | null }
  | { state: "confirmed"; height: number; confirmations: number; block_time: number };

export interface TxRow {
  txid: string;
  received_sat: number;
  sent_sat: number;
  net_sat: number;
  fee_sat: number | null;
  status: TxStatus;
  /** The user's own label for this transaction (`btcw label`), if any. */
  label: string | null;
}

export interface UtxoRow {
  outpoint: string;
  value_sat: number;
  address: string | null;
  keychain: Keychain;
  derivation_index: number;
  confirmed: boolean;
}

export interface SyncProgress {
  height: number;
  tip_height: number;
}

export interface SyncReport {
  tip_height: number;
  blocks_scanned: number;
  mempool_txs: number;
}

export interface SendPreview {
  to: string;
  amount_sat: number;
  fee_sat: number;
  fee_rate_sat_vb: number;
  vsize: number;
  change_sat: number | null;
  total_sat: number;
  /** Contact name the user typed; always shown next to `to`. */
  contact: string | null;
  /** Fee bump: the txid of the unconfirmed payment this transaction replaces. */
  replaces: string | null;
}

/** One address-book entry. Names are unique ignoring case; the address is valid for the network. */
export interface Contact {
  name: string;
  /** Canonical form (lower-case bech32), checked for the wallet's network when it was saved. */
  address: string;
  note: string | null;
}

// ---- Desktop-only shapes (defined by the Tauri command layer, apps/desktop/src-tauri) ----

export type NetworkName = "testnet4" | "signet" | "regtest" | "bitcoin";

export interface AppInfo {
  network: NetworkName;
  /** Networks the user may pick (mainnet only in `--features mainnet` builds). */
  networks: NetworkName[];
  wallet_exists: boolean;
  /** True when a signer is loaded (password entered). Watch-only views work without it. */
  unlocked: boolean;
  /** Height the wallet is synced to (0 before the first sync), or null without a wallet. */
  synced_height: number | null;
  /** Whether the recovery phrase backup has been verified, or null without a wallet. */
  backup_verified: boolean | null;
}

export interface Settings {
  network: NetworkName;
  rpc_url: string | null;
  rpc_cookie: string | null;
  /** 1 to 60. The Rust side also locks on its own, a minute after the UI would. */
  auto_lock_minutes: number;
  /** The user typed MAINNET in Settings (the runtime half of the mainnet gate). */
  mainnet_opt_in: boolean;
}

export interface PreparedSend {
  /** Opaque id; pass to confirm_send / cancel_send. The PSBT stays on the Rust side. */
  id: string;
  preview: SendPreview;
}

/** Every failed command rejects with this (codes = WalletError::code() in btcw-core). */
export interface ApiError {
  code: string;
  message: string;
}

// ---- Assistant (src-tauri/src/assistant) ----

export type AssistantProvider = "groq" | "gemini" | "custom";

/** What the UI may know about the assistant's setup. The API key itself never comes back. */
export interface AssistantSettings {
  enabled: boolean;
  /** The user agreed, once, that wallet data goes to the provider. */
  consented: boolean;
  provider: AssistantProvider;
  base_url: string;
  model: string;
  has_api_key: boolean;
  /** `get_btc_price` (CoinGecko) is offered to the model. */
  live_price: boolean;
}

export interface AssistantSettingsUpdate {
  enabled: boolean;
  provider: AssistantProvider;
  base_url: string;
  model: string;
  /** A new key, or null to keep the saved one. */
  api_key: string | null;
  clear_api_key: boolean;
  live_price: boolean;
  /** The user accepted the data notice just now. */
  consent: boolean;
}

/** What to prepare once the wallet is unlocked. */
export type PrepareRequest =
  | { action: "payment"; to: string; amount_sat: number; fee_rate_sat_vb: number | null }
  | { action: "fee_bump"; txid: string; fee_rate_sat_vb: number | null };

export type AssistantCard =
  | { kind: "payment"; id: string; preview: SendPreview }
  | { kind: "fee_bump"; id: string; preview: SendPreview }
  | { kind: "needs_unlock"; request: PrepareRequest };

export interface ChatItem {
  role: "user" | "assistant";
  text: string;
  cards: AssistantCard[];
  /** Tools the assistant used for this answer. */
  tools: string[];
}
