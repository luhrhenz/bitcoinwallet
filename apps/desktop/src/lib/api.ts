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
  Contact,
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

  /** `to` is an address or a contact's name; the preview's `to` is always the full address. */
  prepareSend(to: string, amountSat: number, feeRateSatVb: number | null): Promise<PreparedSend>;
  /** Sends a prepared payment or fee bump (the same single slot). */
  confirmSend(id: string): Promise<{ txid: string }>;
  cancelSend(id: string): Promise<void>;
  txStatus(txid: string): Promise<TxStatus | null>;

  /**
   * "Speed up": the lowest fee rate (sat/vB, rounded up to the hundredth) that can replace the
   * wallet's unconfirmed payment `txid`. Rejects with the reason when it can't be replaced.
   * Watch-only.
   */
  minFeeBumpRate(txid: string): Promise<number>;
  /**
   * Build the replacement of `txid` and keep it like a prepared payment (same slot, expiry,
   * `confirmSend` / `cancelSend`); `preview.replaces` is `txid`. Needs the wallet unlocked.
   * `null` = the node's estimate, raised to the minimum.
   */
  prepareFeeBump(txid: string, feeRateSatVb: number | null): Promise<PreparedSend>;

  // Address book and labels: watch-only (no password), and count as user activity.
  listContacts(): Promise<Contact[]>;
  addContact(name: string, address: string, note: string | null): Promise<Contact>;
  /** Returns the contact that was removed. */
  removeContact(name: string): Promise<Contact>;
  renameContact(oldName: string, newName: string): Promise<Contact>;
  /** Returns the label as stored (trimmed). `tx_not_found` for a transaction not in the wallet. */
  setLabel(txid: string, label: string): Promise<string>;
  clearLabel(txid: string): Promise<void>;

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

  minFeeBumpRate: (txid) => invoke("min_fee_bump_rate", { txid }),
  prepareFeeBump: (txid, feeRateSatVb) => invoke("prepare_fee_bump", { txid, feeRateSatVb }),

  listContacts: () => invoke("list_contacts"),
  addContact: (name, address, note) => invoke("add_contact", { name, address, note }),
  removeContact: (name) => invoke("remove_contact", { name }),
  renameContact: (oldName, newName) => invoke("rename_contact", { old: oldName, new: newName }),
  setLabel: (txid, label) => invoke("set_label", { txid, label }),
  clearLabel: (txid) => invoke("clear_label", { txid }),

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
