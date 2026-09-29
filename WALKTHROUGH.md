# Walkthrough: How We Built the Bitcoin Wallet

This is a running log of what we built, how we built it and why, written so someone who knows Rust
but is new to Bitcoin can follow it. A new section gets added after each phase is reviewed and merged.
The full plan (requirements, algorithms, pseudocode, work split) is in [`docs/PLAN.md`](docs/PLAN.md).

## 0. Thought process

1. **Read the PRD and turn it into a checklist.** There are 11 functional requirements (R1–R11) and 5 deliverables.
   Every piece of code maps back to one of them.
2. **Don't reinvent consensus-critical code.** Coin selection, script building and signing are easy to get
   subtly wrong, so we use `bdk_wallet` for them. We write the parts that make the product work: key handling,
   secure storage, syncing against our own node, the send flow, status tracking and the CLI.
3. **Stay non-custodial and local.** Keys never leave the machine, and chain data comes from our own `bitcoind`, not a third-party API.
4. **Fix interfaces first, then parallelize.** Module signatures are set before any agent starts, so the
   agents can build keys, chain, wallet and CLI at the same time without blocking each other.
5. **One core, two frontends.** All wallet logic lives in `btcw-core`. The terminal app and the Tauri desktop
   app are both thin layers over it, so they behave the same and share the same wallet files.
6. **Testnet now, mainnet-ready.** The network is one config value. Mainnet is locked behind a build feature
   and an explicit opt-in until the code has been reviewed properly.
7. **Test against a real node.** Regtest lets us mine blocks on demand, so tests can cover the whole cycle:
   receive, confirm, send, confirm again.

## 1. Scaffold, Phase 0 ✅

**What we built:** the skeleton every agent builds on. That's a Cargo workspace (`btcw-core` library +
`btcw-cli` binary), a React/Vite frontend in `apps/desktop`, a regtest helper script and CI. Every module
already exists with its final function signatures and `todo!()` bodies, so agents fill in bodies and never
argue about interfaces.

### Picking versions that agree
The Rust Bitcoin ecosystem has one classic trap: `bdk_wallet`, `bdk_bitcoind_rpc` and your own code must all
use the *same* `bitcoin` crate version, or types like `Transaction` from one crate won't be accepted by another.
We checked with `cargo tree -i bitcoin`. Everything resolves to `bitcoin 0.32`, and our code only ever
imports it as `btcw_core::bitcoin` (a re-export of `bdk_wallet::bitcoin`), so it can't drift.

### Network policy (`config.rs`)
The network is one value that decides everything network-specific:

| | testnet4 | signet | regtest | mainnet |
|---|---|---|---|---|
| address prefix | `tb1q…` | `tb1q…` | `bcrt1q…` | `bc1q…` |
| BIP84 coin type | `1'` | `1'` | `1'` | `0'` |
| Core RPC port | 48332 | 38332 | 18443 | 8332 |
| allowed? | ✅ default | ✅ | ✅ | 🔒 `--features mainnet` **and** opt-in |

Each network gets its own folder (`~/.local/share/btcw/<network>/`), so a testnet wallet can never be
opened as a mainnet one.

### De-risking before splitting the work
Before handing anything to agents, `tests/phase0_smoke.rs` spins up a real regtest node and runs the
whole money path with raw BDK: receive → unconfirmed balance → mine → confirmed → build PSBT → sign →
broadcast → confirm. It surfaced three things that changed the design:

1. **BDK 3.2 deprecated `Wallet::sign`.** Newer BDK wants the wallet to hold *no private keys*. So:
   - the wallet database stores only **public** descriptors (`wpkh([fingerprint/84'/1'/0']tpub…/0/*)`)
   - a separate `Signer` holds the master private key in memory only while the wallet is unlocked
   - signing = rust-bitcoin's `psbt.sign(&master_xprv)` (it finds the right child key using the
     fingerprint + derivation path BDK wrote into each PSBT input), then BDK's `finalize_psbt` builds the witnesses.

   Bonus: balance and history now work **without a password** (watch-only mode).
2. **Wallet birthday.** A brand-new wallet can't have old transactions, so we remember the block height
   at creation and start syncing there instead of scanning the whole chain.
3. **RPC timeouts.** Mining 101 regtest blocks takes ~20 s on Core v31, but `bitcoincore-rpc` gives up after
   15 s. The error ("Resource temporarily unavailable (os error 11)") was really a socket read timeout. The fix is to
   mine in batches in tests and use a longer timeout in the real client.

### Try it
```bash
cargo test --workspace                       # unit tests + regtest smoke test (needs BITCOIND_EXE or bitcoind on PATH)
scripts/regtest.sh start                     # local node with a funded faucet wallet
cargo run -p btcw-cli -- --help              # the full command surface (bodies not implemented yet)
cd apps/desktop && npm run build             # frontend compiles
```

## 2. Keys and keystore, Phase 1 / Agent A — _pending_
## 3. Chain backend, Phase 1 / Agent B — _pending_
## 4. Wallet service, Phase 1 / Agent C — _pending_
## 5. Terminal app (CLI), Phase 1 / Agent D — _pending_
## 6. Desktop UI, Phase 1 / Agent E — _pending_
## 7. Sending and status, Phase 2 / Agent F — _pending_
## 8. Tauri bridge, Phase 2 / Agent G — _pending_
## 9. End-to-end tests, Phase 2 / Agent H — _pending_
## 10. Running the demo (regtest and testnet4) — _pending_
## 11. Lessons learned — _pending_
