# Presentation demo script (about 10 minutes)

Everything runs on **regtest**, a private local network where blocks confirm the moment you mine
them, so nothing depends on the internet or a faucet. Rehearse once before presenting.

## 0. Before you present (5 minutes, off-screen)

```bash
cd ~/Desktop/bitcoinwallet
cargo build -p btcw-cli && alias btcw=$PWD/target/debug/btcw
scripts/regtest.sh reset; scripts/regtest.sh start      # fresh node + funded "faucet" wallet
eval "$(scripts/regtest.sh env)"
export BTCW_DATADIR=~/btcw-presentation                  # separate from any other wallet
rm -rf ~/btcw-presentation
```

Open the GitHub repo in a browser tab: <https://github.com/luhrhenz/bitcoinwallet>
(README feature table, `docs/ARCHITECTURE.md` diagrams).

## 1. The problem and the architecture (1–2 min, slides or the repo)

- "A non-custodial wallet: the keys never leave my computer, and nobody can freeze or take the coins."
- Show `docs/ARCHITECTURE.md`: **one Rust core** (BDK + rust-bitcoin), **two frontends**
  (terminal + Tauri desktop), syncing from Bitcoin Core.
- "The wallet database holds only public keys; the recovery phrase is encrypted with Argon2id +
  XChaCha20-Poly1305; the private key is in memory only while signing."

## 2. Create and back up (2 min, terminal)

```bash
btcw create
```
- Point at: 12 words (BIP39), **shown once**, the warning, the first address `bcrt1q…`
  (BIP84, `m/84'/1'/0'/0/0`), the wallet birthday.
- "Any other command now reminds me to check my backup:"
```bash
btcw balance                 # ← note the reminder on the last line
btcw backup verify           # type the 3 words it asks for (hidden while typing)
btcw balance                 # reminder gone
```

## 3. Receive (2 min)

```bash
btcw address new                              # copy the address
scripts/regtest.sh fund <address> 0.5         # someone pays me
btcw sync && btcw balance                     # 0.5 BTC UNCONFIRMED (in the mempool)
scripts/regtest.sh mine 1
btcw sync && btcw balance                     # now CONFIRMED
btcw history                                  # 1 confirmation
btcw address list                             # address #0 is "used", so the next one is new
```
"It never reuses an address that has received money: that's a privacy rule."

## 4. Send with a PSBT (2 min)

```bash
FAUCET=$(scripts/regtest.sh cli -rpcwallet=faucet getnewaddress)
btcw send --to "$FAUCET" --amount 10000000
```
- Walk through the **preview**: full address in groups of 4, amount, fee, fee rate, size, change,
  total. "Nothing is signed yet: this is an unsigned PSBT."
- Answer `y`, **then** type the password: "the private key is decrypted only now, signs, and is
  wiped straight after."
```bash
btcw status <txid>                            # unconfirmed
scripts/regtest.sh mine 1
btcw status <txid> --watch --until 1          # confirmed in block N, 1 confirmation
btcw balance && btcw history
```

## 5. Desktop app on the same wallet (2 min)

```bash
cd apps/desktop && npm run tauri dev          # or the prebuilt target/debug/btcw-desktop
```
(Uses the same `BTCW_*` variables, so it opens **the same wallet** you just used in the terminal.)
- Dashboard: balances, recent transactions, the network badge.
- Receive: QR code. Send: unlock dialog → preview → confirm → the transaction screen counts
  confirmations (run `scripts/regtest.sh mine 1` in the terminal and watch it update).
- Settings → Show recovery phrase (password required) and auto-lock.

## 6. Restore (1 min, optional)

```bash
BTCW_DATADIR=~/btcw-restore btcw restore      # paste the 12 words: same balance and history
```

## 7. Close (30 s)

- PRD coverage: all 11 MVP features, plus encrypted seed, backup check, watch-only mode, desktop
  app and testnet4 support (also via a hosted HTTPS node).
- Quality: 134 Rust tests + 68 UI tests, run in CI against a real regtest node on two Bitcoin Core
  versions; reorg handling tested; the BIP84 official test vector passes.
- Built in phases with one agent per module and a review gate before every merge; the whole
  journey is written up in `WALKTHROUGH.md`.

## If something goes wrong on stage

| Symptom | Fix |
|---|---|
| `wallet_not_found` | `echo $BTCW_DATADIR`, you're in a new shell; re-run the `export` lines |
| `rpc … connection refused` | `scripts/regtest.sh start` then `eval "$(scripts/regtest.sh env)"` |
| `wallet is open in another btcw process` | close the desktop app (it and the CLI take turns on a wallet) |
| Balance still unconfirmed | `scripts/regtest.sh mine 1 && btcw sync` |
| Immature balance after `btcw mine` | normal: mined coins need 100 confirmations; use `scripts/regtest.sh fund` for spendable coins |
