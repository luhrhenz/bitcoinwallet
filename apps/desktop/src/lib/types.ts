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
  /**
   * Height the wallet is synced to (`WalletService::synced_height`, 0 before the first sync),
   * or null when there is no wallet. Lets the dashboard say "as of block N" before, or
   * without, a successful sync (the CLI's `as_of`).
   */
  synced_height: number | null;
}

export interface Settings {
  network: NetworkName;
  rpc_url: string | null;
  rpc_cookie: string | null;
  auto_lock_minutes: number;
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
