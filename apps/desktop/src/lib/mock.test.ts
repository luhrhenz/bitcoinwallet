import { checkMnemonic, createMockApi, decodeSegwit, encodeSegwit, entropyToMnemonic, sha256 } from "./mock";
import { BIP39_ENGLISH } from "./mock-wordlist";
import type { ApiError, SyncProgress } from "./types";

const PASSWORD = "correct horse battery";
const ABANDON = `${"abandon ".repeat(11)}about`;

async function rejection(promise: Promise<unknown>): Promise<ApiError> {
  try {
    await promise;
  } catch (e) {
    return e as ApiError;
  }
  throw new Error("expected the promise to reject");
}

const hex = (bytes: Uint8Array) => Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");

describe("mock building blocks", () => {
  it("hashes like SHA-256", () => {
    expect(hex(sha256(new TextEncoder().encode("abc")))).toBe(
      "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    );
    expect(hex(sha256(new Uint8Array(0)))).toBe(
      "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    );
    // Two blocks of padding.
    expect(hex(sha256(new TextEncoder().encode("a".repeat(64))))).toBe(
      "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb",
    );
  });

  it("speaks BIP39", () => {
    expect(BIP39_ENGLISH).toHaveLength(2048);
    expect(entropyToMnemonic(new Uint8Array(16)).join(" ")).toBe(ABANDON);
    expect(checkMnemonic(ABANDON.split(" "))).toBeNull();
    expect(checkMnemonic("abandon ".repeat(12).trim().split(" "))).toMatch(/checksum/);
    expect(checkMnemonic(ABANDON.split(" ").slice(1))).toMatch(/got 11/);
    expect(checkMnemonic([...ABANDON.split(" ").slice(0, 4), "bitcoinx", ...ABANDON.split(" ").slice(5)])).toBe(
      "word 5 is not in the BIP39 English word list",
    );
  });

  it("speaks bech32 (BIP173 test vector)", () => {
    const program = Uint8Array.from("751e76e8199196d454941c45d1b3a323f1433bd6".match(/../g) ?? [], (b) => parseInt(b, 16));
    expect(encodeSegwit("bc", 0, program)).toBe("bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4");
    expect(encodeSegwit("tb", 0, program)).toBe("tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx");
    expect(decodeSegwit("tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx")).toMatchObject({ hrp: "tb", version: 0 });
    expect(decodeSegwit("tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsy")).toBe("invalid checksum");
    expect(decodeSegwit("tb1qW508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx")).toMatch(/mixed/);
  });
});

describe("mock wallet", () => {
  it("starts empty on testnet4", async () => {
    const api = createMockApi({ latencyMs: 0 });
    expect(await api.appInfo()).toEqual({
      network: "testnet4",
      networks: ["testnet4", "signet", "regtest"],
      wallet_exists: false,
      unlocked: false,
      synced_height: null,
      backup_verified: null,
    });
    expect(await rejection(api.balance())).toMatchObject({ code: "wallet_not_found" });
  });

  it("creates a wallet with a valid phrase and enforces the password rules", async () => {
    const api = createMockApi({ latencyMs: 0 });
    expect(await rejection(api.createWallet(12, "short"))).toEqual({
      code: "weak_password",
      message: "password must be at least 8 characters",
    });
    const { mnemonic } = await api.createWallet(24, PASSWORD);
    expect(mnemonic).toHaveLength(24);
    expect(checkMnemonic(mnemonic)).toBeNull();
    expect(await rejection(api.createWallet(12, PASSWORD))).toMatchObject({ code: "wallet_exists" });

    await api.lock();
    expect((await api.appInfo()).unlocked).toBe(false);
    expect(await rejection(api.unlock("wrong password"))).toMatchObject({ code: "wrong_password" });
    await api.unlock(PASSWORD);
    expect((await api.appInfo()).unlocked).toBe(true);
  });

  it("reuses the receive address until it is paid, and works watch-only", async () => {
    const api = createMockApi({ latencyMs: 0 });
    await api.createWallet(12, PASSWORD);
    await api.lock();
    const first = await api.newAddress();
    expect(first.address).toMatch(/^tb1q[a-z0-9]{38}$/);
    expect(decodeSegwit(first.address)).toMatchObject({ hrp: "tb", version: 0 });
    expect((await api.newAddress()).address).toBe(first.address);

    api.simulateIncoming(150_000);
    // The wallet hasn't synced yet, so it doesn't know the address was paid.
    expect((await api.newAddress()).index).toBe(0);
    await api.sync();
    expect(await api.balance()).toEqual({ confirmed_sat: 0, unconfirmed_sat: 150_000, immature_sat: 0, total_sat: 150_000 });
    const second = await api.newAddress();
    expect(second.index).toBe(1);
    expect(await api.listAddresses()).toEqual([
      { ...first, used: true },
      { ...second, used: false },
    ]);
  });

  it("emits sync progress and reports the tip", async () => {
    const api = createMockApi({ latencyMs: 0, syncStepMs: 0 });
    await api.restoreWallet(ABANDON, PASSWORD, null);
    const events: SyncProgress[] = [];
    const unlisten = await api.onSyncProgress((p) => events.push(p));
    const report = await api.sync();
    unlisten();
    expect(events.length).toBeGreaterThan(5);
    expect(events.at(-1)).toEqual({ height: report.tip_height, tip_height: report.tip_height });
    expect(report.blocks_scanned).toBe(report.tip_height);
    // A restore finds the wallet's past.
    expect((await api.history()).length).toBe(3);
    expect((await api.balance()).confirmed_sat).toBe(80_000 + 189_718);
    expect((await api.appInfo()).synced_height).toBe(report.tip_height);
  });

  it("rejects bad restore input with the core's codes", async () => {
    const api = createMockApi({ latencyMs: 0 });
    expect(await rejection(api.restoreWallet("abandon abandon", PASSWORD, null))).toMatchObject({
      code: "invalid_mnemonic",
    });
    expect(await rejection(api.restoreWallet(ABANDON, "1234567", null))).toMatchObject({ code: "weak_password" });
  });

  it("prepares, confirms and tracks a send", async () => {
    const api = createMockApi({ latencyMs: 0 });
    await api.createWallet(12, PASSWORD);
    api.simulateIncoming(1_000_000);
    api.mineBlocks(1);
    await api.sync();
    const to = encodeSegwit("tb", 0, new Uint8Array(20).fill(7));

    await api.lock();
    expect(await rejection(api.prepareSend(to, 10_000, null))).toMatchObject({ code: "locked" });
    await api.unlock(PASSWORD);

    expect(await rejection(api.prepareSend("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080", 10_000, null))).toMatchObject({
      code: "network_mismatch",
    });
    expect(await rejection(api.prepareSend("bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4", 10_000, null))).toMatchObject({
      code: "network_mismatch",
    });
    expect(await rejection(api.prepareSend("tb1qnotanaddress", 10_000, null))).toMatchObject({ code: "invalid_address" });
    expect(await rejection(api.prepareSend(to, 293, null))).toMatchObject({ code: "dust_amount" });
    expect(await rejection(api.prepareSend(to, 2_000_000, null))).toMatchObject({ code: "insufficient_funds" });

    const { id, preview } = await api.prepareSend(to, 100_000, 3);
    expect(preview).toEqual({
      to,
      amount_sat: 100_000,
      fee_sat: 423,
      fee_rate_sat_vb: 3,
      vsize: 141,
      change_sat: 1_000_000 - 100_000 - 423,
      total_sat: 100_423,
    });

    const { txid } = await api.confirmSend(id);
    expect(await api.txStatus(txid)).toMatchObject({ state: "unconfirmed" });
    expect(await api.balance()).toMatchObject({ confirmed_sat: 0, unconfirmed_sat: 899_577 });
    expect((await api.history())[0]).toMatchObject({ txid, net_sat: -100_423, fee_sat: 423 });

    api.mineBlocks(1);
    expect(await api.txStatus(txid)).toMatchObject({ state: "confirmed", confirmations: 1 });
    api.mineBlocks(1);
    expect(await api.txStatus(txid)).toMatchObject({ state: "confirmed", confirmations: 2 });
    expect(await rejection(api.confirmSend(id))).toMatchObject({ code: "tx_build" });
  });

  it("drops dust change into the fee; a new preview replaces the last one", async () => {
    const api = createMockApi({ latencyMs: 0 });
    await api.createWallet(12, PASSWORD);
    api.simulateIncoming(10_000);
    api.mineBlocks(1);
    await api.sync();
    const to = encodeSegwit("tb", 0, new Uint8Array(20).fill(9));
    const { id, preview } = await api.prepareSend(to, 9_700, 1);
    expect(preview.change_sat).toBeNull();
    expect(preview.vsize).toBe(110);
    expect(preview.fee_sat).toBe(300);
    // One prepared payment at a time (like the Rust side): a new preview replaces the open one,
    // whose coin is free again, and the old id is dead.
    const second = await api.prepareSend(to, 1_000, 1);
    expect(second.preview.change_sat).toBe(10_000 - 1_000 - 141);
    expect(await rejection(api.confirmSend(id))).toMatchObject({ code: "tx_build" });
    await api.cancelSend(second.id);
    expect(await rejection(api.confirmSend(second.id))).toMatchObject({ code: "tx_build" });
  });

  it("fails node-backed commands with rpc when the node is down", async () => {
    const api = createMockApi({ latencyMs: 0 });
    await api.createWallet(12, PASSWORD);
    api.setNodeOnline(false);
    expect(await rejection(api.sync())).toMatchObject({ code: "rpc" });
    // Watch-only views still work offline.
    expect((await api.balance()).total_sat).toBe(0);
  });

  it("tracks the backup like the core: created unverified, checked with the password", async () => {
    const api = createMockApi({ latencyMs: 0 });
    const { mnemonic } = await api.createWallet(12, PASSWORD);
    expect((await api.appInfo()).backup_verified).toBe(false);

    expect(await rejection(api.backupChallenge("nope nope"))).toMatchObject({ code: "wrong_password" });
    const positions = await api.backupChallenge(PASSWORD);
    expect(positions).toHaveLength(3);
    expect([...positions].sort((a, b) => a - b)).toEqual(positions);
    expect(positions.every((p) => p >= 1 && p <= 12)).toBe(true);
    expect(new Set(positions).size).toBe(3);

    const answers = positions.map((p): [number, string] => [p, mnemonic[p - 1] ?? ""]);
    expect(await rejection(api.verifyBackup("nope nope", answers))).toMatchObject({ code: "wrong_password" });
    expect(await rejection(api.verifyBackup(PASSWORD, answers.slice(0, 2)))).toMatchObject({ code: "config" });
    expect(await rejection(api.verifyBackup(PASSWORD, [[13, "x"], ...answers]))).toMatchObject({ code: "config" });
    const twoWrong = answers.map(([p, w], i): [number, string] => [p, i === 0 ? w : "zzz"]);
    expect(await rejection(api.verifyBackup(PASSWORD, twoWrong))).toEqual({
      code: "backup_mismatch",
      message: `words ${positions[1]} and ${positions[2]} do not match your recovery phrase`,
    });
    expect((await api.appInfo()).backup_verified).toBe(false);
    await api.verifyBackup(PASSWORD, answers.map(([p, w]) => [p, ` ${w.toUpperCase()} `]));
    expect((await api.appInfo()).backup_verified).toBe(true);

    expect(await rejection(api.revealPhrase("nope nope"))).toMatchObject({ code: "wrong_password" });
    expect((await api.revealPhrase(PASSWORD)).mnemonic).toEqual(mnemonic);
  });

  it("starts a restored wallet verified", async () => {
    const api = createMockApi({ latencyMs: 0 });
    await api.restoreWallet(ABANDON, PASSWORD, null);
    expect((await api.appInfo()).backup_verified).toBe(true);
  });

  it("uses up a prepared payment on any confirm, and drops it on lock", async () => {
    const api = createMockApi({ latencyMs: 0 });
    await api.createWallet(12, PASSWORD);
    api.simulateIncoming(1_000_000);
    api.mineBlocks(1);
    await api.sync();
    const to = encodeSegwit("tb", 0, new Uint8Array(20).fill(7));

    const first = await api.prepareSend(to, 10_000, 2);
    api.setNodeOnline(false);
    expect(await rejection(api.confirmSend(first.id))).toMatchObject({ code: "rpc" });
    api.setNodeOnline(true);
    expect(await rejection(api.confirmSend(first.id))).toMatchObject({ code: "tx_build" });

    const second = await api.prepareSend(to, 10_000, 2);
    await api.lock();
    await api.unlock(PASSWORD);
    expect(await rejection(api.confirmSend(second.id))).toMatchObject({ code: "tx_build" });

    // tx_status answers from the last sync when the node is down.
    const third = await api.prepareSend(to, 10_000, 2);
    const { txid } = await api.confirmSend(third.id);
    api.setNodeOnline(false);
    api.mineBlocks(1);
    expect(await api.txStatus(txid)).toMatchObject({ state: "unconfirmed" });
    api.setNodeOnline(true);
    expect(await api.txStatus(txid)).toMatchObject({ state: "confirmed", confirmations: 1 });
  });

  it("keeps a separate wallet per network", async () => {
    const api = createMockApi({ latencyMs: 0 });
    await api.createWallet(12, PASSWORD);
    const settings = await api.getSettings();
    await api.setSettings({ ...settings, network: "regtest" });
    expect(await api.appInfo()).toMatchObject({ network: "regtest", wallet_exists: false });
    await api.createWallet(12, PASSWORD);
    expect((await api.newAddress()).address).toMatch(/^bcrt1q/);
    expect(await rejection(api.setSettings({ ...settings, network: "bitcoin" }))).toMatchObject({
      code: "mainnet_disabled",
    });
    await api.setSettings({ ...settings, network: "testnet4" });
    expect(await api.appInfo()).toMatchObject({ network: "testnet4", wallet_exists: true, unlocked: false });
  });
});
