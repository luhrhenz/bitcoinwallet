// In-memory stand-in for the Rust command layer (src-tauri, Agent G), so the whole UI can be
// built, demoed (`npm run dev:mock`) and tested without Rust or a node.
//
// It follows btcw-core's rules closely enough that the UI meets every real error path:
// - phrases are real BIP39 (word list + checksum); passwords need 8+ characters;
// - addresses are real bech32 for the active network (`tb1…` / `bcrt1…`), and sending checks
//   prefix, checksum, dust (294 sat), funds, and builds a fee preview with P2WPKH sizes
//   (~141 vB for 1 input and 2 outputs);
// - the wallet only learns about the chain by syncing: a payment shows up after `sync`, and a
//   block mined with `mineBlocks` turns into confirmations after `sync` or `txStatus` (which
//   syncs first, as PLAN §5.9 and `tx::tx_status`'s doc ask of the real command);
// - every failure rejects with a plain `ApiError { code, message }` object, like Tauri.
//
// Simplifications: one fake "node" per network, coins only to and from this wallet, no
// reorgs or evictions, legacy (base58) addresses accepted by shape without a checksum.

import type { UnlistenFn } from "@tauri-apps/api/event";
import type { WalletApi } from "./api";
import type {
  AddressRow,
  ApiError,
  AppInfo,
  BalanceView,
  Keychain,
  NetworkName,
  PreparedSend,
  Settings,
  SyncProgress,
  SyncReport,
  TxRow,
  TxStatus,
  UtxoRow,
} from "./types";
import { DUST_LIMIT_SAT, formatBtc } from "./amount";
import { BIP39_ENGLISH } from "./mock-wordlist";

export interface MockOptions {
  /** Delay before every command answers. Default 150 ms; use 0 in tests. */
  latencyMs?: number;
  /** Delay between `sync-progress` events. Default 60 ms; use 0 in tests. */
  syncStepMs?: number;
  /** Active network at start. Default testnet4. */
  network?: NetworkName;
  /** Networks offered in Settings. Default testnet4, signet, regtest (no mainnet). */
  networks?: NetworkName[];
  /** Mine a block on the active network every N ms (demo mode). Default off. */
  autoMineMs?: number | null;
  /** Fee rate used when the user leaves it empty (the node's "estimate"). Default 2 sat/vB. */
  defaultFeeRate?: number;
}

/** Test and demo controls on top of the `WalletApi`. The UI itself never calls these. */
export interface MockControls {
  /** Someone pays `sat` to the wallet's current receive address (lands in the mempool). */
  simulateIncoming(sat: number): string;
  /** Mine `n` blocks: the first one confirms everything in the mempool. Returns the new tip. */
  mineBlocks(n?: number, opts?: { toWallet?: boolean }): number;
  /** Take the node down (every node-backed command fails with `rpc`) or bring it back. */
  setNodeOnline(online: boolean): void;
  isNodeOnline(): boolean;
  tipHeight(): number;
  /** Stop the auto-mining timer. */
  dispose(): void;
}

export type MockWalletApi = WalletApi & MockControls;

const DEFAULT_NETWORKS: NetworkName[] = ["testnet4", "signet", "regtest"];
/** Roughly where each chain was in late 2026; only used to make heights look plausible. */
const START_TIP: Record<NetworkName, number> = {
  testnet4: 118_420,
  signet: 289_311,
  regtest: 250,
  bitcoin: 966_000,
};
const HRP: Record<NetworkName, string> = { testnet4: "tb", signet: "tb", regtest: "bcrt", bitcoin: "bc" };
const RPC_PORT: Record<NetworkName, number> = { testnet4: 48332, signet: 38332, regtest: 18443, bitcoin: 8332 };
const BLOCK_SECS = 600;
const COINBASE_MATURITY = 100;

// ---------------------------------------------------------------------------------------------
// State

interface Output {
  outpoint: string;
  value: number;
  keychain: Keychain;
  index: number;
  address: string;
}

interface MockTx {
  txid: string;
  /** Outpoints of this wallet's outputs that the tx spends. */
  spends: string[];
  /** Total value of those inputs. */
  sentSat: number;
  /** Outputs paying this wallet. */
  outputs: Output[];
  feeSat: number | null;
  firstSeen: number;
  /** Block the node has it in; null while in the mempool. */
  height: number | null;
  coinbase: boolean;
}

interface Wallet {
  password: string;
  seed: Uint8Array;
  birthday: number;
  syncedHeight: number;
  /** Transactions the wallet has learned about (by syncing or by sending them). */
  seen: Set<string>;
  revealed: Record<Keychain, number>;
  unlocked: boolean;
}

interface Chain {
  network: NetworkName;
  tip: number;
  anchorTip: number;
  anchorTime: number;
  minedTimes: Map<number, number>;
  txs: MockTx[];
  wallet: Wallet | null;
}

interface Pending {
  network: NetworkName;
  preview: PreparedSend["preview"];
  inputs: Output[];
  change: Output | null;
}

const nowSecs = () => Math.floor(Date.now() / 1000);
/** What crossing the IPC boundary does: the UI gets copies, never the mock's own objects. */
const clone = <T>(value: T): T => (value === undefined ? value : (JSON.parse(JSON.stringify(value)) as T));
const fail = (code: string, message: string): ApiError => ({ code, message });

function delay(ms: number): Promise<void> {
  return ms > 0 ? new Promise((resolve) => setTimeout(resolve, ms)) : Promise.resolve();
}

function randomBytes(n: number): Uint8Array {
  const bytes = new Uint8Array(n);
  crypto.getRandomValues(bytes);
  return bytes;
}

const hex = (bytes: Uint8Array) => Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
const utf8 = (text: string) => new TextEncoder().encode(text);

function concat(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let offset = 0;
  for (const p of parts) {
    out.set(p, offset);
    offset += p.length;
  }
  return out;
}

const u32le = (n: number) => new Uint8Array([n & 0xff, (n >>> 8) & 0xff, (n >>> 16) & 0xff, (n >>> 24) & 0xff]);

// ---------------------------------------------------------------------------------------------
// The mock

export function createMockApi(options: MockOptions = {}): MockWalletApi {
  const latency = options.latencyMs ?? 150;
  const syncStep = options.syncStepMs ?? 60;
  const networks = options.networks ?? DEFAULT_NETWORKS;
  const defaultFeeRate = options.defaultFeeRate ?? 2;

  const chains = new Map<NetworkName, Chain>();
  const chainFor = (network: NetworkName): Chain => {
    let chain = chains.get(network);
    if (!chain) {
      const tip = START_TIP[network];
      chain = { network, tip, anchorTip: tip, anchorTime: nowSecs() - 240, minedTimes: new Map(), txs: [], wallet: null };
      chains.set(network, chain);
    }
    return chain;
  };

  let settings: Settings = {
    network: options.network ?? "testnet4",
    rpc_url: null,
    rpc_cookie: null,
    auto_lock_minutes: 5,
  };
  let nodeOnline = true;
  const listeners = new Set<(p: SyncProgress) => void>();
  const pending = new Map<string, Pending>();
  let syncing: Promise<SyncReport> | null = null;

  const active = () => chainFor(settings.network);
  const rpcUrl = () => settings.rpc_url ?? `http://127.0.0.1:${RPC_PORT[settings.network]}`;

  function requireWallet(): { chain: Chain; wallet: Wallet } {
    const chain = active();
    if (!chain.wallet) {
      throw fail("wallet_not_found", `no wallet found in ~/.local/share/btcw/${chain.network}; create or restore one first`);
    }
    return { chain, wallet: chain.wallet };
  }

  function requireNode(): void {
    if (!nodeOnline) {
      throw fail("rpc", `bitcoin node RPC error: could not connect to ${rpcUrl()}: Connection refused (os error 111)`);
    }
  }

  function requireUnlocked(wallet: Wallet): void {
    if (!wallet.unlocked) throw fail("locked", "the wallet is locked; unlock it with your password to send");
  }

  function checkNewPassword(password: string): void {
    if ([...password].length < 8) throw fail("weak_password", "password must be at least 8 characters");
  }

  function ensureAbsent(chain: Chain): void {
    if (chain.wallet) throw fail("wallet_exists", `a wallet already exists in ~/.local/share/btcw/${chain.network}`);
  }

  // --- chain and wallet views ----------------------------------------------------------------

  const blockTime = (chain: Chain, height: number) =>
    chain.minedTimes.get(height) ?? chain.anchorTime - (chain.anchorTip - height) * BLOCK_SECS;

  const addressOf = (chain: Chain, wallet: Wallet, keychain: Keychain, index: number) =>
    encodeSegwit(HRP[chain.network], 0, sha256(concat(wallet.seed, new Uint8Array([keychain === "external" ? 0 : 1]), u32le(index))).slice(0, 20));

  const outputFor = (chain: Chain, wallet: Wallet, txid: string, vout: number, value: number, keychain: Keychain, index: number): Output => ({
    outpoint: `${txid}:${vout}`,
    value,
    keychain,
    index,
    address: addressOf(chain, wallet, keychain, index),
  });

  const visibleTxs = (chain: Chain, wallet: Wallet) => chain.txs.filter((tx) => wallet.seen.has(tx.txid));

  /** Confirmed only once the wallet has synced past the block (BDK's view, not the node's). */
  function statusOf(chain: Chain, wallet: Wallet, tx: MockTx): TxStatus {
    if (tx.height !== null && tx.height <= wallet.syncedHeight) {
      return {
        state: "confirmed",
        height: tx.height,
        confirmations: wallet.syncedHeight - tx.height + 1,
        block_time: blockTime(chain, tx.height),
      };
    }
    return { state: "unconfirmed", first_seen: tx.firstSeen };
  }

  function unspent(chain: Chain, wallet: Wallet): { out: Output; tx: MockTx; status: TxStatus }[] {
    const txs = visibleTxs(chain, wallet);
    const spent = new Set(txs.flatMap((tx) => tx.spends));
    return txs.flatMap((tx) => {
      const status = statusOf(chain, wallet, tx);
      return tx.outputs.filter((out) => !spent.has(out.outpoint)).map((out) => ({ out, tx, status }));
    });
  }

  const isImmature = (tx: MockTx, status: TxStatus) =>
    tx.coinbase && (status.state === "unconfirmed" || status.confirmations < COINBASE_MATURITY);

  function usedIndexes(chain: Chain, wallet: Wallet, keychain: Keychain): Set<number> {
    return new Set(
      visibleTxs(chain, wallet).flatMap((tx) => tx.outputs.filter((o) => o.keychain === keychain).map((o) => o.index)),
    );
  }

  /** BDK's `next_unused_address`: the lowest revealed address nobody has paid yet. */
  function nextUnusedIndex(chain: Chain, wallet: Wallet): number {
    const used = usedIndexes(chain, wallet, "external");
    for (let i = 0; i < wallet.revealed.external; i++) if (!used.has(i)) return i;
    return wallet.revealed.external;
  }

  /** Pull everything the node knows into the wallet (what `Node::sync` does). */
  function applySync(chain: Chain, wallet: Wallet): SyncReport {
    const start = wallet.syncedHeight > 0 ? wallet.syncedHeight : wallet.birthday;
    const scanned = Math.max(0, chain.tip - start);
    wallet.syncedHeight = chain.tip;
    for (const tx of chain.txs) {
      if (tx.height === null || tx.height >= wallet.birthday) wallet.seen.add(tx.txid);
    }
    // Lookahead: addresses that received coins count as revealed (restores find them this way).
    for (const keychain of ["external", "internal"] as const) {
      const used = usedIndexes(chain, wallet, keychain);
      if (used.size > 0) wallet.revealed[keychain] = Math.max(wallet.revealed[keychain], Math.max(...used) + 1);
    }
    const mempool = visibleTxs(chain, wallet).filter((tx) => tx.height === null).length;
    return { tip_height: chain.tip, blocks_scanned: scanned, mempool_txs: mempool };
  }

  function mine(chain: Chain, n: number, toWallet: boolean): number {
    for (let i = 0; i < n; i++) {
      chain.tip += 1;
      chain.minedTimes.set(chain.tip, nowSecs());
      const fees = chain.txs.filter((tx) => tx.height === null).reduce((sum, tx) => sum + (tx.feeSat ?? 0), 0);
      for (const tx of chain.txs) if (tx.height === null) tx.height = chain.tip;
      const wallet = chain.wallet;
      if (toWallet && wallet) {
        const txid = hex(randomBytes(32));
        const index = nextUnusedIndex(chain, wallet);
        wallet.revealed.external = Math.max(wallet.revealed.external, index + 1);
        chain.txs.push({
          txid,
          spends: [],
          sentSat: 0,
          outputs: [outputFor(chain, wallet, txid, 0, 5_000_000_000 + fees, "external", index)],
          feeSat: 0,
          firstSeen: nowSecs(),
          height: chain.tip,
          coinbase: true,
        });
      }
    }
    return chain.tip;
  }

  /** A restored wallet already has a past: two payments in and one out, found by the first sync. */
  function seedHistory(chain: Chain, wallet: Wallet): void {
    const id = (i: number) => hex(sha256(concat(wallet.seed, utf8(`history-${i}`))));
    const at = (back: number) => Math.max(1, chain.tip - back);
    const a = id(0);
    const b = id(1);
    const c = id(2);
    const firstOut = outputFor(chain, wallet, a, 1, 250_000, "external", 0);
    chain.txs.push(
      { txid: a, spends: [], sentSat: 0, outputs: [firstOut], feeSat: null, firstSeen: blockTime(chain, at(1500)), height: at(1500), coinbase: false },
      { txid: b, spends: [], sentSat: 0, outputs: [outputFor(chain, wallet, b, 0, 80_000, "external", 1)], feeSat: null, firstSeen: blockTime(chain, at(240)), height: at(240), coinbase: false },
      {
        txid: c,
        spends: [firstOut.outpoint],
        sentSat: 250_000,
        outputs: [outputFor(chain, wallet, c, 1, 250_000 - 60_000 - 282, "internal", 0)],
        feeSat: 282,
        firstSeen: blockTime(chain, at(120)),
        height: at(120),
        coinbase: false,
      },
    );
  }

  function emit(progress: SyncProgress): void {
    for (const listener of [...listeners]) listener({ ...progress });
  }

  async function runSync(): Promise<SyncReport> {
    const { chain, wallet } = requireWallet();
    requireNode();
    const start = wallet.syncedHeight > 0 ? wallet.syncedHeight : wallet.birthday;
    const target = chain.tip;
    const total = Math.max(0, target - start);
    const steps = Math.min(total, 24);
    if (steps === 0) emit({ height: target, tip_height: target });
    for (let k = 1; k <= steps; k++) {
      await delay(syncStep);
      if (!nodeOnline) throw fail("rpc", "bitcoin node RPC error: connection lost during sync");
      emit({ height: start + Math.round((total * k) / steps), tip_height: target });
    }
    return applySync(chain, wallet);
  }

  // --- sending ---------------------------------------------------------------------------------

  function checkAddress(raw: string, network: NetworkName): string {
    const text = raw.trim();
    if (text === "") throw fail("invalid_address", "invalid address: empty");
    const decoded = decodeSegwit(text);
    if (typeof decoded === "string") {
      const base58 = /^[1-9A-HJ-NP-Za-km-z]{25,34}$/.test(text);
      if (base58 && /^[13]/.test(text)) {
        if (network === "bitcoin") return text;
        throw fail("network_mismatch", `network mismatch: expected ${network}, found a mainnet address`);
      }
      if (base58 && /^[mn2]/.test(text)) {
        if (network !== "bitcoin") return text;
        throw fail("network_mismatch", "network mismatch: expected bitcoin, found a testnet4/signet/regtest address");
      }
      throw fail("invalid_address", `invalid address: \`${text}\`: ${decoded}`);
    }
    const owners: Record<string, { nets: NetworkName[]; name: string }> = {
      bc: { nets: ["bitcoin"], name: "a mainnet address" },
      tb: { nets: ["testnet4", "signet"], name: "a testnet4/signet address" },
      bcrt: { nets: ["regtest"], name: "a regtest address" },
    };
    const owner = owners[decoded.hrp];
    if (!owner) throw fail("invalid_address", `invalid address: \`${text}\`: unknown prefix ${decoded.hrp}1`);
    if (!owner.nets.includes(network)) {
      throw fail("network_mismatch", `network mismatch: expected ${network}, found ${owner.name}`);
    }
    return text.toLowerCase();
  }

  /** P2WPKH sizes: 10.5 vB overhead + 68 per input + 31 per output (141 vB for 1-in-2-out). */
  const vbytes = (inputs: number, outputs: number) => Math.ceil(10.5 + 68 * inputs + 31 * outputs);

  function prepare(to: string, amountSat: number, feeRate: number | null): PreparedSend {
    const { chain, wallet } = requireWallet();
    requireUnlocked(wallet);
    const address = checkAddress(to, chain.network);
    if (!Number.isSafeInteger(amountSat) || amountSat <= 0) {
      throw fail("tx_build", "could not build transaction: the amount must be a whole number of satoshis above zero");
    }
    if (amountSat < DUST_LIMIT_SAT) {
      throw fail("dust_amount", `amount ${formatBtc(amountSat)} BTC is below the dust limit`);
    }
    if (feeRate !== null && !(Number.isFinite(feeRate) && feeRate > 0 && feeRate <= 10_000)) {
      throw fail("tx_build", "could not build transaction: the fee rate must be between 0 and 10,000 sat/vB");
    }
    requireNode();
    applySync(chain, wallet); // spend from fresh UTXO state, like the core's send flow
    const rate = feeRate ?? defaultFeeRate;

    // Coins already reserved by another open preview are skipped, as BDK would have to.
    const reserved = new Set([...pending.values()].flatMap((p) => p.inputs.map((i) => i.outpoint)));
    const spendable = unspent(chain, wallet)
      .filter(({ out, tx, status }) => !isImmature(tx, status) && !reserved.has(out.outpoint))
      .sort((x, y) => Number(y.status.state === "confirmed") - Number(x.status.state === "confirmed") || y.out.value - x.out.value)
      .map(({ out }) => out);

    const inputs: Output[] = [];
    let inSum = 0;
    for (const out of spendable) {
      inputs.push(out);
      inSum += out.value;
      const feeWithChange = Math.ceil(vbytes(inputs.length, 2) * rate);
      if (inSum >= amountSat + feeWithChange + DUST_LIMIT_SAT) {
        const changeIndex = wallet.revealed.internal;
        wallet.revealed.internal += 1;
        const change: Output = {
          outpoint: "",
          value: inSum - amountSat - feeWithChange,
          keychain: "internal",
          index: changeIndex,
          address: addressOf(chain, wallet, "internal", changeIndex),
        };
        return remember(chain, address, amountSat, feeWithChange, vbytes(inputs.length, 2), inputs, change);
      }
      // Change would be dust: drop it and let the miner have the remainder (BDK does the same).
      const feeNoChange = Math.ceil(vbytes(inputs.length, 1) * rate);
      if (inSum >= amountSat + feeNoChange) {
        return remember(chain, address, amountSat, inSum - amountSat, vbytes(inputs.length, 1), inputs, null);
      }
    }
    const available = spendable.reduce((sum, out) => sum + out.value, 0);
    const needed = amountSat + Math.ceil(vbytes(Math.max(1, spendable.length), 2) * rate);
    throw fail("insufficient_funds", `insufficient funds: need ${formatBtc(needed)} BTC, available ${formatBtc(available)} BTC`);
  }

  function remember(chain: Chain, to: string, amount: number, fee: number, vsize: number, inputs: Output[], change: Output | null): PreparedSend {
    const id = hex(randomBytes(16));
    const preview = {
      to,
      amount_sat: amount,
      fee_sat: fee,
      fee_rate_sat_vb: Math.round((fee / vsize) * 100) / 100,
      vsize,
      change_sat: change ? change.value : null,
      total_sat: amount + fee,
    };
    pending.set(id, { network: chain.network, preview, inputs, change });
    return { id, preview };
  }

  function release(id: string): void {
    const entry = pending.get(id);
    if (!entry) return;
    pending.delete(id);
    const wallet = chainFor(entry.network).wallet;
    // `tx::cancel`: un-reveal the change address if nothing was revealed after it.
    if (entry.change && wallet && wallet.revealed.internal === entry.change.index + 1) {
      wallet.revealed.internal -= 1;
    }
  }

  function confirm(id: string): { txid: string } {
    const { chain, wallet } = requireWallet();
    requireUnlocked(wallet);
    const entry = pending.get(id);
    if (!entry || entry.network !== chain.network) {
      throw fail("tx_build", "could not build transaction: this send was already confirmed or cancelled; prepare it again");
    }
    requireNode();
    const spent = new Set(visibleTxs(chain, wallet).flatMap((tx) => tx.spends));
    if (entry.inputs.some((input) => spent.has(input.outpoint))) {
      release(id);
      throw fail("tx_build", "could not build transaction: its coins were spent in the meantime; prepare it again");
    }
    const txid = hex(randomBytes(32));
    const outputs = entry.change ? [{ ...entry.change, outpoint: `${txid}:1` }] : [];
    chain.txs.push({
      txid,
      spends: entry.inputs.map((input) => input.outpoint),
      sentSat: entry.inputs.reduce((sum, input) => sum + input.value, 0),
      outputs,
      feeSat: entry.preview.fee_sat,
      firstSeen: nowSecs(),
      height: null,
      coinbase: false,
    });
    wallet.seen.add(txid); // `tx::record_broadcast`
    pending.delete(id);
    return { txid };
  }

  // --- views -----------------------------------------------------------------------------------

  function balanceOf(chain: Chain, wallet: Wallet): BalanceView {
    const b = { confirmed_sat: 0, unconfirmed_sat: 0, immature_sat: 0, total_sat: 0 };
    for (const { out, tx, status } of unspent(chain, wallet)) {
      if (isImmature(tx, status)) b.immature_sat += out.value;
      else if (status.state === "confirmed") b.confirmed_sat += out.value;
      else b.unconfirmed_sat += out.value;
      b.total_sat += out.value;
    }
    return b;
  }

  function historyOf(chain: Chain, wallet: Wallet): TxRow[] {
    const rows = visibleTxs(chain, wallet).map((tx): TxRow => {
      const received = tx.outputs.reduce((sum, out) => sum + out.value, 0);
      return {
        txid: tx.txid,
        received_sat: received,
        sent_sat: tx.sentSat,
        net_sat: received - tx.sentSat,
        fee_sat: tx.feeSat,
        status: statusOf(chain, wallet, tx),
      };
    });
    // PLAN §5.7: unconfirmed first (latest seen first), then by height, newest first.
    const key = (r: TxRow): [number, number] =>
      r.status.state === "unconfirmed" ? [1, r.status.first_seen ?? 0] : [0, r.status.height];
    return rows.sort((x, y) => {
      const [gx, vx] = key(x);
      const [gy, vy] = key(y);
      return gy - gx || vy - vx;
    });
  }

  async function answer<T>(fn: () => T, extraMs = 0): Promise<T> {
    await delay(latency + extraMs);
    return clone(fn());
  }

  let miner: ReturnType<typeof setInterval> | null = null;
  if (options.autoMineMs) {
    miner = setInterval(() => {
      if (nodeOnline) mine(active(), 1, false);
    }, options.autoMineMs);
  }

  return {
    appInfo: () =>
      answer((): AppInfo => {
        const chain = active();
        return {
          network: chain.network,
          networks: [...networks],
          wallet_exists: chain.wallet !== null,
          unlocked: chain.wallet?.unlocked ?? false,
          synced_height: chain.wallet ? chain.wallet.syncedHeight : null,
        };
      }),

    createWallet: (words, password) =>
      answer(() => {
        const chain = active();
        if (words !== 12 && words !== 24) throw fail("config", "configuration error: a new phrase has 12 or 24 words");
        ensureAbsent(chain);
        checkNewPassword(password);
        const mnemonic = entropyToMnemonic(randomBytes(words === 12 ? 16 : 32));
        chain.wallet = {
          password,
          seed: sha256(utf8(mnemonic.join(" "))),
          birthday: nodeOnline ? chain.tip : 0,
          syncedHeight: 0,
          seen: new Set(),
          revealed: { external: 0, internal: 0 },
          unlocked: true,
        };
        return { mnemonic };
      }, latency * 4),

    restoreWallet: (phrase, password, birthday) =>
      answer(() => {
        const chain = active();
        ensureAbsent(chain);
        checkNewPassword(password);
        const words = phrase.trim().toLowerCase().split(/\s+/).filter(Boolean);
        const problem = checkMnemonic(words);
        if (problem) throw fail("invalid_mnemonic", `invalid mnemonic: ${problem}`);
        if (birthday !== null && !(Number.isSafeInteger(birthday) && birthday >= 0)) {
          throw fail("config", "configuration error: the birthday must be a block height (0 or more)");
        }
        const wallet: Wallet = {
          password,
          seed: sha256(utf8(words.join(" "))),
          birthday: birthday ?? 0,
          syncedHeight: 0,
          seen: new Set(),
          revealed: { external: 0, internal: 0 },
          unlocked: true,
        };
        chain.wallet = wallet;
        seedHistory(chain, wallet);
      }, latency * 4),

    unlock: (password) =>
      answer(() => {
        const { wallet } = requireWallet();
        if (password !== wallet.password) throw fail("wrong_password", "wrong password or corrupted keystore");
        wallet.unlocked = true;
      }, latency * 3), // Argon2id is meant to be slow

    lock: () =>
      answer(() => {
        const wallet = active().wallet;
        if (wallet) wallet.unlocked = false;
      }),

    newAddress: () =>
      answer((): AddressRow => {
        const { chain, wallet } = requireWallet();
        const index = nextUnusedIndex(chain, wallet);
        wallet.revealed.external = Math.max(wallet.revealed.external, index + 1);
        return { index, address: addressOf(chain, wallet, "external", index), keychain: "external", used: false };
      }),

    listAddresses: () =>
      answer((): AddressRow[] => {
        const { chain, wallet } = requireWallet();
        const used = usedIndexes(chain, wallet, "external");
        return Array.from({ length: wallet.revealed.external }, (_, index) => ({
          index,
          address: addressOf(chain, wallet, "external", index),
          keychain: "external" as const,
          used: used.has(index),
        }));
      }),

    sync: async () => {
      await delay(latency);
      // One sync at a time: a second caller shares the running one.
      if (!syncing) {
        syncing = runSync().finally(() => {
          syncing = null;
        });
      }
      return clone(await syncing);
    },

    onSyncProgress: (cb) => {
      listeners.add(cb);
      const unlisten: UnlistenFn = () => {
        listeners.delete(cb);
      };
      return Promise.resolve(unlisten);
    },

    balance: () =>
      answer(() => {
        const { chain, wallet } = requireWallet();
        return balanceOf(chain, wallet);
      }),

    history: () =>
      answer(() => {
        const { chain, wallet } = requireWallet();
        return historyOf(chain, wallet);
      }),

    utxos: () =>
      answer((): UtxoRow[] => {
        const { chain, wallet } = requireWallet();
        return unspent(chain, wallet).map(({ out, status }) => ({
          outpoint: out.outpoint,
          value_sat: out.value,
          address: out.address,
          keychain: out.keychain,
          derivation_index: out.index,
          confirmed: status.state === "confirmed",
        }));
      }),

    prepareSend: (to, amountSat, feeRate) => answer(() => prepare(to, amountSat, feeRate), latency),
    confirmSend: (id) => answer(() => confirm(id), latency * 2),
    cancelSend: (id) => answer(() => release(id)),

    txStatus: (txid) =>
      answer((): TxStatus | null => {
        const { chain, wallet } = requireWallet();
        requireNode();
        applySync(chain, wallet);
        const tx = visibleTxs(chain, wallet).find((t) => t.txid === txid);
        return tx ? statusOf(chain, wallet, tx) : null;
      }),

    getSettings: () => answer(() => settings),

    setSettings: (next) =>
      answer(() => {
        if (!networks.includes(next.network)) {
          if (next.network === "bitcoin") {
            throw fail("mainnet_disabled", "mainnet is disabled; build with `--features mainnet` and opt in explicitly");
          }
          throw fail("config", `configuration error: network ${next.network} is not available`);
        }
        if (!Number.isInteger(next.auto_lock_minutes) || next.auto_lock_minutes < 1 || next.auto_lock_minutes > 1440) {
          throw fail("config", "configuration error: auto-lock must be between 1 and 1440 minutes");
        }
        if (next.rpc_url !== null && !/^https?:\/\/[^\s/]+(\/\S*)?$/.test(next.rpc_url)) {
          throw fail("config", `configuration error: \`${next.rpc_url}\` is not an http:// URL`);
        }
        if (next.rpc_cookie !== null && next.rpc_cookie.trim() === "") {
          throw fail("config", "configuration error: the cookie path is empty");
        }
        if (next.network !== settings.network) {
          // Switching networks opens a different wallet; the old one is locked and its
          // prepared sends are dropped.
          const old = active();
          if (old.wallet) old.wallet.unlocked = false;
          for (const [id, entry] of pending) if (entry.network === old.network) release(id);
        }
        settings = clone(next);
      }),

    // --- controls ----------------------------------------------------------------------------

    simulateIncoming(sat) {
      const { chain, wallet } = requireWallet();
      if (!Number.isSafeInteger(sat) || sat <= 0) throw new RangeError("sat must be a positive integer");
      const index = nextUnusedIndex(chain, wallet);
      wallet.revealed.external = Math.max(wallet.revealed.external, index + 1);
      const txid = hex(randomBytes(32));
      chain.txs.push({
        txid,
        spends: [],
        sentSat: 0,
        outputs: [outputFor(chain, wallet, txid, 0, sat, "external", index)],
        feeSat: null,
        firstSeen: nowSecs(),
        height: null,
        coinbase: false,
      });
      return txid;
    },
    mineBlocks: (n = 1, opts = {}) => mine(active(), n, opts.toWallet ?? false),
    setNodeOnline(online) {
      nodeOnline = online;
    },
    isNodeOnline: () => nodeOnline,
    tipHeight: () => active().tip,
    dispose() {
      if (miner) clearInterval(miner);
      miner = null;
    },
  };
}

// ---------------------------------------------------------------------------------------------
// BIP39 (mnemonic ↔ entropy with checksum)

let wordIndex: Map<string, number> | null = null;
const indexOfWord = (word: string) => {
  wordIndex ??= new Map(BIP39_ENGLISH.map((w, i) => [w, i]));
  return wordIndex.get(word);
};

const bits = (byte: number) => byte.toString(2).padStart(8, "0");

export function entropyToMnemonic(entropy: Uint8Array): string[] {
  const checksumBits = (entropy.length * 8) / 32;
  const firstHashByte = sha256(entropy)[0] ?? 0;
  const all = Array.from(entropy, bits).join("") + bits(firstHashByte).slice(0, checksumBits);
  const words: string[] = [];
  for (let i = 0; i < all.length; i += 11) {
    words.push(BIP39_ENGLISH[parseInt(all.slice(i, i + 11), 2)] ?? "");
  }
  return words;
}

/** Null when valid; otherwise a message that names positions, never words (like the core). */
export function checkMnemonic(words: string[]): string | null {
  if (![12, 15, 18, 21, 24].includes(words.length)) {
    return `expected 12, 15, 18, 21 or 24 words, got ${words.length}`;
  }
  let all = "";
  for (const [i, word] of words.entries()) {
    const index = indexOfWord(word);
    if (index === undefined) return `word ${i + 1} is not in the BIP39 English word list`;
    all += index.toString(2).padStart(11, "0");
  }
  const entropyBits = (all.length * 32) / 33;
  const entropy = new Uint8Array(entropyBits / 8);
  for (let i = 0; i < entropy.length; i++) entropy[i] = parseInt(all.slice(i * 8, i * 8 + 8), 2);
  const expected = bits(sha256(entropy)[0] ?? 0).slice(0, all.length - entropyBits);
  return all.slice(entropyBits) === expected
    ? null
    : "checksum mismatch (a word is probably mistyped or out of order)";
}

// ---------------------------------------------------------------------------------------------
// bech32 / bech32m segwit addresses (BIP173, BIP350)

const CHARSET = "qpzry9x8gf2tvdw0s3jn54khce6mua7l";
const BECH32M_CONST = 0x2bc830a3;

function polymod(values: number[]): number {
  const gen = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
  let chk = 1;
  for (const v of values) {
    const top = chk >>> 25;
    chk = ((chk & 0x1ffffff) << 5) ^ v;
    for (let i = 0; i < 5; i++) if ((top >>> i) & 1) chk ^= gen[i] ?? 0;
  }
  return chk;
}

const hrpExpand = (hrp: string) => [
  ...Array.from(hrp, (c) => c.charCodeAt(0) >>> 5),
  0,
  ...Array.from(hrp, (c) => c.charCodeAt(0) & 31),
];

function convertBits(data: ArrayLike<number>, from: number, to: number, pad: boolean): number[] | null {
  let acc = 0;
  let width = 0;
  const out: number[] = [];
  const max = (1 << to) - 1;
  for (const value of Array.from(data)) {
    acc = (acc << from) | value;
    width += from;
    while (width >= to) {
      width -= to;
      out.push((acc >>> width) & max);
    }
  }
  if (pad) {
    if (width > 0) out.push((acc << (to - width)) & max);
  } else if (width >= from || ((acc << (to - width)) & max) !== 0) {
    return null;
  }
  return out;
}

export function encodeSegwit(hrp: string, version: number, program: Uint8Array): string {
  const data = [version, ...(convertBits(program, 8, 5, true) ?? [])];
  const mod = polymod([...hrpExpand(hrp), ...data, 0, 0, 0, 0, 0, 0]) ^ (version === 0 ? 1 : BECH32M_CONST);
  const checksum = Array.from({ length: 6 }, (_, p) => (mod >>> (5 * (5 - p))) & 31);
  return `${hrp}1${[...data, ...checksum].map((d) => CHARSET[d]).join("")}`;
}

/** The address's parts, or a short reason it isn't a segwit address. */
export function decodeSegwit(address: string): { hrp: string; version: number; program: number[] } | string {
  if (address !== address.toLowerCase() && address !== address.toUpperCase()) return "mixed upper and lower case";
  const text = address.toLowerCase();
  const sep = text.lastIndexOf("1");
  if (sep < 1 || sep + 7 > text.length || text.length > 90) return "not a bech32 address";
  const hrp = text.slice(0, sep);
  const data = Array.from(text.slice(sep + 1), (c) => CHARSET.indexOf(c));
  if (data.some((d) => d < 0)) return "invalid character";
  const version = data[0] ?? -1;
  const constant = polymod([...hrpExpand(hrp), ...data]);
  if (constant !== (version === 0 ? 1 : BECH32M_CONST)) return "invalid checksum";
  const program = convertBits(data.slice(1, -6), 5, 8, false);
  if (!program || version > 16) return "invalid witness program";
  if (version === 0 ? program.length !== 20 && program.length !== 32 : program.length < 2 || program.length > 40) {
    return "invalid witness program length";
  }
  return { hrp, version, program };
}

// ---------------------------------------------------------------------------------------------
// SHA-256 (FIPS 180-4), synchronous so the mock needs no async crypto

const K = [
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
  0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
  0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
  0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
  0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
  0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
  0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
  0xc67178f2,
];

const rotr = (x: number, n: number) => (x >>> n) | (x << (32 - n));

export function sha256(data: Uint8Array): Uint8Array {
  const h = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
  const total = Math.ceil((data.length + 9) / 64) * 64;
  const buf = new Uint8Array(total);
  buf.set(data);
  buf[data.length] = 0x80;
  const view = new DataView(buf.buffer);
  const bitLen = data.length * 8;
  view.setUint32(total - 8, Math.floor(bitLen / 2 ** 32));
  view.setUint32(total - 4, bitLen >>> 0);
  const w = new Array<number>(64).fill(0);
  for (let off = 0; off < total; off += 64) {
    for (let i = 0; i < 16; i++) w[i] = view.getUint32(off + i * 4);
    for (let i = 16; i < 64; i++) {
      const w15 = w[i - 15] ?? 0;
      const w2 = w[i - 2] ?? 0;
      const s0 = rotr(w15, 7) ^ rotr(w15, 18) ^ (w15 >>> 3);
      const s1 = rotr(w2, 17) ^ rotr(w2, 19) ^ (w2 >>> 10);
      w[i] = ((w[i - 16] ?? 0) + s0 + (w[i - 7] ?? 0) + s1) >>> 0;
    }
    let [a, b, c, d, e, f, g, hh] = h as [number, number, number, number, number, number, number, number];
    for (let i = 0; i < 64; i++) {
      const t1 = (hh + (rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25)) + ((e & f) ^ (~e & g)) + (K[i] ?? 0) + (w[i] ?? 0)) >>> 0;
      const t2 = ((rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22)) + ((a & b) ^ (a & c) ^ (b & c))) >>> 0;
      hh = g;
      g = f;
      f = e;
      e = (d + t1) >>> 0;
      d = c;
      c = b;
      b = a;
      a = (t1 + t2) >>> 0;
    }
    const next = [a, b, c, d, e, f, g, hh];
    for (let i = 0; i < 8; i++) h[i] = ((h[i] ?? 0) + (next[i] ?? 0)) >>> 0;
  }
  const out = new Uint8Array(32);
  const outView = new DataView(out.buffer);
  for (let i = 0; i < 8; i++) outView.setUint32(i * 4, h[i] ?? 0);
  return out;
}
