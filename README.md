# btcw: a non-custodial Bitcoin wallet in Rust

Capstone project for the Rust for Bitcoin cohort. A BIP84 (native SegWit) wallet with a **desktop app**
(Tauri + React) and a **terminal app** (`btcw`), both built on one Rust core that uses
[BDK](https://bitcoindevkit.org) and syncs from your own Bitcoin Core node.

> ⚠️ **Status: under construction.** Runs on testnet4, signet and regtest. Mainnet is compiled out by default.

- Plan, algorithms and pseudocode: [`docs/PLAN.md`](docs/PLAN.md)
- How it was built, step by step: [`WALKTHROUGH.md`](WALKTHROUGH.md)

## Layout
```
crates/btcw-core   wallet engine: keys, encrypted keystore, sync, PSBT signing, SQLite persistence
crates/btcw-cli    `btcw` terminal app
apps/desktop       Tauri 2 + React desktop app
scripts/regtest.sh local regtest node for demos and tests
```

## Quick start (regtest)
```bash
scripts/regtest.sh start && eval "$(scripts/regtest.sh env)"
cargo run -p btcw-cli -- --help
```
Full usage docs arrive with Phase 3.

## Using a hosted node (e.g. Alchemy) instead of your own

Any Bitcoin Core-compatible JSON-RPC endpoint works, including `https://` providers whose API
key is part of the URL. Keep the key out of files and shell history where you can:

```bash
read -rs BTCW_RPC_URL && export BTCW_RPC_URL     # paste https://bitcoin-testnet4.g.alchemy.com/v2/<key>
btcw --network testnet4 create
btcw --network testnet4 sync
```

- `https://` URLs need no `--rpc-user`/`--rpc-cookie`; errors and logs show only the host, never
  the key.
- Use the provider's **testnet4** endpoint; testnet3 is refused.
- Each sync downloads new blocks since the last one plus new mempool transactions (cached in
  `<datadir>/<network>/mempool-cache.txt`), at roughly one request per item, so the first sync
  after a long break is the slow one.
- Restoring an old wallet scans from its birthday; with a hosted node pass `--birthday <height>`,
  since scanning all of testnet4 one block per request would take days.
