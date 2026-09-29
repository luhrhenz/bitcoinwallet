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

## 2. Keys and keystore, Phase 1 / Agent A

**What we built:** `keys.rs` turns a 12- or 24-word phrase into the two *public* descriptors the wallet
stores plus a `Signer` that can sign PSBTs, and `keystore.rs` keeps the phrase encrypted on disk.
Nothing here touches the network or a node; it's pure key math and file I/O, tested offline.

### From words to addresses
Every key in the wallet comes from one phrase, through four standard steps:

```text
12 words ──BIP39 PBKDF2(passphrase)──▶ 64-byte seed ──BIP32──▶ master xprv (fingerprint 73c5da0a)
master ──m/84'/coin'/0'──▶ account xpub ──/0/i──▶ receive keys    ──P2WPKH──▶ bc1q… / tb1q… / bcrt1q…
                                        ──/1/i──▶ change keys
```

1. **Mnemonic (BIP39).** 128 bits of randomness (256 for 24 words) plus a short checksum, written as words
   from a fixed list of 2048. We take the randomness straight from the OS with `getrandom`, not from a
   userspace PRNG, and wipe the buffer afterwards:
   ```rust
   let mut entropy = Zeroizing::new([0u8; 32]);
   let entropy = &mut entropy[..words.entropy_bytes()];   // 16 or 32 bytes
   getrandom::fill(entropy)?;
   let mnemonic = bip39::Mnemonic::from_entropy(entropy)?;
   ```
   When a user *types* a phrase, `parse_mnemonic` trims it, collapses whitespace and lowercases it,
   then checks every word against the list and verifies the checksum. The error says *where* the
   problem is ("word 5 is not in the BIP39 English word list", "checksum mismatch") but never repeats
   the words, because error messages end up in terminals, logs and bug reports.
2. **Seed.** PBKDF2-HMAC-SHA512 over the words with 2048 rounds. The optional passphrase (the "25th word")
   goes in here, so the same words with a different passphrase are a completely different wallet.
   The 64-byte seed lives in a `Zeroizing` buffer and is wiped as soon as derivation finishes.
3. **Master key (BIP32).** `Xpriv::new_master(network, &seed)`. Its **fingerprint** (the first 4 bytes of
   the hash of its public key) is a short, non-secret name for "this seed".
4. **Account (BIP84).** Derive `m/84'/coin'/account'`: `84'` says "native SegWit, one key per
   address", `coin'` is `0'` on mainnet and `1'` on every test network (`config::coin_type`), and `'` means
   *hardened*. Without hardening, one leaked child private key plus the parent's public key reveals the
   parent private key; hardening the account level stops such a leak at the account.

### Descriptors and the key origin
BDK doesn't want a list of keys, it wants **descriptors**: strings that say exactly which scripts the
wallet owns. We build them from typed miniscript values rather than `format!`, so the syntax and the
trailing checksum come from the library:
```rust
let key = DescriptorPublicKey::XPub(DescriptorXKey {
    origin: Some((master_fingerprint, account_path)),   // the [73c5da0a/84'/1'/0'] part
    xkey: account_xpub,
    derivation_path: DerivationPath::from(vec![ChildNumber::Normal { index: chain }]),  // /0 or /1
    wildcard: Wildcard::Unhardened,                      // the /* part
});
Descriptor::new_wpkh(key)?.to_string()   // Display appends #checksum
```
which gives, for testnet4:
```text
wpkh([73c5da0a/84'/1'/0']tpubDC8msFGeGuwnKG9Upg7DM2b4DaRqg3CUZa5g8v2SRQ6K4NSkxUgd7HsL2XVWbVm39yBA4LAxysQAm397zwQSQoQgewGiYZqrA9DsP4zbQ1M/0/*)#2ag6nxcd
```
The `[fingerprint/path]` prefix is the **key origin**. It looks like decoration, but signing depends
on it: when BDK builds a PSBT it copies it into every input's `bip32_derivation` as "this input needs the
key at `73c5da0a / 84'/1'/0'/0/3`". The signer checks that the fingerprint is its own and derives exactly
that path. Without the origin, the PSBT would only say "key under this xpub", and a signer holding the
master key couldn't tell which of its keys that is.

### Why the wallet never sees the private key
The descriptors hold only an `xpub` (or `tpub`), so the SQLite wallet file can show addresses, balances and
history but can never spend. That's why watch-only mode needs no password. Spending needs a
`Signer`, which holds the master `Xpriv` in memory only while the wallet is unlocked:
```rust
pub fn sign_psbt(&self, psbt: &mut Psbt) -> Result<usize> {
    match psbt.sign(&self.master, &Secp256k1::new()) {
        Ok(used) => Ok(used.values().filter(|keys| signed_any(keys)).count()),
        Err((_, errors)) => Err(WalletError::Sign(/* "input 0: missing spend utxo", no key bytes */)),
    }
}
```
One detail caught during testing: `Psbt::sign` returns an entry for *every* input, with an empty key list
when none of its keys matched, so we count the non-empty entries. A signer from another seed therefore
reports `0`, and the caller (`tx.rs`) can say "wrong wallet" instead of passing on an unsignable
transaction. The `Signer` wipes its key on `Drop`, and its `Debug` prints `Signer(<redacted>)`.
`Mnemonic`'s prints `Mnemonic(<12 words redacted>)`.

### The keystore: the phrase, encrypted at rest
BDK persists public descriptors only, so to be able to spend again after a restart we have to keep the phrase
somewhere. `seed.enc` is a small JSON file:
```json
{ "version": 1, "network": "regtest",
  "kdf":    { "alg": "argon2id", "m_kib": 65536, "t": 3, "p": 1, "salt": "…16 bytes…" },
  "cipher": { "alg": "xchacha20poly1305", "nonce": "…24 bytes…", "ciphertext": "…" } }
```

| Choice | Why |
|---|---|
| **Argon2id**, 64 MiB, 3 passes | Turns the password into a 256-bit key. It's *memory-hard*: each guess needs 64 MiB of RAM, which makes GPU/ASIC brute force of a stolen file expensive. It costs us about half a second per unlock. A fresh random salt per file means precomputed tables are useless. |
| **XChaCha20-Poly1305** (an AEAD) | Encrypts *and* authenticates. A wrong password or any flipped bit fails the Poly1305 tag, so we get a clean `WrongPassword` instead of decrypting to garbage words. The 24-byte nonce is big enough to pick at random with no risk of reuse. |
| **Network as AAD** (`"btcw-keystore-v1\|regtest"`) | The "associated data" is authenticated but not encrypted. The plaintext `network` field gives a friendly `NetworkMismatch` error, but the AAD is what enforces it: edit the field to `testnet4` and decryption fails, so a keystore can't be relabelled onto another network. |
| **KDF bounds on load** | The cost parameters are read from the file, which is untrusted input. We refuse `m_kib` > 1 GiB, `t` > 10, `p` > 16 *before* running Argon2, so a crafted file can't make `load` allocate terabytes or spin for hours. |
| **Atomic, 0600 writes** | The file is written to a temp file in the same directory, opened with `create_new` and mode `0600` from the first byte, `sync_all`ed, then `rename`d into place (and the directory fsynced). A crash leaves either no keystore or a complete one, never half a file, and other users on the machine can't read it even briefly. An existing keystore is never overwritten (`WalletExists`). |

The derived key and the decrypted phrase sit in `Zeroizing` buffers, so they're wiped as soon as they go out of scope.

### Proof: the BIP84 test vector
BIP84 publishes the addresses for `abandon abandon … about` with no passphrase. Our derivation, fed into a
plain BDK wallet, must reproduce them exactly:
```rust
let (descriptors, signer) = keys::derive_account(&abandon, "", Network::Bitcoin, 0)?;
assert_eq!(signer.fingerprint().to_string(), "73c5da0a");
let wallet = Wallet::create(ext, int).network(Network::Bitcoin).create_wallet_no_persist()?;
assert_eq!(wallet.peek_address(External, 0).address.to_string(), "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu");
assert_eq!(wallet.peek_address(Internal, 0).address.to_string(), "bc1q8c6fshw2dlwun7ekn9qwf37cu2rn755upcp6el");
```
It passes, and so does the account xpub from the BIP (`zpub6rFR7y4Q…` re-encoded as `xpub6CatWdiZ…`). This runs
with mainnet *compiled out*, because derivation is pure math. Only opening a mainnet wallet is gated (§1).
The same test checks that testnet4, signet and regtest use coin type `1'`, `tpub` keys and `tb1q…`/`bcrt1q…` addresses.

The signing test needs no node either: it builds a regtest wallet from our public descriptors, hands it a
made-up unconfirmed transaction paying 100 000 sat to its first address, builds a real PSBT with BDK,
signs it with our `Signer`, and checks that BDK can finalize it. A signer from a different phrase signs 0 inputs.

### Try it
```bash
cargo test -p btcw-core --test keys       # BIP39/BIP84 vectors, parsing, PSBT signing (offline)
cargo test -p btcw-core --test keystore   # round trip, wrong password, tampering, AAD, 0600, KDF bounds
```

## 3. Chain backend, Phase 1 / Agent B — _pending_
## 4. Wallet service, Phase 1 / Agent C — _pending_
## 5. Terminal app (CLI), Phase 1 / Agent D — _pending_
## 6. Desktop UI, Phase 1 / Agent E — _pending_
## 7. Sending and status, Phase 2 / Agent F — _pending_
## 8. Tauri bridge, Phase 2 / Agent G — _pending_
## 9. End-to-end tests, Phase 2 / Agent H — _pending_
## 10. Running the demo (regtest and testnet4) — _pending_
## 11. Lessons learned — _pending_
