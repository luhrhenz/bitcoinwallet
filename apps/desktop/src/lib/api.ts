// The desktop command contract. The UI talks ONLY to `api`, never to `invoke` directly.
// `npm run dev:mock` swaps in the in-memory mock (src/lib/mock.ts, Agent E) so the whole UI
// can be built and tested without Rust; the Tauri build uses the real commands (Agent G).
//
// Security rule: recovery words cross into JS only as the return value of `create_wallet` (for
// the backup screen) and of `reveal_phrase` (the user asked to see them, with the password).
// Nothing else returns secrets: the keys and the PSBT of a prepared payment stay in Rust.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  AddressRow,
  AppInfo,
  BalanceView,
  PreparedSend,
  Settings,
  SyncProgress,
  SyncReport,
  TxRow,
  TxStatus,
  UtxoRow,
} from "./types";

export interface WalletApi {
  appInfo(): Promise<AppInfo>;
  createWallet(words: 12 | 24, password: string): Promise<{ mnemonic: string[] }>;
  restoreWallet(phrase: string, password: string, birthday: number | null): Promise<void>;
  unlock(password: string): Promise<void>;
  lock(): Promise<void>;
  /**
   * The user is active (key press, click) while unlocked. Rust runs its own auto-lock as a
   * backstop for the UI's timer and counts only user actions, not background polls.
   */
  keepAlive(): Promise<void>;

  newAddress(): Promise<AddressRow>;
  listAddresses(): Promise<AddressRow[]>;
  sync(): Promise<SyncReport>;
  onSyncProgress(cb: (p: SyncProgress) => void): Promise<UnlistenFn>;
  balance(): Promise<BalanceView>;
  history(): Promise<TxRow[]>;
  utxos(): Promise<UtxoRow[]>;

  prepareSend(to: string, amountSat: number, feeRateSatVb: number | null): Promise<PreparedSend>;
  confirmSend(id: string): Promise<{ txid: string }>;
  cancelSend(id: string): Promise<void>;
  txStatus(txid: string): Promise<TxStatus | null>;

  getSettings(): Promise<Settings>;
  setSettings(s: Settings): Promise<void>;

  /**
   * Word positions (1-based, ascending; three of them) to ask for in a backup check. Needs the
   * password: only the encrypted keystore knows how many words the phrase has, and a wrong
   * password is caught before the user types any words.
   */
  backupChallenge(password: string): Promise<number[]>;
  /**
   * Compare `[position, word]` pairs with the phrase; marks the backup verified on success.
   * Rejects with `backup_mismatch` naming the wrong positions (never words).
   */
  verifyBackup(password: string, answers: [number, string][]): Promise<void>;
  /** The recovery phrase again, with the password, at any time. */
  revealPhrase(password: string): Promise<{ mnemonic: string[] }>;
}

// Command names are snake_case to match #[tauri::command] fns; args are camelCase (Tauri default).
export const tauriApi: WalletApi = {
  appInfo: () => invoke("app_info"),
  createWallet: (words, password) => invoke("create_wallet", { words, password }),
  restoreWallet: (phrase, password, birthday) =>
    invoke("restore_wallet", { phrase, password, birthday }),
  unlock: (password) => invoke("unlock", { password }),
  lock: () => invoke("lock"),
  keepAlive: () => invoke("keep_alive"),

  newAddress: () => invoke("new_address"),
  listAddresses: () => invoke("list_addresses"),
  sync: () => invoke("sync"),
  onSyncProgress: (cb) => listen<SyncProgress>("sync-progress", (e) => cb(e.payload)),
  balance: () => invoke("balance"),
  history: () => invoke("history"),
  utxos: () => invoke("utxos"),

  prepareSend: (to, amountSat, feeRateSatVb) =>
    invoke("prepare_send", { to, amountSat, feeRateSatVb }),
  confirmSend: (id) => invoke("confirm_send", { id }),
  cancelSend: (id) => invoke("cancel_send", { id }),
  txStatus: (txid) => invoke("tx_status", { txid }),

  getSettings: () => invoke("get_settings"),
  setSettings: (settings) => invoke("set_settings", { settings }),

  backupChallenge: (password) => invoke("backup_challenge", { password }),
  verifyBackup: (password, answers) => invoke("verify_backup", { password, answers }),
  revealPhrase: (password) => invoke("reveal_phrase", { password }),
};

let selected: WalletApi = tauriApi;

/** Tests and the mock dev mode inject an implementation here. */
export function setApi(impl: WalletApi): void {
  selected = impl;
}

export const api: WalletApi = new Proxy({} as WalletApi, {
  get: (_t, prop: keyof WalletApi) => selected[prop],
});
