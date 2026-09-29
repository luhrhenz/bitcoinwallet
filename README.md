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
