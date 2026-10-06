# btcw: a non-custodial Bitcoin wallet in Rust

Capstone project for the **Rust for Bitcoin cohort** (Project 1: Bitcoin Wallet).

btcw stores your keys on your own computer and builds, signs and broadcasts transactions without
trusting a third party. It comes as a **desktop app** (Tauri + React) and a **terminal app**
(`btcw`), both thin layers over one Rust core built on [BDK](https://bitcoindevkit.org) and
[rust-bitcoin](https://github.com/rust-bitcoin/rust-bitcoin). It syncs from a Bitcoin Core node,
either your own or a hosted endpoint.

> Runs on **testnet4** (default), **signet** and **regtest**. Mainnet is compiled out by default and
> needs both a build feature and an explicit opt-in. Testnet coins have no value.

- Architecture: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)

## Features

| Requirement | How btcw does it |
|---|---|
| Create a wallet | 12 or 24 BIP39 words from the OS random generator, shown once, encrypted at rest |
| Restore from a mnemonic | Phrase plus optional `--birthday` height; full rescan otherwise |
| BIP32/BIP84 derivation | `m/84'/1'/0'/0/*` (receive) and `/1/*` (change); checked against the official BIP84 test vector |
| New addresses, tracking used ones | Next *unused* address; never reuses one that has received funds |
| Sync with the chain | Block by block from Bitcoin Core RPC (`bdk_bitcoind_rpc`), reorg-safe, plus mempool |
| Confirmed and unconfirmed balance | Confirmed, unconfirmed and immature, as of the last synced block |
| Transaction history | Net amount, fee, status and confirmations |
| Sign via PSBT | rust-bitcoin `Psbt::sign` with the master key, finalized by BDK |
| Broadcast | `sendrawtransaction`, with Core's reason shown if rejected |
| Poll transaction status | `btcw status <txid> --watch`; live in the desktop app |
| Persist between runs | SQLite (BDK) plus an encrypted seed file |

Beyond the requirements:
- **Encrypted recovery phrase:** Argon2id + XChaCha20-Poly1305, bound to its network.
- **Backup check:** btcw reminds you until you prove your paper copy is right with 3 random words, and can show the phrase again with your password.
- **Watch-only by default:** balance and history need no password, and sending asks for it only after you've confirmed the preview.
- **Address book and labels:** pay a saved contact by name (the full address is always shown), and label transactions.
- **Speed up (RBF):** replace a stuck unconfirmed payment with one paying a higher fee (`btcw bump`, or "Speed up" in the app).
- **Desktop app with auto-lock:** the private key never reaches the UI, and the window is locked down with a strict CSP and no plugins.
- **Hosted nodes over HTTPS** (e.g. Alchemy), with the API key cut out of every message and log.

## Requirements

- Rust 1.90+ (`rustup`), and Node 20+ for the desktop app
- Bitcoin Core (`bitcoind`) for regtest, or a testnet4 RPC endpoint
- Desktop app on Linux: `sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev`

## Quick start: regtest in 2 minutes

```bash
git clone https://github.com/luhrhenz/bitcoinwallet && cd bitcoinwallet
cargo build -p btcw-cli
alias btcw=$PWD/target/debug/btcw

scripts/regtest.sh start                 # local node + a funded "faucet" wallet
eval "$(scripts/regtest.sh env)"         # point btcw at it

btcw create                              # shows your 12 words once
btcw backup verify                       # type 3 of them back
btcw address new                         # bcrt1q…
scripts/regtest.sh fund <address> 0.5    # the faucet pays you
btcw sync && btcw balance                # unconfirmed
btcw mine 1 && btcw sync && btcw balance # confirmed
btcw send --to <address> --amount 100000 # preview, confirm, then your password
btcw status <txid> --watch               # until it confirms (mine 1 in another terminal)
btcw history
scripts/regtest.sh stop
```

Every command takes `--json` for scripts; errors come back as `{"error":{"code","message"}}`
with exit code 1. Run `btcw --help` for everything.

## Desktop app

```bash
cd apps/desktop
npm ci
npm run tauri dev        # the real app (uses the same wallets and settings as the CLI)
npm run dev:mock         # the UI alone in a browser with a fake backend, for a quick look
npm run tauri build      # installable packages in target/release/bundle/ (.deb, AppImage)
```

The app and the CLI share wallet files (`~/.local/share/btcw/<network>/`), and you can use both
at once. Node settings are under Settings → Node, or come from the `BTCW_*` variables below.

## Using a hosted node (e.g. Alchemy) instead of your own

Any Bitcoin Core-compatible JSON-RPC endpoint works, including `https://` providers whose API
key is part of the URL. Keep the key out of files and shell history where you can:

```bash
read -rs BTCW_RPC_URL && export BTCW_RPC_URL     # paste https://bitcoin-testnet4.g.alchemy.com/v2/<key>
btcw --network testnet4 create
btcw --network testnet4 sync
```

- `https://` URLs need no `--rpc-user`/`--rpc-cookie`. Errors and logs show only the host, never
  the key.
- Use the provider's **testnet4** endpoint; testnet3 is refused.
- Each sync downloads new blocks plus new mempool transactions, cached in
  `<datadir>/<network>/mempool-cache.txt`. Each item is one request, so the first sync after a
  long break is the slow one.
- When restoring an old wallet through a hosted node, pass `--birthday <height>`. Scanning all of
  testnet4 one block per request would take days.

Free testnet4 coins: e.g. <https://mempool.space/testnet4/faucet>.

## Configuration

Precedence: command-line flag › environment variable › `<datadir>/btcw.toml` › default.

| Setting | Flag | Environment |
|---|---|---|
| Network (`testnet4`, `signet`, `regtest`) | `--network` | `BTCW_NETWORK` |
| Data directory | `--datadir` | `BTCW_DATADIR` |
| Node RPC URL | `--rpc-url` | `BTCW_RPC_URL` |
| Cookie file / user + password | `--rpc-cookie`, `--rpc-user`, `--rpc-pass` | `BTCW_RPC_COOKIE`, `BTCW_RPC_USER`, `BTCW_RPC_PASS` |
| Wallet password (**scripts and tests only**) | prompt | `BTCW_PASSWORD` |

## Layout

```
crates/btcw-core        wallet engine: keys, encrypted keystore, sync, PSBT send, SQLite persistence
crates/btcw-cli         `btcw` terminal app
apps/desktop            React UI (src/) + Tauri 2 Rust bridge (src-tauri/)
scripts/regtest.sh      local regtest node for demos and tests
docs/                   ARCHITECTURE.md
```

## Tests

```bash
BITCOIND_EXE=$(which bitcoind) cargo test --workspace   # unit + integration + regtest journeys
cd apps/desktop && npm test                             # UI tests (Vitest)
```

Node-backed tests start their own throwaway regtest `bitcoind`. If `BITCOIND_EXE` isn't set and
`bitcoind` isn't installed, they are skipped.

## Security notes

This is a learning project and has not been audited. Don't use it for real funds.
- The recovery phrase is the only real backup. Write it on paper and run `btcw backup verify`.
- The wallet database holds only public descriptors. The seed file is encrypted, and the private
  key is in memory only while you sign.
- `BTCW_PASSWORD` and `--rpc-pass` can leak through shell history and process listings. Prefer the
  prompts and cookie files.

## License

MIT
