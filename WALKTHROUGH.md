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

## 3. Chain backend, Phase 1 / Agent B

**What we built:** `chain.rs` is the only module that talks to the outside world. A `Node` connects to
our own Bitcoin Core (`bitcoind`) over JSON-RPC and does five things: checks it's on the right network,
reports the tip height, **syncs** a `WalletService` block by block (plus the mempool), broadcasts signed
transactions, and estimates fees. On regtest it can also mine blocks for demos and tests.

### What Bitcoin Core's RPC gives us
`bitcoind` keeps a full copy of the chain and validates every block itself, so asking it is as
trustworthy as it gets: no third-party server learns our addresses. Its RPC is JSON over HTTP on a local
port (`18443` regtest, `48332` testnet4, …). We use a handful of calls:

| RPC | Used for |
|---|---|
| `getblockchaininfo` | which chain the node is on, its height, whether it's still syncing or pruned |
| `getblockcount` | `tip_height()` |
| `getblock`, `getblockhash`, `getrawmempool`, `getrawtransaction` | sync (called for us by BDK's `Emitter`) |
| `sendrawtransaction` | broadcast |
| `estimatesmartfee` | fee estimates |
| `generatetoaddress` | `mine()`, regtest only |

`bitcoincore-rpc` 0.19 has typed helpers for all of these, but it predates Core v28 and some of its
result structs no longer match what Core v31 sends. Where we read the response ourselves, we make a raw
`client.call::<serde_json::Value>(..)` and pick out the fields we need.

### Connecting: cookie auth, a 120 s timeout, and checking the chain
Bitcoin Core protects its RPC with a password. The easy and safe way is the **cookie**: every time
bitcoind starts it writes a fresh random `__cookie__:<password>` line to `<datadir>/<network>/.cookie`,
readable only by the user running it. Whoever can read that file is trusted, so there's no password to
configure or leak. We read it ourselves (split on the first `:`), keep it in a `Zeroizing` buffer, and
never put it in an error. Explicit `--rpc-user`/`--rpc-pass` also work.

The client is built by hand instead of with `Client::new`, for one reason: **timeouts**. Phase 0 found
that `bitcoincore-rpc`'s HTTP transport gives up after 15 s, while Core v31 mines regtest blocks at about
200 ms each. A 101-block `generatetoaddress` failed with a baffling "Resource temporarily unavailable
(os error 11)", which turned out to be the socket read timeout. A large block on a slow disk can take a while too.
So we build the transport ourselves with 120 s:

```rust
let builder = simple_http::Builder::new()
    .timeout(RPC_TIMEOUT)                      // 120 s, not the default 15 s
    .url(&rpc.url)?
    .auth(user.as_str(), Some(pass.as_str()));
let client = Client::from_jsonrpc(jsonrpc::Client::with_transport(builder.build()));
```

`mine()` still batches (25 blocks ≈ 5 s per call), so no single request gets anywhere near the limit.

Then `connect` asks `getblockchaininfo` which chain the node is on (`"main"`, `"test"`, `"testnet4"`,
`"signet"`, `"regtest"`) and refuses a mismatch. This is a safety check, not a formality: a wallet
syncing against the wrong chain would show a history that isn't real, its birthday height would point
at an unrelated block, and a broadcast would go to a network the user didn't choose. So a testnet4
wallet on a regtest node gets
`NetworkMismatch { expected: testnet4, found: "regtest" }` before anything else happens. If the node
reports `initialblockdownload: true` we log a warning: sync works, but only up to the node's current height.

Connection errors say what to check, with the URL (any `user:pass@` in it is stripped first):

| Problem | Message (after `bitcoin node RPC error: `) |
|---|---|
| nothing listening | `getblockchaininfo: cannot connect to bitcoind at http://127.0.0.1:18443 (connection refused); is bitcoind running for regtest? check --rpc-url` |
| no cookie file | `cannot read the RPC cookie file /home/me/.bitcoin/testnet4/.cookie: No such file or directory (os error 2); is bitcoind running for testnet4? set --rpc-cookie (or --rpc-user and --rpc-pass)` |
| wrong password | `… rejected the RPC credentials (HTTP 401); check --rpc-user and --rpc-pass, or --rpc-cookie (bitcoind writes a new cookie each time it starts)` |
| too slow | `… did not answer within 120 s` |

### Sync: walking the chain block by block
The wallet's job during sync is to find every transaction that pays to, or spends from, one of its
scripts. BDK's `bdk_bitcoind_rpc::Emitter` feeds it whole blocks from our node, one at a time:

```rust
let mut emitter = Emitter::new(
    &self.client,
    wallet.bdk().latest_checkpoint(),   // where we stopped last time
    wallet.birthday_height(),           // where a brand-new wallet starts
    unconfirmed_txs(wallet),            // our mempool txs, so evictions can be reported
);
while let Some(event) = emitter.next_block()? {
    let height = event.block_height();
    wallet.bdk_mut().apply_block_connected_to(&event.block, height, event.connected_to())?;
    on_progress(SyncProgress { height, tip_height });
    if blocks_scanned.is_multiple_of(100) { wallet.persist()?; }   // crash-safe progress
}
```

`apply_block_connected_to` does two things: it keeps the transactions in the block that touch our
scripts (anchored to that block), and it adds the block to the wallet's **local chain**.

**Checkpoints.** The local chain (BDK's `LocalChain`, see the table in §4) is a list of
`(height, block hash)` checkpoints, and its tip is "how far we've synced". It's persisted, so the next
sync starts from there. The Emitter begins by walking our checkpoints *backwards* and asking the node
about each one, until it finds one that's still on the node's best chain: the **agreement point**.
Normally that's simply our tip, and it continues with the next block.

**The birthday.** When the agreement point is below `start_height` (a new wallet has only the
genesis checkpoint), the Emitter jumps straight to `start_height` and emits *that* block. So a
wallet created at height 871 234 downloads block 871 234 onwards and never touches the 871 233 before
it. After the first sync our checkpoint is above the birthday, so the birthday stops mattering. The
test proves both sides: a wallet whose birthday is the tip scans exactly **1** block, while a restore
(birthday 0) of the same seed scans every block from 1 to the tip and ends up with the same balance.

**Persisting every 100 blocks.** BDK only writes on `persist()`. The first sync of a restored testnet4
wallet can run for a long time, so we save every 100 blocks: if it's interrupted, the next run resumes
from the last save rather than from the birthday.

### Reorgs, and how `connected_to` handles them
Sometimes two miners find a block at the same height. The network briefly disagrees, then follows
whichever branch gets more work on top, and the losing block is **reorged out**. Its transactions go
back to the mempool. A wallet that remembered "my payment is confirmed in block 102" now has to forget it.

This is what `connected_to` is for. Each emitted block says which earlier block it builds on: the
previous emitted block, or after a reorg the agreement point. `LocalChain` compares that with its own
checkpoints. When the new block has the same height as one of ours but a different hash, ours is stale:
BDK drops it and every checkpoint above it. A transaction is only "confirmed" if its anchor block is in
the local chain, so a transaction anchored in a dropped block goes back to being unconfirmed, with no
extra code on our side.

The regtest test proves it, step by step:

```text
fund 1 000 000 sat → sync: unconfirmed          balance 0 / 1 000 000
mine 1 (block 102) → sync: confirmed, 1 conf    balance 1 000 000 / 0
invalidateblock 102          node tip is 101 again; Core puts our tx back in its mempool
generateblock faucet []      a competing, *empty* block 102'
sync → 1 block scanned (102', connected to 101)  tx unconfirmed again: balance 0 / 1 000 000
mine 1 (block 103) → sync: confirmed at 103, 1 conf
```

One subtlety came out of this test. Right after `invalidateblock`, before 102' exists, the node's chain
is simply *shorter* than ours. The Emitter only emits blocks that connect, so there's nothing to apply,
and BDK has no public way to drop checkpoints without a replacing block. So the wallet keeps its
height-102 tip until the node has *some* block at that height again, and `sync` logs a warning. On a real
network a reorg always comes with the competing block, so this only shows up with manual `invalidateblock`
or when you switch to a node that is still catching up.

### The mempool and evictions
After the last block, `emitter.mempool()` returns every transaction in the node's mempool (unconfirmed,
waiting to be mined) with the current time as "last seen". `apply_unconfirmed_txs` keeps the ones that
touch our scripts. That's how an incoming payment appears as *unconfirmed* within seconds of being sent.

A mempool transaction can also disappear without being mined: replaced by a higher-fee version (RBF),
expired after two weeks, or kicked out when the mempool is full. That's an **eviction**. The Emitter
can only spot one if it knows what was there before, so we seed it with the wallet's currently
unconfirmed transactions. Any of those that are neither in a new block nor in the mempool anymore come
back in `mempool.evicted`, and `apply_evicted_txs` stops counting them in the balance. (The Emitter only
reports evictions once it has reached the node's tip, because before that it can't tell "evicted" from
"confirmed in a block we haven't fetched yet".) `SyncReport::mempool_txs` is the number of the wallet's
transactions still unconfirmed afterwards.

### Pruned nodes
A node started with `-prune=N` deletes old blocks once it has validated them, keeping only the most
recent ones. That's fine for a wallet *if* the blocks it still needs are there. If they're not, BDK would
fail deep inside `getblock` with "Block not available (pruned data)". So `sync` checks first:

```rust
// getblockchaininfo: "pruned": true, "pruneheight": 4000 (the lowest block still stored)
if first_needed < prune_height {
    "node pruned blocks below 4000; the wallet needs blocks from 1 — use a non-pruned node or a later --birthday"
}
```

`first_needed` is the block after our checkpoint, or the birthday for a new wallet. A new wallet on a
pruned testnet4 node works fine, because its birthday is the tip. Restoring an old seed needs either a
full node or a birthday the user is sure is early enough.

### Fee estimates: three units and a rounding rule
Fees are paid per unit of transaction *size*, and three units are involved:

| Unit | Who uses it | 1 sat/vB is… |
|---|---|---|
| **BTC/kvB** (BTC per 1000 virtual bytes) | Core's `estimatesmartfee` answer | 0.00001 |
| **sat/vB** | what people and block explorers quote | 1 |
| **sat/kwu** (sat per 1000 weight units) | BDK's `FeeRate` internally | 250 |

SegWit measures size in *weight*: witness bytes (signatures) count 1 weight unit each, everything else 4.
A *virtual byte* is 4 weight units. So:

```text
sat/kwu = BTC/kvB × 100 000 000 (sat per BTC) ÷ 4 (wu per vB)
```

We round **up**, and never go below 1 sat/vB (250 sat/kwu), the default minimum relay fee. Rounding down
could undercut the node's estimate, or drop below the relay minimum and get the transaction rejected:
0.00001001 BTC/kvB is 250.25 sat/kwu, and 250 would be *below* what was asked. Float input
needs care too: `0.29 × 1e8` is `28999999.999999996` in `f64`, and truncating would lose a satoshi. Core
prints amounts with exactly 8 decimals, so we `round()` to whole sat/kvB first and only then divide by
4, rounding up. Unit tests check these cases without a node.

When Core has no estimate (it answers `{"errors": ["Insufficient data or no feerate found"]}`), we fall
back to `FALLBACK_FEE_RATE` = 2 sat/vB and log a warning. Core estimates from how long past transactions
took to confirm, so a fresh regtest chain, with no fee-paying history at all, *always* takes this path.
A fresh testnet4 node can too.

### Broadcast: Core's reason is the message
`broadcast` is `sendrawtransaction`. Core checks the transaction fully (signatures, inputs exist and
are unspent, fee policy) and either accepts it into its mempool, returning the txid, or rejects it with
a reason. We pass that reason through, because it's the only useful part. From the regtest test,
broadcasting a second payment that spends the same coin:

```text
broadcasting 1e78…9bdb: insufficient fee, rejecting replacement 1e78…9bdb, not enough additional fees to relay; 0.00 < 0.00000015 (code -26)
```

That's *replace-by-fee* at work: since Core 28 a conflicting transaction can replace a mempool one, but only
by paying more. Once the first payment is mined, the same broadcast fails with
`bad-txns-inputs-missingorspent (code -25)`, because the coin no longer exists.

### Try it
```bash
cargo test -p btcw-core --lib chain            # fee conversion, cookie parsing, pruning check (offline)
                                               # + broadcast/double-spend on regtest
BITCOIND_EXE=~/.local/bin/bitcoind cargo test -p btcw-core --test chain
    # connect errors, network mismatch, user/pass auth, mining in batches,
    # receive → confirm → reorg → re-confirm, birthday vs full rescan, resume after reopen
```

## 4. Wallet service, Phase 1 / Agent C

**What we built:** `wallet.rs` wraps a BDK `Wallet` in a `WalletService`. It creates or opens the wallet's
SQLite file, makes sure only one process uses it at a time, and turns BDK's internal state into the plain
views the CLI and the UI show: addresses, balance, history and UTXOs. It never talks to a node. Syncing is
`chain.rs`'s job (Agent B), which feeds blocks into the same wallet through `bdk_mut()`.

### What BDK's `Wallet` keeps track of
A BDK wallet is four pieces of state, all derived from the two public descriptors from §2:

| Piece | What it is | Why it matters |
|---|---|---|
| **Keychains** | `External` (receive, `…/0/*`) and `Internal` (change, `…/1/*`) | Change goes back to an address that's never shown to anyone. |
| **SPK index** (`KeychainTxOutIndex`) | Every script pubkey (spk, the "locking script" an output pays to) derived so far, by keychain and index, plus which ones have been paid | This is how the wallet recognises its own outputs in a block. |
| **Tx graph** | Every transaction relevant to us, with *anchors* (the block it confirmed in) and *first/last seen* times for mempool txs | History, fees and balance are all computed from it. |
| **Local chain** | A list of checkpoints (height + block hash) up to the last synced block | It tells us which anchors are still on the best chain after a reorg, and its tip is our idea of "now". |

The spk index derives a few addresses *beyond* the last one handed out: the **lookahead**. We use
`LOOKAHEAD = 25` (a bit above the BIP44 gap limit of 20). When you restore from a phrase, the wallet has
no idea which addresses you used before. It scans blocks for the revealed addresses *plus the next 25*,
and whenever it finds a payment to one of them it reveals up to that index and looks 25 further. That's
how a restore finds every coin, as long as nobody skipped more than 25 addresses in a row. The lookahead
isn't stored in the database, so `create` and `open` must both pass the same value.

### Balance is a sum of unspent outputs
Bitcoin has no accounts. A transaction *spends* earlier outputs completely and creates new ones; if you
spend 0.3 BTC from a 1 BTC output, the transaction has a 0.3 output to the recipient and a ~0.7 output back
to your change address. So "my balance" is just the sum of outputs paying to my scripts that no
transaction has spent yet: the **UTXO set**. BDK groups that sum by how safe it is:

```rust
let b = self.wallet.balance();
BalanceView {
    confirmed_sat:   b.confirmed.to_sat(),                       // in a block on our chain
    unconfirmed_sat: b.trusted_pending + b.untrusted_pending,    // in the mempool
    immature_sat:    b.immature.to_sat(),                        // coinbase, < 100 confirmations
    total_sat:       /* sum of the three, saturating */,
}
```

- **Trusted pending** is unconfirmed money from a transaction *we* made (our own change). Nobody else can
  double-spend it, so it's safe to count on before it confirms.
- **Untrusted pending** is unconfirmed money someone sent us. The sender could still replace the
  transaction (RBF) or double-spend it, so it isn't really ours until it's mined.
- **Immature** is a mining reward. Consensus says coinbase outputs can't be spent for 100 blocks (in case
  the block is orphaned), which is why regtest demos mine 101 blocks.

The UI shows trusted and untrusted together as "unconfirmed"; the distinction matters for coin selection
(Agent F), not for display.

### Confirmations come from our own tip
A confirmed transaction's *anchor* says which block it's in. Confirmations count that block and every
block on top of it:

```rust
ChainPosition::Confirmed { anchor, .. } => TxStatus::Confirmed {
    height: anchor.block_id.height,
    confirmations: tip.saturating_sub(height).saturating_add(1),   // tip = synced_height()
    block_time: anchor.confirmation_time,
},
ChainPosition::Unconfirmed { first_seen, .. } => TxStatus::Unconfirmed { first_seen: *first_seen },
```

`tip` is the wallet's latest checkpoint, *not* the node's current height. That's deliberate: history works
offline, and the count can only be too low (if we haven't synced lately), never too high. Mine 2 blocks
without syncing and the wallet still says 1 confirmation; after a sync it says 3. The regtest test checks
1 after the first block, then 3 after mining two more and syncing, and again after reopening the file.

History rows also carry `sent`/`received` (from BDK's `sent_and_received`: our inputs vs our outputs),
`net = received − sent` as an `i64` computed through `i128` with no `as` casts, and the fee. The fee
(inputs − outputs) is only known when the wallet has seen every input's previous output, which is true for
transactions we sent and false for incoming ones, so `fee_sat` is `None` for those. Rows are sorted
unconfirmed-first (newest first), then by height descending, then by txid, so two runs over the same data
print the same order.

### Handing out addresses: `next_unused`, not `reveal_next`
Reusing an address is bad for privacy: everyone who paid you, and everyone watching the chain, can link
all those payments together. So each payer should get a fresh address. But "fresh" has a trap: if you
press *Receive* 50 times without getting paid, you've revealed 50 unused addresses, and a restore with a
gap limit of 25 might stop scanning before it reaches a payment to address 49.

BDK has two calls for this:
- `reveal_next_address` always moves to a new index.
- `next_unused_address` returns the **lowest revealed address that has never been paid**, and only reveals
  a new one when all of them are used.

`new_address()` uses the second one. It's the same address until someone pays it, which keeps the
revealed-but-unused gap at one and makes restores reliable:

```rust
pub fn new_address(&mut self) -> Result<AddressRow> {
    let info = self.wallet.next_unused_address(KeychainKind::External);
    self.persist()?;   // the revealed index must survive a restart (BDK's docs say so too)
    Ok(AddressRow { index: info.index, address: info.address.to_string(), keychain: Keychain::External, used: false })
}
```

`addresses()` lists every revealed receive spk with a `used` flag straight from the spk index
(`is_used(External, i)`), which turns true as soon as any transaction output pays to it, confirmed or not.

### Persistence: staged changes and `persist`
BDK never writes to disk on its own. Every change (a revealed address, an applied block, a new mempool
transaction) is added to an in-memory **staged changeset**. `persist()` writes that changeset to SQLite in
one database transaction and clears it only if the write succeeded:

```rust
pub fn persist(&mut self) -> Result<()> {
    self.wallet.persist(&mut self.db).map(|_changed| ()).map_err(|e| persist_err("saving wallet changes", e))
}
```

If the process dies before `persist`, the staged changes are lost, but the file is still a consistent
older state: sync just re-downloads those blocks next time. On `open`, BDK reads all its tables back
into a changeset and rebuilds the wallet from it, so balance, history and confirmations are available
straight away with no node.

`open` checks that the database belongs to this network (`check_network`), and when unlocking with a
password (`expected` descriptors) that it belongs to this seed. Each failure becomes its own error:

| What happened | Error |
|---|---|
| No `wallet.sqlite` | `WalletNotFound` (and nothing is created: no file, no directory) |
| File exists but has no wallet in it | `WalletNotFound` |
| A regtest database copied into the signet folder | `NetworkMismatch { expected: signet, found: "regtest" }` |
| Seed from the keystore doesn't match the stored descriptors | `Persist("wallet database does not belong to this seed …")` |
| Not a SQLite file / unreadable | `Persist(..)` with the path |

When `create` fails after SQLite has already created the file (say, an invalid descriptor), it deletes the
partial file, so the next attempt doesn't hit `WalletExists`.

### The birthday
A new wallet can't have transactions in blocks mined before it existed. `create` stores the tip height at
creation in our own table, next to BDK's, in the same file:

```sql
CREATE TABLE IF NOT EXISTS btcw_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO btcw_meta (key, value) VALUES ('birthday_height', '871234');
```

`Node::sync` starts the block scan there instead of at genesis. On testnet4 that's the difference between
seconds and hours. A restore doesn't know when the phrase was first used, so its birthday is 0. The row is
written *after* BDK's tables on purpose: if we crash in between, the birthday reads as 0, which means a
slower sync but never missed coins.

### One process at a time: the lock file
The CLI and the desktop app share the same datadir (§4.2 of the plan). If both had the wallet open, each
would hold its own in-memory copy and the second `persist` would overwrite the first. So `create` and
`open` take an exclusive OS lock on `<datadir>/<network>/wallet.lock` *before* touching SQLite, and keep
the `File` in the service so the lock lives exactly as long as the wallet:

```rust
match file.try_lock() {                       // std::fs::File::try_lock, flock(2) on Linux
    Ok(()) => Ok(file),
    Err(TryLockError::WouldBlock) => Err(WalletError::WalletInUse),
    Err(TryLockError::Error(e)) => Err(io_err("locking", &path, e)),
}
```

The OS releases the lock if the process crashes, so there's no stale lock to clean up, and the file is
never deleted (deleting a lock file while someone waits on it lets two processes "own" two different
files). `_lock` is the struct's last field, so it's dropped after the database connection is closed.
Each network has its own lock, so a regtest demo and a testnet4 wallet can be open together.

### Try it
```bash
cargo test -p btcw-core --test wallet          # create/open, lock, every open error (offline)
cargo test -p btcw-core --lib wallet           # receive offline + persist/reopen; regtest confirmations
                                               # (the node test skips without bitcoind; set BITCOIND_EXE)
```

## 5. Terminal app (CLI), Phase 1 / Agent D

**What we built:** `btcw`, the terminal frontend. It has no wallet logic of its own: every command
opens the wallet through `btcw_core::api`, makes one or two core calls and prints the result, either
as text for a person or as JSON for a script. All the Bitcoin work (keys, sync, balances) is in
§2–§4; this layer is about input, output and not leaking secrets on the way.

```text
crates/btcw-cli/src/
├── main.rs        clap definitions (the contract from PLAN §4), logging, dispatch
├── output.rs      network badge, colors, amounts, dates, tables, JSON and error rendering
├── prompt.rs      passwords and the recovery phrase
└── commands/      create, restore, address, sync, view (balance/history/utxos), mine
```

### Thin over the core
Every command has the same shape: open → call → drop the wallet (which releases its lock, §4) →
render. `balance` in full:

```rust
pub fn balance(cfg: &Config, ui: &Ui) -> Result<()> {
    let wallet = api::open_watch_only(cfg)?;       // no password
    let balance = wallet.balance();                // BalanceView, computed by BDK
    let synced_height = wallet.synced_height();
    drop(wallet);                                  // release wallet.lock before printing
    if ui.json() {
        return ui.print_json(&BalanceJson { network: cfg.network.to_string(), synced_height, balance });
    }
    ui.println(&format!("{} Balance {}\n{}", ui.out.badge(cfg.network), as_of(synced_height),
                        balance_lines(&balance, ui.out)))
}
```

| Command | Wallet access | Node needed | Core calls |
|---|---|---|---|
| `create` | creates it (new password) | optional, for the birthday | `api::create_wallet`, `Node::tip_height` |
| `restore` | creates it (new password) | optional, for the first sync | `api::restore_wallet`, `Node::sync` |
| `address new` / `list` | watch-only | no | `new_address` / `addresses` |
| `sync` | watch-only | yes | `Node::connect`, `Node::sync` |
| `balance`, `history`, `utxos` | watch-only | **no** | `balance` / `history` / `utxos` |
| `mine N [--to]` | watch-only, only without `--to` | yes, regtest | `new_address`, `Node::mine` |
| `send`, `status` | (Phase 2) | | |

Errors are never rewritten. The core's messages were written for people (§3, §4), so "wallet is open
in another btcw process", "no wallet found in …; create or restore one first" and "mainnet is
disabled; …" reach the terminal unchanged.

### Watch-only vs unlocked: why `balance` needs no password but `send` will
The wallet database holds only *public* descriptors (`wpkh([73c5da0a/84'/1'/0']tpub…/0/*)`, §2).
From those BDK can derive every address and recognise every payment, so balance, history, UTXOs and
new receive addresses need nothing secret. That's **watch-only** mode, `api::open_watch_only`.

Spending is different: a transaction must be signed with private keys, and those come only from the
recovery phrase, which lives encrypted in `seed.enc` (Argon2id + XChaCha20-Poly1305, §2). Decrypting
it takes the password. So `send` (Phase 2) will call `api::unlock_wallet(cfg, &password)`, get a
`Signer`, sign, and drop the signer as soon as the transaction is out. Day-to-day commands never ask
for a password, which also means they never have the keys in memory.

### Reading secrets
| Secret | Interactive | Scripts and tests |
|---|---|---|
| wallet password | hidden prompt on the terminal (`rpassword` opens `/dev/tty`) | `BTCW_PASSWORD` |
| recovery phrase (`restore`) | hidden prompt when stdin is a terminal | the first line of stdin: `echo "<words>" \| btcw restore` |

- Prompts are written to the terminal device, not to stdout, so they never end up in `--json` output
  or a redirected file.
- A **new** password is typed twice, must match, and must pass `api::check_new_password` (at least
  `MIN_PASSWORD_LEN` = 8 characters, the same rule the desktop app gets from the core). A short or
  mismatched entry gets a warning and another try, three in total.
- `BTCW_PASSWORD` exists for demos and tests and says so every time it's used
  (`note: using the wallet password from BTCW_PASSWORD (insecure; …)`). Environment variables leak
  through shell history, child processes, `/proc/<pid>/environ` and crash reports. An empty value
  counts as unset, so you get the prompt.
- Without a terminal and without `BTCW_PASSWORD` (cron, CI), the error says so instead of hanging:
  `cannot prompt for a password (No such device or address (os error 6)); for non-interactive use set BTCW_PASSWORD …`.
- `restore` checks the phrase (`keys::parse_mnemonic`: word list + checksum) *before* asking for a
  password, so a typo doesn't cost two password prompts. The error names a position, never a word.
- Every secret goes straight into a `SecretString` or `Zeroizing<String>`, which wipes its memory on
  drop. Those buffers are allocated at their final size up front, because a `String` that grows
  copies itself into a new allocation and frees the old one *without* wiping it.

### Why the phrase is shown exactly once
`create` prints the phrase in a numbered grid, then never again. The core never stores it
unencrypted, and showing it again would just put another copy on a screen, in a scrollback buffer or in
a screenshot. The warning says what matters:

```text
Recovery phrase (12 words). Write them down on paper, in this order:

     1. stomach     2. heavy       3. twin        4. puzzle
     5. bag         6. congress    7. track       8. wreck
     9. poverty    10. misery     11. struggle   12. snap

WARNING: Anyone with these words can take your coins. Keep them offline and private;
         never type them into a website. btcw will never show them again.
         They are the only backup: lose them and this computer, and the coins are gone.
```

The grid is written straight into one `Zeroizing<String>` (word slices, `write!`, no intermediate
copies) and printed *before* anything else that could fail: once `create_wallet` returns, the wallet
exists, and an error that skipped the phrase would leave a wallet nobody can back up. With `--json`,
the grid and warning go to **stderr** and stdout gets `{network, birthday_height, first_address}`, so
a script that logs its JSON never logs the phrase. In human mode, if stdout isn't a terminal
(`btcw create > file`), a warning says the phrase went into that file.

The **birthday** comes from the node: `create` asks for the tip height, and the first sync starts
there (§3). If the node is down, the wallet is still created, with a warning that the first sync will
scan from genesis.

### The network badge
Every human-readable result starts with `[testnet4]` (yellow), `[signet]` (magenta), `[regtest]`
(cyan) or `[mainnet]` (red), so test coins are never mistaken for real ones. Colors are on only when
the stream is a terminal and `NO_COLOR` is unset (no-color.org); stdout and stderr are decided
separately, so `btcw balance | less` gets plain text while warnings on stderr keep their color.

### `--json` and exit codes
With `--json`, stdout carries **exactly one JSON value** per run: the `btcw_core::types` views from
§4, inside a small wrapper that always names the network:

```console
$ btcw --json balance
{
  "network": "regtest",
  "synced_height": 0,
  "balance": {
    "confirmed_sat": 0,
    "unconfirmed_sat": 0,
    "immature_sat": 0,
    "total_sat": 0
  }
}
$ btcw --json address new | jq -r .address
bcrt1qz0muuvy3tku2unzd2zpqyla5u4k476mnhnskxz
```

| Command | JSON (besides `network`) |
|---|---|
| `create` | `birthday_height`, `first_address` (never the phrase) |
| `restore` | `birthday_height`, `synced_height`, `sync` (`SyncReport` or `null`), `sync_error`, `balance` |
| `address new` | the `AddressRow` fields |
| `address list` | `synced_height`, `addresses: [AddressRow]` |
| `sync` | the `SyncReport` fields: `tip_height`, `blocks_scanned`, `mempool_txs` |
| `balance` / `history` / `utxos` | `synced_height` + `balance` / `transactions: [TxRow]` / `utxos: [UtxoRow]` |
| `mine` | `to`, `wallet_address_index`, `block_hashes`, `tip_height` |

Amounts are integer satoshis, as everywhere in the core. Errors are JSON too, on stdout, with the
core's stable code (`WalletError::code()`, found by walking the `anyhow` error chain) or `cli` for
errors from the terminal layer itself:

```console
$ btcw --json send --to bcrt1q… --amount 1000; echo "exit=$?"
{
  "error": {
    "code": "cli",
    "message": "`btcw send` is not implemented yet (Phase 2)"
  }
}
exit=1
```

| Exit | Meaning | Human mode | JSON mode |
|---|---|---|---|
| 0 | success | result on stdout | one JSON value on stdout |
| 1 | error | `error: <message>` on stderr | `{"error":{"code","message"}}` on stdout |
| 2 | invalid command line | clap's usage message on stderr | `{"error":{"code":"cli",…}}` on stdout |

Everything that isn't the result goes to stderr: prompts (via the TTY), warnings, the progress bar
and logs. Logging is `tracing-subscriber` on stderr at level `warn` (`RUST_LOG` overrides it), so the
core's warnings, like "the node's chain is shorter than the wallet's" or the fee-rate fallback, are
visible without ever touching stdout.

### Progress and freshness
`sync` (and the scan at the end of `restore`) passes a callback to `Node::sync`, which calls it once
per block. The CLI turns that into an `indicatif` bar on stderr:

```text
⠙ Syncing [=========>                    ] block 31 of 102 (0s)
```

The bar counts only the blocks *this* sync downloads, from the block after the wallet's checkpoint
(or the birthday, on a first sync) to the tip, while the message shows absolute heights. In `--json`
mode, or when stderr isn't a terminal, the bar is hidden entirely, so logs and CI output don't fill up
with redraws.

`balance`, `history`, `utxos` and `address list` work offline, from the SQLite file. They say how
fresh their numbers are: `as of block 102 (run `btcw sync` to update)`, or `not synced yet` before the
first sync. Confirmation counts are relative to that block, as explained in §4.

### Smaller decisions
- **`mine` is regtest-only twice over.** `Node::mine` refuses other networks, and the CLI checks
  first too, so `btcw --network testnet4 mine 1` fails at once instead of needing a node or touching
  the wallet. `--to` is parsed as `Address<NetworkUnchecked>` and checked with `require_network`;
  a `tb1…` address on regtest gives `network mismatch: expected regtest, found a testnet4/signet
  address` (testnet4 and signet share the `tb` prefix, so the message names both).
- **Errors before prompts.** `create` and `restore` check `api::wallet_exists` before asking for
  anything (the core checks again, under the lock), so you don't type a password just to hear the
  wallet already exists.
- **No `.env` loading.** `scripts/regtest.sh env` prints `export` lines for `eval`. Silently reading
  a `.env` from the current directory or one of its parents could switch the network, datadir or node
  of a wallet without the user noticing, and invite people to keep `BTCW_PASSWORD` in a file.
- **`--rpc-user` / `--rpc-pass`** were added as global flags (they map onto `Overrides`). The help
  text says that other local users can see command-line arguments in `ps`, and points to the cookie
  file or `BTCW_RPC_PASS` instead.

### Demo session (regtest)
```console
$ scripts/regtest.sh start                   # bitcoind + "faucet" wallet with 101 blocks
regtest node up on 127.0.0.1:18443
$ eval "$(scripts/regtest.sh env)"            # BTCW_NETWORK, BTCW_RPC_URL, BTCW_RPC_COOKIE
$ cargo install --path crates/btcw-cli        # or: alias btcw=target/debug/btcw

$ btcw create
Choose a password to encrypt the recovery phrase on this computer (at least 8 characters).
You will need it to send coins; viewing the balance and history does not need it.
New wallet password:
Repeat the password:
[regtest] Created a new wallet in /home/me/.local/share/btcw/regtest

Recovery phrase (12 words). Write them down on paper, in this order:
  …the grid and warning shown above…

First receive address (#0): bcrt1qrk69aywnt0ujqfgz0qtv6lrx62qh70c4hpfw2w
Wallet birthday: block 101 (the first sync starts there)
Next: `btcw sync`, then `btcw balance`.

$ btcw address new
[regtest] Receive address #0
bcrt1qrk69aywnt0ujqfgz0qtv6lrx62qh70c4hpfw2w
btcw hands out this address until it receives a payment, so asking again returns it again.

$ scripts/regtest.sh fund bcrt1qrk69aywnt0ujqfgz0qtv6lrx62qh70c4hpfw2w 0.01
c60488d152e83d8cce92c1d9c7c1077d82746950a926403ac61669059f23d94f
$ btcw sync
[regtest] Synced to block 101: scanned 1 block; 1 unconfirmed wallet transaction waiting in the mempool.
$ btcw balance
[regtest] Balance as of block 101 (run `btcw sync` to update)
  Confirmed   0.00000000 BTC (0 sat)
  Unconfirmed 0.01000000 BTC (1,000,000 sat)
  Total       0.01000000 BTC (1,000,000 sat)

$ btcw mine 1                                # to the wallet's next unused address
[regtest] Mined 1 block to bcrt1qyluzz4nfscy4wmna0sft45vdgsxjz46r0glwz4 (wallet receive address #1); the node's tip is now block 102.
Run `btcw sync` to see the reward in the wallet; mined coins can be spent after 100 confirmations.
$ btcw sync
[regtest] Synced to block 102: scanned 1 block; no unconfirmed wallet transactions.
$ btcw balance
[regtest] Balance as of block 102 (run `btcw sync` to update)
  Confirmed    0.01000000 BTC (1,000,000 sat)
  Unconfirmed  0.00000000 BTC (0 sat)
  Immature    50.00002820 BTC (5,000,002,820 sat)  mined; spendable after 100 confirmations
  Total       50.01002820 BTC (5,001,002,820 sat)

$ btcw history
[regtest] 2 transactions as of block 102 (run `btcw sync` to update)
╭──────────────────┬──────────┬───────────────────────────────────────┬───────┬────────────────┬──────────────────────────────────────────────────────────────────╮
│ Date (UTC)       ┆ Type     ┆                                Amount ┆   Fee ┆ Status         ┆ Txid                                                             │
╞══════════════════╪══════════╪═══════════════════════════════════════╪═══════╪════════════════╪══════════════════════════════════════════════════════════════════╡
│ 2026-10-03 02:02 ┆ received ┆ +50.00002820 BTC (+5,000,002,820 sat) ┆ 0 sat ┆ 1 confirmation ┆ c39f788536d71bae7c729e11ad7edfcf39fe3f13cb3787254e0855d7c84e1a91 │
│ 2026-10-03 02:02 ┆ received ┆      +0.01000000 BTC (+1,000,000 sat) ┆     — ┆ 1 confirmation ┆ c60488d152e83d8cce92c1d9c7c1077d82746950a926403ac61669059f23d94f │
╰──────────────────┴──────────┴───────────────────────────────────────┴───────┴────────────────┴──────────────────────────────────────────────────────────────────╯
```

Two things in that history are worth a second look. The mined block pays 50 BTC **plus 2 820 sat**:
the miner also collects the fee of every transaction in the block, here our funding transaction. And
the fee column shows `—` for the incoming payment because the wallet never saw the sender's inputs (§4),
but `0 sat` for the coinbase, which has no inputs and so pays no fee. The `Immature` line disappears
once the reward has 100 confirmations: `scripts/regtest.sh mine 100` (to the faucet; `btcw mine 100`
would pay 100 *new* immature rewards to the wallet), then `btcw sync`.

### Testing
`crates/btcw-cli/tests/cli.rs` runs the real binary (`env!("CARGO_BIN_EXE_btcw")`) with a temporary
`--datadir`, `--network regtest`, `NO_COLOR=1`, `BTCW_PASSWORD`, and every inherited `BTCW_*`
variable removed, so a developer's own `eval "$(scripts/regtest.sh env)"` can't leak into a test.
"Offline" tests point `--rpc-url` at port 1, where nothing listens.

- **Errors:** `wallet_not_found` before `create` (and no directory left behind), `mainnet_disabled`,
  testnet3 → `config`, a 7-character password → `weak_password` with nothing written, `wallet_exists`,
  an invalid phrase → `invalid_mnemonic` without echoing it, no terminal and no `BTCW_PASSWORD` → a
  clear hint, usage errors → exit 2 (JSON with `--json`), `send`/`status` → "not implemented yet (Phase 2)".
- **Secrets:** `create --json` puts nothing but `{network, birthday_height, first_address}` on
  stdout; the 12 words parsed back from stderr derive that same first address, and restoring them
  in a fresh datadir gives it again. `echo "abandon … about" | btcw restore` yields
  `bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk`, which the test also derives independently with the
  core's `derive_account` and an in-memory BDK wallet.
- **With a node** (skipped without `bitcoind`): create (birthday = tip) → fund → `sync --json`
  (`mempool_txs ≥ 1`) → unconfirmed balance → `mine 1` to the wallet → sync → confirmed + immature
  balance, history with 1 confirmation each, two UTXOs → mine 2 → 3 confirmations → restore the
  phrase elsewhere: a full rescan from genesis finds the same balance and history.
- **Unit tests** for the formatting helpers: thousands separators, sat → BTC strings, signed amounts,
  UTC dates (leap days, 2100), the JSON error shape, the phrase grid and address network checks.

### Try it
```bash
cargo run -p btcw-cli -- --help
cargo test -p btcw-cli                                            # offline tests + unit tests
BITCOIND_EXE=~/.local/bin/bitcoind cargo test -p btcw-cli --test cli   # + the regtest round trip
```

## 6. Desktop UI, Phase 1 / Agent E — _pending_
## 7. Sending and status, Phase 2 / Agent F — _pending_
## 8. Tauri bridge, Phase 2 / Agent G — _pending_
## 9. End-to-end tests, Phase 2 / Agent H — _pending_
## 10. Running the demo (regtest and testnet4) — _pending_
## 11. Lessons learned — _pending_
