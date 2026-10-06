// In-memory stand-in for the Rust command layer, so the UI can be built, demoed
// (`npm run dev:mock`) and tested without Rust or a node.
//
// It follows btcw-core's rules closely enough that the UI meets every real error path: real
// BIP39 phrases and bech32 addresses, dust and fee checks, sync-driven confirmations, one
// prepared payment at a time, the backup flag, the address book and label rules, and RBF
// minimums. Failures reject with `ApiError { code, message }`, like Tauri.
//
// Simplifications: one fake node per network, coins only to and from this wallet, no reorgs or
// evictions, legacy addresses accepted by shape only, and a fee bump never adds a coin.

import type { UnlistenFn } from "@tauri-apps/api/event";
import type { WalletApi } from "./api";
import type {
  AddressRow,
  ApiError,
  AppInfo,
  BalanceView,
  Contact,
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
const PENDING_SEND_TTL_MS = 10 * 60_000;
const BACKUP_CHECK_WORDS = 3;
const MAX_NAME_CHARS = 40;
const MAX_NOTE_CHARS = 200;
const MAX_LABEL_CHARS = 100;
const SEGWIT_PREFIXES = ["bc1", "tb1", "bcrt1"];

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
  /** Set for payments this wallet sent: who was paid how much (a fee bump keeps both). */
  payment?: { to: string; amount: number };
}

interface Wallet {
  password: string;
  /** The recovery phrase. Only `revealPhrase` and the backup check read it. */
  words: string[];
  backupVerified: boolean;
  seed: Uint8Array;
  birthday: number;
  syncedHeight: number;
  /** Transactions the wallet has learned about (by syncing or by sending them). */
  seen: Set<string>;
  revealed: Record<Keychain, number>;
  unlocked: boolean;
  /** The address book (`btcw_contacts`), sorted by name ignoring case. */
  contacts: Contact[];
  /** `btcw_labels`: txid → label. */
  labels: Map<string, string>;
}

interface Chain {
  network: NetworkName;
  tip: number;
  anchorTip: number;
  anchorTime: number;
  minedTimes: Map<number, number>;
  txs: MockTx[];
  wallet: Wallet | null;
  /** Transactions replaced by a fee bump (gone from the node's mempool): old txid → new txid. */
  replaced: Map<string, string>;
}

interface Pending {
  network: NetworkName;
  /** `Date.now()` when prepared; previews expire like the Rust side's (10 minutes). */
  created: number;
  preview: PreparedSend["preview"];
  inputs: Output[];
  change: Output | null;
  /** A fee bump: the txid it replaces. Its change output is the original's (not a new address). */
  replaces: string | null;
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
      chain = {
        network,
        tip,
        anchorTip: tip,
        anchorTime: nowSecs() - 240,
        minedTimes: new Map(),
        txs: [],
        wallet: null,
        replaced: new Map(),
      };
      chains.set(network, chain);
    }
    return chain;
  };

  let settings: Settings = {
    network: options.network ?? "testnet4",
    rpc_url: null,
    rpc_cookie: null,
    auto_lock_minutes: 5,
    mainnet_opt_in: false,
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

  function checkPassword(wallet: Wallet, password: string): void {
    if (password !== wallet.password) throw fail("wrong_password", "wrong password or corrupted keystore");
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

  function checkFeeRate(feeRate: number | null): void {
    if (feeRate !== null && !(Number.isFinite(feeRate) && feeRate > 0)) {
      throw fail("tx_build", "could not build transaction: the fee rate must be a positive number of sat/vB");
    }
    if (feeRate !== null && feeRate < 1) {
      throw fail("tx_build", `could not build transaction: fee rate ${feeRate} sat/vB is below the 1 sat/vB minimum that nodes relay`);
    }
    if (feeRate !== null && feeRate > 25_000) {
      throw fail("tx_build", `could not build transaction: fee rate ${feeRate} sat/vB is above the 25000 sat/vB safety limit`);
    }
  }

  function prepare(to: string, amountSat: number, feeRate: number | null): PreparedSend {
    const { chain, wallet } = requireWallet();
    requireUnlocked(wallet);
    // An address is checked at once; a name after the cheap checks, like the bridge.
    let typed: string | null = null;
    try {
      typed = checkAddress(to, chain.network);
    } catch (e) {
      if ((e as ApiError).code !== "invalid_address" || to.trim() === "" || looksLikeAddress(to)) throw e;
    }
    if (!Number.isSafeInteger(amountSat) || amountSat <= 0) {
      throw fail("tx_build", "could not build transaction: the amount must be a whole number of satoshis above zero");
    }
    if (typed !== null && amountSat < DUST_LIMIT_SAT) {
      throw fail("dust_amount", `amount ${formatBtc(amountSat)} BTC is below the dust limit`);
    }
    checkFeeRate(feeRate);
    // One prepared payment at a time: this one replaces any earlier preview.
    for (const id of [...pending.keys()]) release(id);
    const recipient = typed !== null ? { address: typed, contact: null } : resolveRecipient(chain, wallet, to);
    const address = recipient.address;
    if (amountSat < DUST_LIMIT_SAT) {
      throw fail("dust_amount", `amount ${formatBtc(amountSat)} BTC is below the dust limit`);
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
        return remember(chain, address, recipient.contact, amountSat, feeWithChange, vbytes(inputs.length, 2), inputs, change);
      }
      // Change would be dust: drop it and let the miner have the remainder (BDK does the same).
      const feeNoChange = Math.ceil(vbytes(inputs.length, 1) * rate);
      if (inSum >= amountSat + feeNoChange) {
        return remember(chain, address, recipient.contact, amountSat, inSum - amountSat, vbytes(inputs.length, 1), inputs, null);
      }
    }
    const available = spendable.reduce((sum, out) => sum + out.value, 0);
    const needed = amountSat + Math.ceil(vbytes(Math.max(1, spendable.length), 2) * rate);
    throw fail("insufficient_funds", `insufficient funds: need ${formatBtc(needed)} BTC, available ${formatBtc(available)} BTC`);
  }

  function remember(
    chain: Chain,
    to: string,
    contact: string | null,
    amount: number,
    fee: number,
    vsize: number,
    inputs: Output[],
    change: Output | null,
    replaces: string | null = null,
  ): PreparedSend {
    const id = hex(randomBytes(16));
    const preview = {
      to,
      amount_sat: amount,
      fee_sat: fee,
      fee_rate_sat_vb: Math.round((fee / vsize) * 100) / 100,
      vsize,
      change_sat: change ? change.value : null,
      total_sat: amount + fee,
      contact,
      replaces,
    };
    pending.set(id, { network: chain.network, created: Date.now(), preview, inputs, change, replaces });
    return { id, preview };
  }

  function release(id: string): void {
    const entry = pending.get(id);
    if (!entry) return;
    pending.delete(id);
    const wallet = chainFor(entry.network).wallet;
    // `tx::cancel`: un-reveal the change address if it was the last one. A fee bump reuses the
    // original's, so there is nothing to give back.
    if (entry.change && !entry.replaces && wallet && wallet.revealed.internal === entry.change.index + 1) {
      wallet.revealed.internal -= 1;
    }
  }

  function confirm(id: string): { txid: string } {
    const { chain, wallet } = requireWallet();
    requireUnlocked(wallet);
    const entry = pending.get(id);
    if (!entry) {
      throw fail(
        "tx_build",
        "could not build transaction: this payment is no longer waiting to be sent (it was sent, cancelled or replaced); review it again",
      );
    }
    // Like the Rust side: the prepared payment is used up by this call, whatever happens next.
    if (entry.network !== chain.network || Date.now() - entry.created > PENDING_SEND_TTL_MS) {
      release(id);
      throw fail("tx_build", "could not build transaction: this payment preview expired after 10 minutes; review it again");
    }
    if (!nodeOnline) {
      release(id);
      requireNode();
    }
    if (entry.replaces) return confirmBump(chain, wallet, id, entry, entry.replaces);
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
      payment: { to: entry.preview.to, amount: entry.preview.amount_sat },
    });
    wallet.seen.add(txid); // `tx::record_broadcast`
    pending.delete(id);
    return { txid };
  }

  // --- speed up (fee bump) ---------------------------------------------------------------------

  const ceilCents = (satVb: number) => Math.ceil(satVb * 100 - 1e-9) / 100;

  function parseTxid(txid: string): string {
    const text = txid.trim();
    if (!/^[0-9a-fA-F]{64}$/.test(text)) {
      throw fail("tx_not_found", `transaction not found: \`${txid.slice(0, 80)}\` is not a transaction id`);
    }
    return text.toLowerCase();
  }

  /** `tx::bump_target`: why `txid` can't be replaced, or what replacing it takes. */
  function bumpTarget(chain: Chain, wallet: Wallet, txid: string) {
    const refuse = (why: string) => fail("tx_build", `could not build transaction: cannot speed up ${txid}: ${why}`);
    const tx = visibleTxs(chain, wallet).find((t) => t.txid === txid);
    if (!tx) {
      if (chain.replaced.has(txid)) throw refuse("it was replaced by another transaction or dropped from the mempool");
      throw fail("tx_not_found", `transaction not found: ${txid}`);
    }
    const status = statusOf(chain, wallet, tx);
    if (status.state === "confirmed") {
      throw refuse(`it is already confirmed (block ${status.height}); only an unconfirmed payment can be replaced`);
    }
    if (!tx.payment || tx.feeSat === null || tx.spends.length === 0) {
      throw refuse("it was not sent by this wallet (it spends none of this wallet's coins), so only its sender can replace it");
    }
    const outs = new Set(tx.outputs.map((o) => o.outpoint));
    const child = visibleTxs(chain, wallet).find((t) => t.spends.some((s) => outs.has(s)));
    if (child) throw refuse(`payment ${child.txid} spends one of its outputs, and replacing ${txid} would cancel that payment too`);
    const vsize = vbytes(tx.spends.length, tx.outputs.length + 1);
    const oldRate = Math.floor((tx.feeSat / vsize) * 100) / 100;
    // BDK: old rate + 1 sat/vB; BIP125 rule 4: old fee + 1 sat/vB × size.
    const min = ceilCents(Math.max(oldRate + 1, (tx.feeSat + vsize) / vsize));
    return { tx, payment: tx.payment, oldFee: tx.feeSat, oldRate, vsize, min };
  }

  function prepareBump(rawTxid: string, feeRate: number | null): PreparedSend {
    const { chain, wallet } = requireWallet();
    requireUnlocked(wallet);
    const txid = parseTxid(rawTxid);
    checkFeeRate(feeRate);
    for (const id of [...pending.keys()]) release(id);
    bumpTarget(chain, wallet, txid); // refused from the wallet before the node is asked
    requireNode();
    applySync(chain, wallet);
    const target = bumpTarget(chain, wallet, txid);
    const rate = feeRate ?? Math.max(defaultFeeRate, Math.ceil(target.min));
    if (rate < target.min) {
      throw fail(
        "tx_build",
        `could not build transaction: cannot speed up ${txid}: a fee rate of ${rate} sat/vB is too low; it pays ${target.oldRate.toFixed(2)} sat/vB now, and a replacement must pay at least 1 sat/vB more (BIP125), so at least ${target.min.toFixed(2)} sat/vB`,
      );
    }
    const fee = Math.ceil(target.vsize * rate);
    const extra = fee - target.oldFee;
    const change = target.tx.outputs.find((o) => o.keychain === "internal") ?? null;
    if (!change || change.value - extra < DUST_LIMIT_SAT) {
      throw fail(
        "insufficient_funds",
        `insufficient funds: need ${formatBtc(extra)} BTC more for the fee, available ${formatBtc(change?.value ?? 0)} BTC`,
      );
    }
    const allOutputs = chain.txs.flatMap((t) => t.outputs);
    const inputs = target.tx.spends.map((op) => allOutputs.find((o) => o.outpoint === op)).filter((o): o is Output => !!o);
    const newChange: Output = { ...change, outpoint: "", value: change.value - extra };
    return remember(chain, target.payment.to, null, target.payment.amount, fee, target.vsize, inputs, newChange, txid);
  }

  function confirmBump(chain: Chain, wallet: Wallet, id: string, entry: Pending, replaces: string): { txid: string } {
    // Like the bridge: sync, then check the original can still be replaced.
    applySync(chain, wallet);
    let target: ReturnType<typeof bumpTarget>;
    try {
      target = bumpTarget(chain, wallet, replaces);
    } catch (e) {
      release(id);
      throw e;
    }
    const txid = hex(randomBytes(32));
    const outputs = entry.change ? [{ ...entry.change, outpoint: `${txid}:1` }] : [];
    // The node drops the original from its mempool; the wallet shows only the replacement.
    chain.txs = chain.txs.filter((t) => t !== target.tx);
    chain.replaced.set(replaces, txid);
    wallet.seen.delete(replaces);
    chain.txs.push({
      txid,
      spends: [...target.tx.spends],
      sentSat: target.tx.sentSat,
      outputs,
      feeSat: entry.preview.fee_sat,
      firstSeen: nowSecs(),
      height: null,
      coinbase: false,
      payment: { ...target.payment },
    });
    wallet.seen.add(txid);
    const label = wallet.labels.get(replaces);
    if (label !== undefined && !wallet.labels.has(txid)) wallet.labels.set(txid, label);
    pending.delete(id);
    return { txid };
  }

  // --- address book and labels -----------------------------------------------------------------

  /** `book::checked_text`: trimmed, not empty, at most `max` characters, no control characters. */
  function checkedText(what: string, text: string, max: number): string {
    const t = text.trim();
    const chars = [...t].length;
    if (chars === 0) throw fail("contact", `${what} can't be empty`);
    if (chars > max) throw fail("contact", `${what} can be at most ${max} characters (this one has ${chars})`);
    if (/[\u0000-\u001f\u007f-\u009f]/.test(t)) {
      throw fail("contact", `${what} can't contain control characters such as line breaks or tabs`);
    }
    return t;
  }

  function contactName(name: string): string {
    const text = checkedText("a contact name", name, MAX_NAME_CHARS);
    if (looksLikeAddress(text)) {
      throw fail(
        "contact",
        `\`${text}\` looks like a Bitcoin address, so it can't be a contact name (a recipient must always be clearly one or the other)`,
      );
    }
    return text;
  }

  const findContact = (wallet: Wallet, name: string) =>
    wallet.contacts.find((c) => c.name.toLowerCase() === name.trim().toLowerCase());

  function existingContact(wallet: Wallet, name: string): Contact {
    const found = findContact(wallet, name);
    if (!found) throw fail("contact", `no contact is named \`${name.trim().slice(0, MAX_NAME_CHARS)}\``);
    return found;
  }

  function sortContacts(wallet: Wallet): void {
    wallet.contacts.sort((a, b) => {
      const x = a.name.toLowerCase();
      const y = b.name.toLowerCase();
      return x < y ? -1 : x > y ? 1 : a.name < b.name ? -1 : a.name > b.name ? 1 : 0;
    });
  }

  /** `WalletService::resolve_recipient`: an address first, then (if not address-like) a name. */
  function resolveRecipient(chain: Chain, wallet: Wallet, input: string): { address: string; contact: string | null } {
    const text = input.trim();
    try {
      return { address: checkAddress(text, chain.network), contact: null };
    } catch (e) {
      if ((e as ApiError).code !== "invalid_address" || text === "" || looksLikeAddress(text)) throw e;
    }
    const found = findContact(wallet, text);
    if (!found) throw fail("contact", `no contact is named \`${text}\`, and it is not a valid address either`);
    return { address: checkAddress(found.address, chain.network), contact: found.name };
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
        label: wallet.labels.get(tx.txid) ?? null,
      };
    });
    // Unconfirmed first (latest seen first), then by height, newest first.
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
          backup_verified: chain.wallet ? chain.wallet.backupVerified : null,
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
          words: [...mnemonic],
          backupVerified: false,
          seed: sha256(utf8(mnemonic.join(" "))),
          birthday: nodeOnline ? chain.tip : 0,
          syncedHeight: 0,
          seen: new Set(),
          revealed: { external: 0, internal: 0 },
          unlocked: true,
          contacts: [],
          labels: new Map(),
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
          words,
          // The user just typed the phrase, so the backup is known to be correct.
          backupVerified: true,
          seed: sha256(utf8(words.join(" "))),
          birthday: birthday ?? 0,
          syncedHeight: 0,
          seen: new Set(),
          revealed: { external: 0, internal: 0 },
          unlocked: true,
          contacts: [],
          labels: new Map(),
        };
        chain.wallet = wallet;
        seedHistory(chain, wallet);
      }, latency * 4),

    unlock: (password) =>
      answer(() => {
        const { wallet } = requireWallet();
        checkPassword(wallet, password);
        wallet.unlocked = true;
      }, latency * 3), // Argon2id is meant to be slow

    lock: () =>
      answer(() => {
        const chain = active();
        if (chain.wallet) chain.wallet.unlocked = false;
        // Locking drops the prepared payment with the keys.
        for (const [id, entry] of pending) if (entry.network === chain.network) release(id);
      }),

    keepAlive: () => answer(() => undefined),

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
        // Syncs when the node answers; otherwise the status is as of the last sync.
        if (nodeOnline) applySync(chain, wallet);
        const tx = visibleTxs(chain, wallet).find((t) => t.txid === txid);
        return tx ? statusOf(chain, wallet, tx) : null;
      }),

    minFeeBumpRate: (txid) =>
      answer(() => {
        const { chain, wallet } = requireWallet();
        return bumpTarget(chain, wallet, parseTxid(txid)).min;
      }),
    prepareFeeBump: (txid, feeRate) => answer(() => prepareBump(txid, feeRate), latency),

    listContacts: () =>
      answer((): Contact[] => {
        const { wallet } = requireWallet();
        return wallet.contacts;
      }),

    addContact: (name, address, note) =>
      answer((): Contact => {
        const { chain, wallet } = requireWallet();
        const canonical = checkAddress(address, chain.network);
        const clean = contactName(name);
        const cleanNote = note === null || note.trim() === "" ? null : checkedText("a note", note, MAX_NOTE_CHARS);
        const existing = findContact(wallet, clean);
        if (existing) throw fail("contact", `a contact named \`${existing.name}\` already exists`);
        const contact = { name: clean, address: canonical, note: cleanNote };
        wallet.contacts.push(contact);
        sortContacts(wallet);
        return contact;
      }),

    removeContact: (name) =>
      answer((): Contact => {
        const { wallet } = requireWallet();
        const contact = existingContact(wallet, name);
        wallet.contacts = wallet.contacts.filter((c) => c !== contact);
        return contact;
      }),

    renameContact: (oldName, newName) =>
      answer((): Contact => {
        const { wallet } = requireWallet();
        const contact = existingContact(wallet, oldName);
        const clean = contactName(newName);
        const other = findContact(wallet, clean);
        if (other && other !== contact) throw fail("contact", `a contact named \`${other.name}\` already exists`);
        contact.name = clean;
        sortContacts(wallet);
        return contact;
      }),

    setLabel: (txid, label) =>
      answer((): string => {
        const { chain, wallet } = requireWallet();
        const id = parseTxid(txid);
        const text = checkedText("a label", label, MAX_LABEL_CHARS);
        if (!visibleTxs(chain, wallet).some((t) => t.txid === id)) throw fail("tx_not_found", `transaction not found: ${id}`);
        wallet.labels.set(id, text);
        return text;
      }),

    clearLabel: (txid) =>
      answer(() => {
        const { chain, wallet } = requireWallet();
        const id = parseTxid(txid);
        if (!wallet.labels.delete(id) && !visibleTxs(chain, wallet).some((t) => t.txid === id)) {
          throw fail("tx_not_found", `transaction not found: ${id}`);
        }
      }),

    getSettings: () => answer(() => settings),

    setSettings: (next) =>
      answer(() => {
        if (!networks.includes(next.network) || (next.network === "bitcoin" && !next.mainnet_opt_in)) {
          if (next.network === "bitcoin") {
            throw fail("mainnet_disabled", "mainnet is disabled; build with `--features mainnet` and opt in explicitly");
          }
          throw fail("config", `configuration error: network ${next.network} is not available`);
        }
        if (!Number.isInteger(next.auto_lock_minutes) || next.auto_lock_minutes < 1 || next.auto_lock_minutes > 60) {
          throw fail("config", "configuration error: auto-lock must be between 1 and 60 minutes");
        }
        if (next.rpc_url !== null && !/^https?:\/\/[^\s/]+(\/\S*)?$/.test(next.rpc_url)) {
          throw fail("config", `configuration error: \`${next.rpc_url}\` is not an http:// URL`);
        }
        if (next.rpc_cookie !== null && next.rpc_cookie.trim() === "") {
          throw fail("config", "configuration error: the cookie path is empty");
        }
        if (next.network !== settings.network) {
          // Switching networks locks the old wallet and drops its prepared sends.
          const old = active();
          if (old.wallet) old.wallet.unlocked = false;
          for (const [id, entry] of pending) if (entry.network === old.network) release(id);
        }
        settings = clone(next);
      }),

    backupChallenge: (password) =>
      answer((): number[] => {
        const { wallet } = requireWallet();
        checkPassword(wallet, password);
        return randomPositions(BACKUP_CHECK_WORDS, wallet.words.length);
      }, latency * 3),

    verifyBackup: (password, answers) =>
      answer(() => {
        const { wallet } = requireWallet();
        checkPassword(wallet, password);
        const total = wallet.words.length;
        const seen = new Set<number>();
        for (const [position] of answers) {
          if (!Number.isInteger(position) || position < 1 || position > total) {
            throw fail("config", `configuration error: word position ${position} is outside 1..=${total}`);
          }
          if (seen.has(position)) throw fail("config", `configuration error: word position ${position} was given twice`);
          seen.add(position);
        }
        const needed = Math.min(BACKUP_CHECK_WORDS, total);
        if (seen.size < needed) {
          throw fail("config", `configuration error: the backup check needs at least ${needed} words, got ${seen.size}`);
        }
        const wrong = answers
          .filter(([position, word]) => word.trim().toLowerCase() !== wallet.words[position - 1])
          .map(([position]) => position)
          .sort((a, b) => a - b);
        if (wrong.length > 0) throw fail("backup_mismatch", backupMismatchMessage(wrong));
        wallet.backupVerified = true;
      }, latency * 3),

    revealPhrase: (password) =>
      answer(() => {
        const { wallet } = requireWallet();
        checkPassword(wallet, password);
        return { mnemonic: [...wallet.words] };
      }, latency * 3),

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

/** `book::looks_like_address`: never looked up as a contact's name. */
export function looksLikeAddress(input: string): boolean {
  const text = input.trim();
  const lower = text.toLowerCase();
  if (SEGWIT_PREFIXES.some((prefix) => lower.startsWith(prefix))) return true;
  if ([...text].length > MAX_NAME_CHARS) return true;
  if (typeof decodeSegwit(text) !== "string") return true;
  return /^[1-9A-HJ-NP-Za-km-z]{25,34}$/.test(text) && /^[13mn2]/.test(text);
}

/** `count` distinct positions in 1..=total, ascending, from the OS RNG without modulo bias. */
function randomPositions(count: number, total: number): number[] {
  const picked = new Set<number>();
  const zone = 2 ** 32 - (2 ** 32 % total);
  const random = new Uint32Array(1);
  while (picked.size < Math.min(count, total)) {
    crypto.getRandomValues(random);
    const value = random[0] ?? 0;
    if (value < zone) picked.add((value % total) + 1);
  }
  return [...picked].sort((a, b) => a - b);
}

/** The core's `BackupMismatch` text: positions only, never words. */
function backupMismatchMessage(positions: number[]): string {
  if (positions.length === 1) return `word ${positions[0]} does not match your recovery phrase`;
  return `words ${positions.slice(0, -1).join(", ")} and ${positions.at(-1)} do not match your recovery phrase`;
}

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
