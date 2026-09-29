// The desktop command contract. The UI talks ONLY to `api`, never to `invoke` directly.
// `npm run dev:mock` swaps in the in-memory mock (src/lib/mock.ts, Agent E) so the whole UI
// can be built and tested without Rust; the Tauri build uses the real commands (Agent G).
//
// Security rule: the mnemonic crosses into JS exactly once (create_wallet's return value, for
// the backup screen). Nothing else returns secrets.

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
}

// Command names are snake_case to match #[tauri::command] fns; args are camelCase (Tauri default).
export const tauriApi: WalletApi = {
  appInfo: () => invoke("app_info"),
  createWallet: (words, password) => invoke("create_wallet", { words, password }),
  restoreWallet: (phrase, password, birthday) =>
    invoke("restore_wallet", { phrase, password, birthday }),
  unlock: (password) => invoke("unlock", { password }),
  lock: () => invoke("lock"),

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
};

let selected: WalletApi = tauriApi;

/** Tests and the mock dev mode inject an implementation here. */
export function setApi(impl: WalletApi): void {
  selected = impl;
}

export const api: WalletApi = new Proxy({} as WalletApi, {
  get: (_t, prop: keyof WalletApi) => selected[prop],
});
