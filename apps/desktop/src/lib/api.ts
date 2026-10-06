// The desktop commands. The UI talks only to `api`, never to `invoke` directly; `npm run
// dev:mock` swaps in the in-memory mock (lib/mock.ts).
//
// Recovery words reach JS only from `create_wallet` and `reveal_phrase`. Keys and prepared
// PSBTs stay in Rust.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  AddressRow,
  AppInfo,
  AssistantSettings,
  AssistantSettingsUpdate,
  BalanceView,
  ChatItem,
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
  /** The user is active while unlocked; Rust's own auto-lock backs up the UI's timer. */
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

  /** The lowest fee rate (sat/vB, rounded up) that can replace `txid`, or why it can't. */
  minFeeBumpRate(txid: string): Promise<number>;
  /**
   * Build the replacement of `txid` and keep it like a prepared payment. `null` = the node's
   * estimate, raised to the minimum.
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
   * Three word positions (1-based, ascending) to ask for in a backup check. Needs the password:
   * only the keystore knows the phrase length.
   */
  backupChallenge(password: string): Promise<number[]>;
  /** Check `[position, word]` pairs and mark the backup verified; errors name positions only. */
  verifyBackup(password: string, answers: [number, string][]): Promise<void>;
  /** The recovery phrase again, with the password, at any time. */
  revealPhrase(password: string): Promise<{ mnemonic: string[] }>;

  // Assistant: the model runs behind Rust; it can prepare payments as cards, never send them.
  getAssistantSettings(): Promise<AssistantSettings>;
  /** The key is write-only: the reply says only whether one is saved. */
  setAssistantSettings(update: AssistantSettingsUpdate): Promise<AssistantSettings>;
  /** One message → the assistant's answer. Cards confirm through `confirmSend` like a payment. */
  assistantSend(text: string): Promise<ChatItem>;
  /** The chat so far; empty after a lock or a network switch. */
  assistantHistory(): Promise<ChatItem[]>;
  assistantClear(): Promise<void>;
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

  getAssistantSettings: () => invoke("get_assistant_settings"),
  setAssistantSettings: (settings) => invoke("set_assistant_settings", { settings }),
  assistantSend: (text) => invoke("assistant_send", { text }),
  assistantHistory: () => invoke("assistant_history"),
  assistantClear: () => invoke("assistant_clear"),
};

let selected: WalletApi = tauriApi;

/** Tests and the mock dev mode inject an implementation here. */
export function setApi(impl: WalletApi): void {
  selected = impl;
}

export const api: WalletApi = new Proxy({} as WalletApi, {
  get: (_t, prop: keyof WalletApi) => selected[prop],
});
