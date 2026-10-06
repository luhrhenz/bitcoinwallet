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
cargo run -p btcw-cli -- --help              # the full command surface
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
A birthday *above* the node's tip (the birthday block was reorged away, a restore was given a
height the chain hasn't reached, or the node is still catching up) means there
is nothing to scan yet: `sync` skips the blocks with a warning, still reads the mempool, and scans
once the chain gets there. (Before Agent H's e2e tests found it, the Emitter asked for the birthday
block by height and the sync failed with "Block height out of range"; §10.)

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

### A BDK bug we work around: `first_seen` after a reopen
Found by Agent F while building `status`: bdk_chain 0.23.3's `TxGraph::apply_changeset`, which
`Wallet::load` uses, replays each transaction's `last_seen` but **drops its stored `first_seen`**.
After a reopen an unconfirmed payment looked "first seen" at its *latest* mempool sighting. The
value is still correct in SQLite (`bdk_txs.first_seen`), so `WalletService::open` reads it back
and re-applies it as a sighting (`restore_first_seen`). BDK only ever moves `first_seen` earlier
and `last_seen` later, so this restores the right value and changes nothing else. The regression
test `first_seen_survives_reopen_after_later_sightings` failed before the fix and passes after.

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
| `send` | **unlocked** (password) | yes | `api::unlock_wallet`, `tx::prepare_send`, `tx::sign_psbt`, `tx::broadcast_signed` (§7) |
| `status <txid> [--watch]` | watch-only, reopened per poll | yes (falls back to the last sync) | `Node::sync`, `tx::tx_status` (§7) |

Errors are never rewritten. The core's messages were written for people (§3, §4), so "wallet is open
in another btcw process", "no wallet found in …; create or restore one first" and "mainnet is
disabled; …" reach the terminal unchanged.

### Watch-only vs unlocked: why `balance` needs no password but `send` will
The wallet database holds only *public* descriptors (`wpkh([73c5da0a/84'/1'/0']tpub…/0/*)`, §2).
From those BDK can derive every address and recognise every payment, so balance, history, UTXOs and
new receive addresses need nothing secret. That's **watch-only** mode, `api::open_watch_only`.

Spending is different: a transaction must be signed with private keys, and those come only from the
recovery phrase, which lives encrypted in `seed.enc` (Argon2id + XChaCha20-Poly1305, §2). Decrypting
it takes the password. So `send` calls `api::unlock_wallet(cfg, &password)`, gets a
`Signer`, signs, and drops the signer as soon as the transaction is signed (§7). Day-to-day commands never ask
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
         never type them into a website or keep a photo of them.
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
    "message": "--json needs --yes: a JSON run can't stop to ask for confirmation; review the payment without --json first"
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
  clear hint, usage errors → exit 2 (JSON with `--json`). (`send`/`status` were stubs here; §7 covers them.)
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

## 6. Desktop UI, Phase 1 / Agent E

**What we built:** every screen of the desktop app (PLAN §4.2) in React + TypeScript, plus an
in-memory **mock backend** that behaves like the Rust side will. The UI holds no wallet logic: it
asks the backend, shows the answer, and makes the dangerous moments (backup, sending) hard to get
wrong. Nothing here touches keys; that stays in Rust (§2) behind the Tauri commands (§8).

```text
apps/desktop/src/
├── lib/api.ts, types.ts   the contract with Rust (WalletApi + the serde shapes)
├── lib/mock.ts            in-memory WalletApi for dev and tests (BIP39, bech32, coin selection)
├── lib/amount.ts          sat ↔ BTC strings, integer math only
├── lib/errors.ts          ApiError.code → plain words, the only place that does it
├── state/wallet.tsx       shared state: info, balance, history, sync, lock/unlock, auto-lock
├── screens/               Welcome, CreateWallet, RestoreWallet, Dashboard, Receive, Send,
│                          TxDetail, History, Settings
└── components/            network badge and tape, amounts, chunked addresses, unlock dialog, …
```

### One contract, two implementations
Screens import `api` and nothing else. `api` is a `WalletApi`, an interface with one method per
Tauri command, and every method returns a promise that either resolves with a `types.ts` shape or
rejects with `ApiError { code, message }`, where `code` is `WalletError::code()` from §1:

```ts
export interface WalletApi {
  createWallet(words: 12 | 24, password: string): Promise<{ mnemonic: string[] }>;
  prepareSend(to: string, amountSat: number, feeRateSatVb: number | null): Promise<PreparedSend>;
  confirmSend(id: string): Promise<{ txid: string }>;
  // … unlock, lock, sync, balance, history, txStatus, settings
}
```

There are two implementations. `tauriApi` calls `invoke("create_wallet", …)` and so on; Agent G
writes the Rust side of those calls. `createMockApi()` keeps a fake node and wallet in memory.
Because both satisfy the same interface, the whole UI was built and tested before a single Tauri
command existed, and swapping in the real one changes no screen. `main.tsx` picks the mock for
`npm run dev:mock`, or whenever the page isn't running inside Tauri (`window.__TAURI_INTERNALS__`
is missing), and loads it with a dynamic `import()`, so the Tauri app never even downloads it. In
demo mode a warning-colored "Demo mode: mock backend, not a real wallet" button sits on every screen.

The mock is not a stub that returns canned values. It follows the core's rules so the UI meets every
real error path: phrases are real BIP39 (word list and checksum, messages that name positions,
never words), addresses are real bech32 for the network, `prepareSend` checks the prefix, dust
(294 sat), funds and a locked wallet, and builds a P2WPKH fee preview (141 vB for 1 input and 2
outputs). The wallet only learns about the chain by syncing: a payment appears after `sync`, and a
mined block turns into confirmations after the next `sync` or `txStatus`. Tests drive it with
`simulateIncoming(sat)`, `mineBlocks(n)` and `setNodeOnline(false)`.

### The security rules, from the UI's side
| Rule | How the UI keeps it |
|---|---|
| The phrase crosses into JS **once** | Only `createWallet` returns it. It lives in the create screen's state until the backup check passes, then `setMnemonic(null)`. It is never written to `localStorage`, a URL or a log; tests assert all three. The grid is `user-select: none` and has no copy button, because clipboard managers keep history. |
| A typed phrase leaves quickly too | `restoreWallet` gets a normalized copy; the textarea is cleared right after. It has `autoComplete="off"`, `spellCheck={false}`, `autoCorrect`/`autoCapitalize="off"`, so neither the OS nor a browser extension learns the words. |
| Passwords go straight to Rust | They're sent to `unlock`/`createWallet`/`restoreWallet` and the field is emptied. JS strings can't be wiped like `Zeroizing` buffers, so the UI keeps them short-lived instead. |
| The PSBT stays in Rust | `prepareSend` returns a `SendPreview` and an opaque `id`; `confirmSend(id)` signs and broadcasts on the Rust side. The UI never sees the transaction, so it can't alter it. |
| Watch-only by default | The dashboard, history and receive screens work while locked. Only sending asks for the password, in a dialog, at the moment it's needed. |

### Backup: show once, then check
The create flow is three numbered steps, because the order matters: **password → write down → check**.
The words stay hidden until "Show the 12 words" ("make sure nobody can see your screen"), next to
the warning the CLI prints (§5): anyone with these words can take the coins; paper, not screenshots;
btcw never shows them again. "Continue" stays disabled until the user ticks "I've written all 12
words down". Then the check asks for three random positions (`crypto.getRandomValues`), compared
case- and space-insensitively. A wrong word keeps the user there, with a way back to the words.
Only a correct check finishes the flow and drops the phrase. Without the check, the classic failure
is a backup with a missing or swapped word, found years later when it's needed.

### Sending: the full address, in groups of four
The preview shows everything the user is about to sign: amount, fee (sat, sat/vB and vsize),
change, and the total leaving the wallet. The recipient is shown **in full**, never shortened, split
into groups of four:

```text
SENDING TO   tb1q w508 d6qe jxtd g4y5 r3za rvar y0c5 xw7k xpjz sx
```

Clipboard-swapping malware replaces a copied address with the attacker's own, usually one that
starts and ends like the real one. A shortened `tb1qw5…pjzsx` would hide exactly the part that
changed; groups of four can be ticked off one by one against the recipient's screen or paper, like
a hardware wallet's display. (The gaps are CSS margins, so copying the text still gives the exact
address.) Fees above 10% of the amount or above 100 sat/vB get a warning, and mainnet says "This
sends real bitcoin". Cancel calls `cancelSend` so Rust releases the change address; leaving the
screen or an auto-lock with a preview open does the same, and nothing is sent until "Send … BTC".

After confirming, the transaction screen polls `txStatus` every 10 s while it's open (the interval
is cleared on unmount; a test checks it) and draws confirmations as six little blocks filling up:
"unconfirmed · waiting in the mempool" → "Confirmed · 1 confirmation" → … → treated as final at six.

### Amounts are integers
Every amount the backend sends is satoshis. BTC strings are built and parsed with integers, the
same way the CLI's `output::btc` does it:

```ts
formatBtc(1_000_000)   // "0.01000000": Math.floor(sat / 1e8) + "." + (sat % 1e8) padded to 8
parseBtc("4.35")       // 435_000_000 sat, digit by digit with BigInt (4.35 * 1e8 is 434999999.99999994)
parseBtc("0,001")      // error: "Use a dot for decimals"; in many countries that comma is a decimal point
```

`parseBtc` accepts at most 8 decimals and at most 21 million BTC; `parseSat` accepts whole numbers,
with `,`/`_`/space only as proper thousands groups. The send form has a BTC/sat toggle that converts
exactly (`0.29` ↔ `29000000`). Displays keep all 8 decimals but dim the trailing zeros
(`0.0125`**`0000`**), so magnitudes are easy to read without hiding a digit.

### The network, everywhere
Every screen has the network twice: a badge in the top bar (orange `TESTNET4`, violet `SIGNET`,
teal `REGTEST`, red `MAINNET`) and a tape down the left edge of the window in the same color, with
the network name running down it, hatched for test networks and solid for mainnet. The tape is
the one bold element in an otherwise quiet design, and it never scrolls away. Mainnet, if a build
ever offers it, also gets a red banner, and switching to it in Settings needs `MAINNET` typed in. Switching
networks explains what happens: each network has its own wallet in its own folder; the current one
stays untouched, and a network without a wallet opens at Welcome.

### Auto-lock
`useAutoLock` runs only while the wallet is unlocked. Any key, click, pointer move or scroll
restarts a countdown of `auto_lock_minutes`; when it runs out, the UI calls `api.lock()` (Rust drops
the signer), refreshes `appInfo`, and says "Locked after 5 minutes without activity. Viewing still
works; sending needs your password." Any open send preview is discarded at the same moment.

### Errors in one place
`lib/errors.ts` maps every `WalletError` code to a sentence that says what happened and what to do:
`rpc` → "Can't reach your Bitcoin node. Check that bitcoind is running and that the node settings are
right.", `wallet_in_use` → "open in another btcw window or terminal", `network_mismatch` → "On
testnet4, addresses start with tb1." Where the core's own message adds facts (amounts, a word
position, the node's reason) it's shown underneath, first line only. Stack traces never are.

### What the Rust side (Agent G) has to match
- `AppInfo.synced_height: number | null` (added to `types.ts`): `WalletService::synced_height()`, or
  `null` with no wallet. It's how the dashboard says "as of block N" even when the node is down.
- A locked wallet: `prepare_send` / `confirm_send` reject with code `"locked"` (not a `WalletError`
  code; the UI then asks for the password).
- `tx_status` should sync before reading (as `tx::tx_status`'s doc says), or the open transaction
  screen never sees new confirmations.
- `sync` emits `sync-progress` events (`{height, tip_height}`), as `api.ts` already listens for.

### Testing
Vitest + Testing Library against the mock with zero latency; fake timers for time.
- **Amounts:** formatting identical to the CLI's tests; `0.1 + 0.2`, `4.35`, `1.15` exact; 8-decimal and
  21M limits; commas, exponents, signs rejected; the unit toggle round-trips.
- **Create:** the words appear once; "Continue" needs the checkbox; a wrong word blocks; the right
  ones (any case) finish; afterwards no word is on screen, in storage or in the URL. Short and
  mismatched passwords are caught before the backend is called.
- **Unlock and auto-lock:** wrong password → "That password is not correct"; Escape cancels;
  activity postpones the lock; idle time triggers exactly one `lock()`.
- **Send:** wrong network, invalid address, dust and insufficient funds land on the right field;
  locked → password first; preview → confirm → "Unconfirmed" → mine → "1 confirmation" → "2";
  cancel calls `cancelSend`; polling stops when the screen closes; locking discards a preview.
- **Everywhere:** the badge on every screen and on Welcome; restore with progress; settings
  validation and network switching; a failed startup shows words and a retry, not a trace.
- **The mock itself:** SHA-256, BIP39 and BIP173 test vectors, coin selection, fee math.

### Try it
```bash
cd apps/desktop
npm ci
npm run dev:mock        # http://localhost:1420, in-memory backend; open "Demo mode" for test coins
npm test                # 56 tests
npm run typecheck && npx vite build
```

## 7. Sending and status, Phase 2 / Agent F

**What we built:** the part that moves money. `tx.rs` turns "pay 100 000 sat to this address" into
a signed transaction on the network: validate the address, let BDK pick coins and build an unsigned
**PSBT**, summarise it for the user, sign, finalize, extract, broadcast, and record it in the wallet.
It also answers "where is my transaction?" (unconfirmed, or confirmed with N confirmations). On top
of it, the CLI gets `btcw send` and `btcw status [--watch]`.

```text
crates/btcw-core/src/tx.rs      parse_address · build_psbt · preview · cancel · sign_psbt · extract_tx
                                · record_broadcast · tx_status, plus prepare_send / complete_send
                                (/ broadcast_signed): one code path for the CLI and the desktop app
crates/btcw-cli/src/commands/   send.rs, status.rs
```

### Spending means choosing coins
As §4 explained, a wallet holds no balance, only **UTXOs**: outputs of earlier transactions that pay
one of our scripts. A payment's *inputs* point at some of those outputs (`txid:vout`) and spend each
one **whole**. Picking which ones is **coin selection**. BDK's default first tries *branch and bound*,
which looks for a set of coins that matches amount + fee exactly (no change needed, which saves a
little fee and is better for privacy), and otherwise falls back to picking coins at random until
there's enough. One of the offline tests funds a wallet with coins of 60 000 and 70 000 sat and pays
100 000: neither coin is enough alone, so the transaction has two inputs.

If the coins aren't enough, BDK reports both sides and they reach the user unchanged:
`insufficient funds: need <amount + fee> BTC, available <spendable> BTC` (`insufficient_funds` in
JSON). Immature mining rewards (§4) don't count as available.

### Change goes to a fresh internal address
Spending a 1 000 000 sat coin to pay 100 000 creates a *second* output for the rest: the **change**.
It goes to the **internal** keychain (`m/84'/1'/0'/1/i`), addresses that are never shown to anyone,
and to an index that has never been used. If change went back to the address the coin came from,
anyone looking at the chain could tell which output was the payment and which one is still ours, and
link our payments together. BDK takes the lowest revealed-but-unused internal index; the regtest test
checks the leftover coin really lands there (`keychain: internal` in `btcw utxos`).

### Fees: inputs minus outputs, and sizing a transaction before it's signed
There is no "fee" field in a transaction. The fee is whatever the inputs hold that the outputs don't
pay out, and miners keep it. A **fee rate** is that fee divided by the transaction's size, in
sat per *virtual byte*. SegWit made size a weighted sum (§3): every byte of the witness (signatures,
public keys) weighs 1 **weight unit**, every other byte 4, and `vsize = ⌈weight ÷ 4⌉`. That discount
is why native SegWit inputs are cheap.

The catch: the fee has to be decided *before* signing, but the signatures are part of the size. So the
unsigned transaction is measured and what signing will add is added on top, using the largest
signatures possible. That's `estimated_signed_weight`:

```text
unsigned transaction      every byte × 4 WU (no witnesses yet, so no segwit marker either)
+ 2 WU                    segwit marker + flag bytes (witness data: 1 WU each)
+ per input   1 WU        the witness's item count
            + 107 WU      miniscript's max_weight_to_satisfy() for wpkh:
                            1 + 72  signature push (DER ≤ 71 bytes with low-S, + 1 sighash byte)
                            1 + 33  compressed public key push
```

For one P2WPKH input and two P2WPKH outputs that's 562 WU, **141 vB**, the number the preview shows.
About half of real signatures are a byte shorter, so the estimate is an upper bound and the fee rate
actually paid is never below the one shown. The tests check the bound against the signed transaction:
`real vsize ≤ preview vsize ≤ real vsize + 2 per input`.

BDK's coin selection sizes inputs exactly the same way, and charges per weight unit: at 5 sat/vB
(1.25 sat/WU) 562 WU cost 702.5, rounded up to 703 sat. The preview divides by whole vbytes, the way
Core and block explorers do, so it says 703 ÷ 141 = **4.99 sat/vB** rather than 5. That's not a bug in
the fee, just honest rounding; per weight unit the payment is at least the rate asked for, which the
tests check too.

Two limits are enforced before anything is built: at least **1 sat/vB** (nodes don't relay less) and
at most **25 000 sat/vB**, the limit rust-bitcoin's `extract_tx` uses to catch the classic "typed BTC
into the sat field" mistake, so a bad rate fails before the user is asked to confirm, not after
signing. Amounts below the **dust** limit (294 sat for P2WPKH, 546 for an old P2PKH address: below
that, spending the output would cost more than it's worth, so nodes won't relay it) are refused too.

### What the user confirms: a preview that explains every output
`preview` doesn't just add up numbers. It finds the output that pays exactly `amount` to `to`, and
requires **every other output** to pay this wallet's change keychain; anything else is an error. The
summary therefore always covers the whole transaction: `total = amount + fee` is what really leaves
the wallet, and an output the user didn't ask for can't hide behind it.

```text
[regtest] Payment preview
  To        bcrt 1q4z wueh x60r hdg9 tvvh vgf3 mefz du6y gw95 72c6
  Amount    0.00100000 BTC (100,000 sat)
  Fee       0.00000703 BTC (703 sat)
  Fee rate  4.99 sat/vB for an estimated 141 vB
  Change    0.00899297 BTC (899,297 sat), back to this wallet
  Total     0.00100703 BTC (100,703 sat), amount + fee
Send? [y/N]
```

The recipient is shown **in full, in groups of four**, never shortened, for the same reason as in the
desktop app (§6): clipboard malware swaps addresses for look-alikes that share the start and the end.
A fee of 10% of the amount or more, or a rate above 100 sat/vB, gets a warning, and on mainnet so does
the fact that the coins are real. The question is read from the **terminal** (`/dev/tty`), never from
stdin, so `yes | btcw send …` can't approve a payment by accident; without a terminal, `send` refuses
up front and points to `--yes`. `--json` requires `--yes`, because a JSON run can't stop to ask.

`send` also checks everything it can *before* the password prompt: the address and its network, dust,
the fee rate, whether `--psbt-out` already exists, whether there is a terminal to ask on. A typo never
costs a password and a sync.

### The PSBT lifecycle (BIP174), and why signing is split in two
A **PSBT** (partially signed bitcoin transaction) is the unsigned transaction plus everything a signer
needs: for each input, the output it spends (`witness_utxo`, which is how the fee is known before
signing) and which key can sign it (`bip32_derivation`: master fingerprint + path, e.g.
`73c5da0a/84'/1'/0'/0/0`). It moves through four roles:

| Step | Who | What changes |
|---|---|---|
| **create** | BDK `build_tx().add_recipient(..).fee_rate(..).finish()` | coin selection, change output, unsigned tx + input metadata |
| **sign** | rust-bitcoin `Psbt::sign(master_xprv)` via `keys::Signer` | derives each input's key from its `bip32_derivation`, adds `partial_sigs` |
| **finalize** | BDK `finalize_psbt` | turns signature + public key into the input's final witness, clears the rest |
| **extract** | rust-bitcoin `Psbt::extract_tx` | the network transaction, refused above 25 000 sat/vB |

Signing and finalizing are done by different libraries on purpose (Phase 0 finding 1). BDK 3.2
deprecated keeping keys inside the wallet, so the wallet has only public descriptors and the
`Signer` has only the master key. Finalizing needs to know the *script* (here `wpkh`: the witness is
`[signature, public key]`), which the descriptors know; it needs no secret. `sign_psbt` insists
the key signed **every** input (`this wallet's key signed 0 of 1 inputs` for a PSBT from another
seed) and that BDK finalized every input, and `extract_tx` refuses an input without a final witness,
which would otherwise produce a transaction every node rejects. The CLI drops the `Signer`, wiping
the master key, the moment signing is done, before anything touches the network.

`--psbt-out FILE` saves the **unsigned** PSBT as base64 (a new file, mode 0600: no keys in it, but
it lists the wallet's coins and paths), so Core can double-check it:

```text
$ bitcoin-cli decodepsbt "$(cat payment.psbt)"        (abridged)
  tx.txid           3485a59489e4cd7425887ced25fe63f90d33989339dee25ea0675dc1185312d6
  tx.vin[0].sequence  4294967293 (0xfffffffd)
  tx.locktime       102
  fee               0.00000703
```

Same txid as the broadcast transaction (a SegWit txid doesn't cover the witness, so signing doesn't
change it), and Core computes the same fee.

### RBF signalling and anti fee-sniping
Two of BDK's defaults are kept and are visible above. Every input's `nSequence` is **`0xFFFFFFFD`**,
which signals *replace-by-fee* (BIP125: anything below `0xFFFFFFFE`). A payment stuck at a low fee can
then be replaced by one paying more; Core 28+ allows replacement regardless, but signalling it is the
honest thing to tell other wallets. `nLockTime` is the current tip (102): the transaction can't be
mined in a block *below* the tip, which takes away a miner's incentive to rewrite recent blocks just to
collect our fee (*fee sniping*).

### Broadcast, then record it straight away
`Node::broadcast` is `sendrawtransaction` (§3). As soon as the node accepts the transaction,
`record_broadcast` applies it to the wallet as unconfirmed (`apply_unconfirmed_txs([(tx, now)])`) and
persists. Without that, the wallet wouldn't know about its own payment until the next sync: the
balance would still count the spent coin, and a second payment could try to spend it again. With it,
`btcw balance` right after a send shows the spent coin gone and the change as unconfirmed (BDK counts
it as *trusted* pending, because we made it).

If recording fails *after* the broadcast (a database error), that is only a warning: the coins are on
their way, the spend stays staged in memory, and the next sync finds it in the mempool anyway. Reporting
"send failed" for a payment that was sent would be the worse mistake.

### Cancel: giving the change address back
Building a transaction reveals the change address and marks it used, in memory, so a second payment
prepared in the meantime gets a different one. If the user says no, that address was never used on
chain. BDK 3.2 has no `cancel_tx`, so `cancel` does it by hand: for each output that pays our internal
keychain, `unmark_used(Internal, index)`, and the next payment gets the same address again. Without it
every declined preview would leave a never-used change address behind; in the desktop app, which stays
open and may prepare many payments, they would pile up past the lookahead. `prepare_send`,
`complete_send` and `broadcast_signed` call `cancel` themselves on every error before the broadcast
succeeds; the CLI calls it when the answer is no. The tests check the index is reused.

### Status and polling for confirmations
`tx_status` looks the transaction up in the wallet (`get_tx`) and maps its chain position with the
same helper `history` uses (§4): unconfirmed with the time it was first seen, or confirmed at height
*h* with `tip − h + 1` confirmations, counted from the wallet's synced tip. `btcw status TXID` syncs
first. If bitcoind can't be reached it answers anyway, from the database, with a warning saying it's
as of the last sync.

`--watch` repeats that every `--interval` seconds (10 by default) until `--until` confirmations (1
by default). Each check opens the wallet, syncs, reads and **closes** it, so between checks the wallet
lock is free for the desktop app or another `btcw` command, and Ctrl-C during the wait can't interrupt
a database write. It prints a line only when something changes. It compares the printed text rather
than the raw status, because BDK 0.23 reloads `first_seen` from the stored *last*-seen time, so that
field creeps forward every time the wallet is reopened. A node that's down gives a warning and another
try; a transaction the wallet doesn't know after a successful sync is `tx_not_found`.

### One path for both frontends
The desktop app (§8) can't run `send` start to finish: the user looks at the preview in between, and
the PSBT must stay in Rust while they do. So the core offers the two halves, and the CLI uses them too:

```rust
// "Review payment": parse `to`, fee rate (given, or the node's 6-block estimate), build, preview.
pub fn prepare_send(wallet: &mut WalletService, node: &Node, to: &str, amount_sat: u64,
                    fee_rate: Option<FeeRate>) -> Result<(Psbt, SendPreview)>;
// "Send": sign_psbt, then broadcast_signed (extract → broadcast → record_broadcast).
pub fn complete_send(wallet: &mut WalletService, signer: &Signer, node: &Node, psbt: Psbt) -> Result<Txid>;
pub fn broadcast_signed(wallet: &mut WalletService, node: &Node, psbt: Psbt) -> Result<Txid>;
// "Cancel"
pub fn cancel(wallet: &mut WalletService, psbt: &Psbt);
```

`check_amount` and `check_fee_rate` are the rules `build_psbt` applies, exposed so a form can reject
bad input before unlocking or syncing.

### Demo session (regtest)
After the §5 session (one confirmed 1 000 000 sat coin), with a real terminal:

```console
$ btcw send --to bcrt1q4zwuehx60rhdg9tvvhvgf3mefzdu6ygw9572c6 --amount 100000 --fee-rate 5 --psbt-out payment.psbt
Wallet password:
[regtest] Payment preview
  To        bcrt 1q4z wueh x60r hdg9 tvvh vgf3 mefz du6y gw95 72c6
  Amount    0.00100000 BTC (100,000 sat)
  Fee       0.00000703 BTC (703 sat)
  Fee rate  4.99 sat/vB for an estimated 141 vB
  Change    0.00899297 BTC (899,297 sat), back to this wallet
  Total     0.00100703 BTC (100,703 sat), amount + fee
note: wrote the unsigned PSBT to payment.psbt (inspect it with `bitcoin-cli decodepsbt "$(cat payment.psbt)"`)
Send? [y/N] y
[regtest] Sent 0.00100000 BTC (100,000 sat) to bcrt1q4zwuehx60rhdg9tvvhvgf3mefzdu6ygw9572c6
Txid: 3485a59489e4cd7425887ced25fe63f90d33989339dee25ea0675dc1185312d6
It is waiting in the mempool now; follow it with `btcw status 3485a59489e4cd7425887ced25fe63f90d33989339dee25ea0675dc1185312d6 --watch`.
$ TXID=3485a59489e4cd7425887ced25fe63f90d33989339dee25ea0675dc1185312d6

$ btcw balance                                # no sync needed: the spend was recorded
[regtest] Balance as of block 102 (run `btcw sync` to update)
  Confirmed   0.00000000 BTC (0 sat)
  Unconfirmed 0.00899297 BTC (899,297 sat)
  Total       0.00899297 BTC (899,297 sat)

$ btcw status $TXID
[regtest] Transaction 3485a59489e4cd7425887ced25fe63f90d33989339dee25ea0675dc1185312d6
  Status      unconfirmed, waiting in the mempool
  First seen  2026-10-03 09:04 UTC
  As of       block 102

$ btcw status $TXID --watch --until 3 --interval 2     # meanwhile: regtest.sh mine 1, then mine 2
[regtest] Watching 3485a59489e4cd7425887ced25fe63f90d33989339dee25ea0675dc1185312d6 until it has 3 confirmations (checking every 2 s; Ctrl-C to stop)
  block 102      unconfirmed, waiting in the mempool
  block 103      confirmed in block 103: 1 confirmation
  block 105      confirmed in block 103: 3 confirmations

$ btcw --json status $TXID
{
  "network": "regtest",
  "txid": "3485a59489e4cd7425887ced25fe63f90d33989339dee25ea0675dc1185312d6",
  "synced_height": 105,
  "status": { "state": "confirmed", "height": 103, "confirmations": 3, "block_time": 1791018295 },
  "sync_error": null
}
```

Answering `n` instead prints `[regtest] Cancelled; nothing was sent.` and exits 0. With bitcoind
down, `status` still answers, and says so:

```text
warning: could not sync with bitcoind, so this is the status as of block 107 (the last sync): bitcoin node RPC error: getblockchaininfo: cannot connect to bitcoind at http://127.0.0.1:1 (connection refused); is bitcoind running for regtest? check --rpc-url
[regtest] Transaction 1aea59a54784135f5becbe2380fd0fc0e47a56b4eae23dd541ab950d66ac5935
  Status      confirmed, 2 confirmations
  Block       106, mined 2026-10-03 09:07 UTC
  As of       block 107 (bitcoind unreachable; may be out of date)
```

| Command | JSON (besides `network`) |
|---|---|
| `send --yes` | `txid`, `preview` (the `SendPreview`: `to`, `amount_sat`, `fee_sat`, `fee_rate_sat_vb`, `vsize`, `change_sat`, `total_sat`) |
| `status` | `txid`, `synced_height`, `status` (the `TxStatus`), `sync_error` (`null`, or why it couldn't sync) |

### Testing
- **Offline unit tests** (`src/tx.rs`, a wallet funded with made-up transactions): address parsing
  and network wording; a two-input payment through build → preview (`total = amount + fee`, fee =
  inputs − outputs, change on the internal keychain, 209 vB) → sign → extract (estimate within
  +0/+2 vB per input of the signed size, rate per weight unit ≥ the one asked for) → record (balance
  pending at once); RBF sequence and lock time; `cancel` reuses the change index; dust (P2WPKH and
  P2PKH), fee rates 0, 0.996 and above 25 000 sat/vB, insufficient funds with BDK's numbers, an address
  checked for another network; another seed's signer (`signed 0 of 1 inputs`); extracting an unsigned
  PSBT; a preview with the wrong amount, the wrong recipient or an extra output; a malformed PSBT is
  an error, not a panic inside rust-bitcoin.
- **Regtest** (`tests/tx.rs`, two node tests): the step-by-step path through broadcast, the node's
  mempool, record, reopen, mine → 1 confirmation → 3; and `prepare_send` / `complete_send` with the
  node's fee estimate, a stranger's signer (nothing broadcast, change address released), then a
  real send to confirmation.
- **CLI** (`tests/cli.rs`): offline, every early refusal comes before the password (each run uses a
  wrong one to prove it), then `wrong_password`, `rpc`, malformed and unknown txids, and no terminal
  without `--yes`. With a node: `insufficient_funds`; a payment declined at a real prompt (a
  pseudo-terminal via `script`); `send --yes --json --psbt-out` (PSBT parsed back, mode 0600, same
  txid, `decodepsbt` agrees on the fee); balance before any sync; `status --json`; `--watch --until 2`
  while two blocks are mined; a human-mode send with both fee warnings.
- One flaky-test trap found on the way: `TestNode::available()` runs `bitcoind -version`, which in
  Core v31 rewrites `~/.bitcoin/settings.json`. Two at once can fail on the rename, and the test then
  *skips* as if bitcoind were missing. The new tests serialize and retry that probe.

### Try it
```bash
cargo test -p btcw-core --lib tx::                                    # offline
BITCOIND_EXE=~/.local/bin/bitcoind cargo test -p btcw-core --test tx  # regtest send → confirm
BITCOIND_EXE=~/.local/bin/bitcoind cargo test -p btcw-cli --test cli  # the CLI, end to end
```

## 8. Backup check, showing the phrase, password after the preview (lead)

**Why:** the 12 words on paper are the wallet's only real backup. A wallet whose owner never wrote
them down correctly works perfectly right up until the laptop dies, and then the coins are gone.
So btcw keeps asking until the user has *proved* their copy is right, and lets them see the words
again (with the password) if they need to redo it.

### The flag
`btcw_meta` (the wallet's own SQLite table, §4) gets a `backup_verified` row:

| Event | Flag |
|---|---|
| `create` | unverified (no row yet; a missing row *means* unverified, so a crash can only cause an extra reminder) |
| `restore` | verified (the user just typed the whole phrase in) |
| `backup verify` succeeds | verified |

`WalletService::read_backup_verified(cfg)` reads it **without the wallet lock** (read-only SQLite),
so the reminder works even while the desktop app has the wallet open.

### Verifying
```rust
let positions = api::backup_challenge(word_count)?;          // e.g. [2, 6, 7], random, ascending
let answers: Vec<(usize, String)> = /* hidden prompt per position */;
api::verify_backup(cfg, &mut wallet, &password, &answers)?;  // compares against the decrypted phrase
```
- Positions come from the OS RNG with rejection sampling (no modulo bias).
- The words are typed **hidden**: on screen they'd be as good as the phrase to anyone watching.
- A mismatch is `backup_mismatch` with **positions only** ("word 7 does not match your recovery
  phrase"), never the words.
- The password is needed because the only copy of the phrase on the computer is encrypted (§2).
- Scripts can pipe the whole phrase: `echo "<words>" | btcw backup verify` checks every word.

### Showing the phrase again
`btcw backup show` (and the desktop "Show recovery phrase") decrypt with the password and print
the numbered grid, **at any time**, the owner's choice, like Electrum or Sparrow. It is never
available as `--json`: the phrase must not end up in machine-readable output, logs or pipes.

### The reminder
After every other command, while unverified:
```text
warning: your recovery phrase backup is not verified yet: run `btcw backup verify` (`btcw backup show` displays the words again)
```
It goes to stderr, so `--json` output stays one clean JSON value.

### `send` asks for the password last
`send` now opens the wallet **watch-only**, syncs, builds the PSBT, shows the preview, asks
`Send? [y/N]`, and only then asks for the password (`api::load_signer`, which also checks the
decrypted seed belongs to this wallet). So the master key is never in memory while the user reads
the preview, and a declined payment never decrypts anything:
```text
[regtest] Payment preview
  To        bcrt 1q4z wueh …
  …
Send? [y/N] y
Wallet password:
[regtest] Sent 0.00100000 BTC (100,000 sat) to bcrt1q4z…
```

## 9. Tauri bridge, Phase 2 / Agent G

**What we built:** the Rust half of the desktop app. `apps/desktop/src-tauri` (crate
`btcw-desktop`) answers every call the React UI makes through `api.ts` by running the same
`btcw-core` functions the CLI uses, and keeps every secret on the Rust side while it does. The UI
also gained the backup features from §8: a reminder until the backup is verified, a "Verify backup"
dialog, and "Show recovery phrase" in Settings.

```text
apps/desktop/src-tauri/
├── tauri.conf.json          window, CSP, icons; no plugins
├── capabilities/            what the webview may call besides our commands: listen/unlisten only
└── src/
    ├── commands.rs          every command as a plain function over AppState (unit-tested)
    ├── state.rs             AppState: settings, the signing session, per-network wallet gates
    ├── settings.rs          <datadir>/desktop.json, applied as config::Overrides
    ├── error.rs             ApiError { code, message }
    ├── ipc.rs               thin #[tauri::command] wrappers (run on a blocking thread)
    └── lib.rs, main.rs      the window (navigation locked to the app), logging, startup
apps/desktop/src/components/ BackupReminder, VerifyBackup, RevealPhrase, Phrase (grid + warning)
```

### Tauri's model: a webview and Rust, with IPC in between
A Tauri app is one native process. It opens a window with the operating system's webview
(WebKitGTK on Linux) and loads the React build into it from inside the binary (`tauri://localhost`;
nothing is fetched from the network). The page can't touch files, sockets or keys. It can only
send messages to Rust over Tauri's IPC: `invoke("balance")` serializes the call to JSON, Tauri
finds the Rust function registered as `balance`, deserializes the arguments (JS `amountSat` →
Rust `amount_sat`), runs it, and sends the `Result` back as JSON. `Ok` resolves the promise, `Err`
rejects it. Events go the other way: `sync` calls `app.emit("sync-progress", …)` and `api.ts`
`listen`s for it.

```rust
#[tauri::command]
pub async fn balance(state: State<'_, Arc<AppState>>) -> ApiResult<BalanceView> {
    blocking("balance", &state, commands::balance).await   // spawn_blocking + log the code
}
```

Every wrapper looks like that. The work happens in `commands.rs`, in plain functions that take
`&AppState` and know nothing about Tauri, so the tests call them directly, without a window.
They run on Tauri's blocking thread pool, because nearly everything they do blocks: SQLite, Argon2
(about half a second per password), RPC calls to bitcoind, and a first sync that can take minutes.
On the UI thread that would freeze the window. Commands that panic come back as
`{ code: "internal" }`, not as a crashed app.

### Why secrets stay in Rust
A webview is the most exposed part of a desktop app: it parses HTML, runs JavaScript, and a bug in
any npm package runs with the page's rights. So the rule from PLAN §4.2 is that Rust holds
everything that can spend:

| What | Where it lives | Does it cross into JS? |
|---|---|---|
| Master private key (`keys::Signer`) | `Session` in `AppState`, while unlocked | never |
| Recovery phrase | encrypted keystore on disk | only as the reply of `create_wallet` (backup screen) and `reveal_phrase` (the user asked, with the password) |
| Password | arrives as `secrecy::SecretString`, used once, dropped | it starts in JS (the user types it); never sent back |
| Unsigned PSBT of a payment | `Session.pending`, behind a random id | never; the UI gets the `SendPreview` and the id |
| Words typed in a backup check | wiped after the check (`zeroize`) | they start in JS |

The phrase is serialized straight from the core's wipe-on-drop buffer (`PhraseWords` writes a JSON
array from the `Zeroizing<String>`, no `Vec<String>` copy). JS strings can't be wiped, so the UI's
answer is to keep them briefly: the create screen drops the words once the check passes, "Show
recovery phrase" after a minute or when the screen closes. Nothing secret goes into a log, an error
message or a `Debug` print: errors are the core's messages (written never to contain secrets),
`Session`, `PhraseWords` and the settings (whose RPC URL may hold `user:pass@`) have redacted
`Debug`, and the command log has names and error codes only.

### One wallet open per command, one signer per session
The wallet is **not** kept open while the app runs. Each command opens it watch-only, does its work
and closes it:

```rust
fn with_wallet<T>(state: &AppState, cfg: &Config,
                  f: impl FnOnce(&mut WalletService) -> ApiResult<T>) -> ApiResult<T> {
    let gate = state.gate(cfg.network);
    let _turn = lock(&gate);                       // this process's commands take turns
    let mut wallet = api::open_watch_only(cfg)?;   // takes the wallet's lock file (§4)
    let result = f(&mut wallet);
    drop(wallet);                                  // releases it again
    result
}
```

So the wallet's lock file (§4) is held for milliseconds, and `btcw balance` in a terminal works
while the app is open. If the CLI has the wallet at that moment, the app's command fails with
`wallet_in_use`, which the UI already explains. The **gate** is needed because the lock is per
*open file*: the dashboard asks for balance, history and app info at once, and three opens from
the same process would refuse each other. One gate per network means they wait for each other
instead (a test fires eight commands at once), and a long sync on testnet4 never blocks regtest.
`app_info` doesn't wait at all: while a sync holds the wallet it answers from the height the last
progress event reported.

The **signer** is what stays. `unlock(password)` opens the wallet, calls `api::load_signer` (which
decrypts the keystore and checks the seed belongs to this wallet), keeps only the `Signer`, and
closes the wallet. `lock()` drops it, and `Signer`'s `Drop` erases the key. `create_wallet` and
`restore_wallet` keep their signer too, so a new wallet is unlocked, as the UI expects. A signer is
tied to its network: switching networks in Settings locks the app.

### Sending: the PSBT waits in Rust
```text
prepare_send(to, amountSat, feeRate)               confirm_send(id)
  locked? → "locked"                                 locked? → "locked"
  address, dust, fee rate (no node, no lock yet)     take the PSBT for id (gone either way)
  drop any earlier preview                           unknown, used or > 10 min old → tx_build
  open wallet → sync → tx::prepare_send              open wallet → tx::check_prepared (§10)
                                                     node
  persist (the change address must survive)          sign (holding the session) → tx::sign_psbt
  id = 16 random bytes (OS RNG), keep {id, PSBT}     broadcast → tx::broadcast_signed
  → { id, preview }                                  → { txid }
```

The UI never sees the transaction, so it can't change it between the preview and the signature,
and an id it made up finds nothing. There is at most **one** prepared payment: a new preview
replaces the last, a lock drops it, and it expires after 10 minutes, so a forgotten preview can't
be signed an hour later against a changed wallet. `prepare_send` persists the wallet because the
wallet is closed in between; without it, the change address BDK revealed for the preview would be
unknown to the wallet that signs. Signing is `tx::complete_send` split into its two halves: the
signature is made while holding the session, so `lock` can't wipe the key in the middle of it,
and the broadcast happens after letting go, so "Lock" never waits for a slow node.
Because the wallet is closed while the preview is on screen, `btcw send` in a terminal can spend
the same coin, or take the same revealed-but-unused change address, in between. So `confirm_send`
first runs `tx::check_prepared` on the reopened wallet: every input must still be an unspent coin,
and no change address may have been paid since; otherwise `tx_build` ("review the payment again").
Without it, a stale PSBT at a higher fee *replaced* the CLI's payment by RBF (found by the e2e
tests, §10).
`cancel_send(id)` drops the PSBT and runs `tx::cancel`. Since BDK keeps "used" marks only in memory,
a reopened wallet has nothing left to release, but `tx::cancel` still runs, so cancelling stays
correct if that ever changes.

Fee rates arrive as decimals (`2.5` sat/vB). They are converted to BDK's sat per 1000 weight units
by rounding *up* (`2.3 × 250` is `575.0000000000001` in floating point, so a tiny epsilon keeps it at
575), then go through the core's `check_fee_rate` (1 to 25 000 sat/vB) before anything is built.

`tx_status` syncs first when the node answers, so the transaction screen sees new confirmations;
when it doesn't, it answers from the last sync, like `btcw status`.

### Auto-lock in two layers
The UI locks after `auto_lock_minutes` without a key press, click or mouse move (§6). Rust has a
second timer as a backstop, in case the UI's never fires (a bug, a hung or throttled webview):
every command first checks how long it has been since the last *user* activity, and if that is
more than `auto_lock_minutes` plus one minute, it drops the signer and any prepared payment before
doing anything else.

What counts as activity matters. The transaction screen polls `tx_status` every 10 s; if polls
counted, leaving that screen open would keep the key in memory forever. So reads (`app_info`,
`balance`, `history`, `utxos`, `list_addresses`, `sync`, `tx_status`, `get_settings`) only *check*
the deadline, and only actions (unlock, prepare, send, settings, backup, …) push it back. Reading
the dashboard isn't a command at all, so the UI reports activity with `keep_alive` at most every
30 s while unlocked. The extra minute on the Rust side means the UI's timer always fires first when
it works, and says why ("Locked after 5 minutes without activity").

### Locking the window down
- **CSP** (`tauri.conf.json`): `default-src 'self'`, scripts and styles only from the app's own
  files (Vite emits no inline script, and React sets styles through the DOM, which CSP allows),
  images also as `data:` (the favicon), IPC through `ipc:`, and `object-src`, `frame-src`,
  `base-uri`, `form-action` all `'none'`. No remote origin appears anywhere.
- **No plugins**: no shell, fs, http, dialog or clipboard APIs exist for the page.
- **Capabilities**: Tauri 2 allows nothing to the webview unless a capability grants it. Ours
  (`capabilities/main-window.json`) grants `core:event:allow-listen` and `allow-unlisten` to the
  `main` window, enough for `sync-progress`. This is narrower than `core:default`, which would also
  let the page create menus and tray icons, read images from paths and query windows and monitors.
- **Navigation**: the window is built in `setup()` so it can refuse to navigate anywhere but the
  app itself (`tauri://localhost`, or the Vite dev server in development builds) and to open new
  windows. A remote page can never end up in the window that has the IPC bridge.
- `freezePrototype` is on and `withGlobalTauri` off (no `window.__TAURI__`).

### Settings: `desktop.json` on top of the CLI's config
The app reads and writes `<datadir>/desktop.json` (mode 0600, written to a temp file, fsynced and
renamed, so a crash leaves the old file or the new one, never half of one):

```json
{ "network": "regtest", "rpc_url": null, "rpc_cookie": null, "auto_lock_minutes": 5, "mainnet_opt_in": false }
```

Each command turns it into `config::Overrides` and calls `Config::load`, so a `null` falls through to
`BTCW_*`, then `btcw.toml`, then the defaults, exactly as for the CLI (§1). The data directory is
`BTCW_DATADIR` or the default (`~/.local/share/btcw`), so the app and `btcw` share wallets.
`set_settings` checks everything before saving: the network must be selectable in this build,
mainnet also needs `mainnet_opt_in` (the UI sets it after the user types MAINNET), auto-lock
1–60 minutes, an `http://` URL, and the whole configuration must still resolve. A damaged file
never stops the app from starting: unusable values are dropped (logged, without the file's
contents) and the next save replaces it. `~/` in the cookie path is expanded, as the placeholder
in the Settings screen suggests.

### The backup reminder
While `AppInfo.backup_verified` is `false`, the dashboard and the send screen show a calm, blue
reminder ("Check your recovery phrase backup") with two buttons:

- **Verify backup**: password first, then `backup_challenge` returns three positions. It takes the
  password because only the encrypted keystore knows how long the phrase is (12- and 24-word
  phrases overlap in length, and restored ones can have 15, 18 or 21 words), and because a wrong
  password then fails *before* the user types any words, as in `btcw backup verify`. The words are
  typed into password-style fields with a "Show the words as I type" box. A mismatch names the
  position ("Word #7 doesn't match your recovery phrase. Check your paper copy…"), never a word.
  Success marks the wallet verified and the reminder disappears.
- **Show the words again**: the same component as Settings → **Show recovery phrase**: password,
  then the words decrypted but still hidden until "Reveal", in the numbered grid with the same
  warning as at creation. They are dropped after a minute, on "Hide", or when the screen or dialog
  closes, and can't be selected or copied.

The creation screen no longer says btcw will never show the words again. Its own three-word check
now also calls `verify_backup` with the same answers, so a new wallet whose owner just passed the
check isn't reminded to do it again. That needs the password one more time: the create screen keeps
it in a ref (never rendered) for exactly as long as it already holds the phrase, and drops both
together. If recording fails (say the CLI has the wallet open), the user is told and the reminder
stays.

### Errors across the bridge
Every failure reaches JS as `{ code, message }`: `WalletError::code()` and its `Display` text, or one
of the bridge's own codes, `locked` (sending without the signer) and `internal` (a panic).
`errors.ts` gained `backup_mismatch` and `internal`. Because a prepared payment is gone after any
failed `confirm_send`, the send screen now goes back to the form, with what was typed still there.

### Testing
- **Rust, offline** (21 tests in `btcw-desktop`; `commands_tests.rs` uses a temp datadir and node settings pointing nowhere): create →
  `app_info` → lock → wrong password → unlock; the CLI can open the wallet while the app is unlocked,
  and `app_info` still answers while it does; eight concurrent commands all succeed; restore is
  verified; backup challenge, mismatch (positions only), wrong password, success, reveal; settings
  validation, the 0600 file, `desktop.json` beating `BTCW_NETWORK`, switching networks locks and
  drops the payment; `locked` on prepare and confirm, input refused before the node, unknown ids,
  "removed either way", expiry after 10 minutes; the auto-lock backstop (polls don't count,
  `keep_alive` does); fee-rate conversion, the progress throttle, ids; settings files and the
  navigation filter.
- **Rust, regtest** (`send_flow_against_a_regtest_node`): create (birthday from the node) → fund →
  sync with progress events → prepare → cancel → prepare twice (the first id dies) → confirm (in the
  node's mempool, the id can't be reused) → `tx_status` unconfirmed → mine → confirmed, balance and
  history agree. Skipped only when no bitcoind is available; it fails if `BITCOIND_EXE` is set but
  broken.
- **UI** (Vitest, 68 tests in all, 12 new): the reminder on the dashboard and send screens and not for a restored wallet;
  verify with a wrong password, a mismatch and success; reveal with a wrong password, hidden until
  "Reveal", not copyable, gone after leaving the screen and after a minute; creation records the
  check (or says why not); a failed send returns to the form; `keep_alive` is throttled; the mock's
  new rules.

### Try it
```bash
cd apps/desktop && npm ci
npm run dev:mock                                 # the UI with the in-memory backend, in a browser
npm run tauri dev                                # the real app (Vite + Rust, hot reload)
npm run tauri build -- --debug --no-bundle       # target/debug/btcw-desktop, assets embedded
```

The built app against a local regtest node, in a throwaway datadir (from the repo root):

```bash
scripts/regtest.sh start && eval "$(scripts/regtest.sh env)"
BTCW_DATADIR=$(mktemp -d) RUST_LOG=btcw_desktop=debug target/debug/btcw-desktop
scripts/regtest.sh reset

BITCOIND_EXE=~/.local/bin/bitcoind cargo test -p btcw-desktop   # the bridge's tests
```

## 10. End-to-end tests, Phase 2 / Agent H

**What we built:** tests that use the wallet the way a person does: many commands in a row, each
one a new process, two wallets, two frontends on one wallet, a reorg, a double-spend. Every other
test in the repo checks one piece. These check that the pieces agree with each other and with
Bitcoin Core, and they found two real bugs.

```text
crates/btcw-cli/tests/e2e.rs                   3 journeys through the real `btcw` binary,
                                               each against its own regtest bitcoind
apps/desktop/src-tauri/src/commands_tests.rs   2 journeys: the desktop commands on a wallet the
                                               CLI created, and a payment prepared in the app
                                               while the CLI spends
```

### What a journey checks that a unit test can't
- **Agreement, not just output.** The wallet's fee is the fee in Core's mempool entry
  (`getmempoolentry`). Its outputs are the ones Core decodes (`getrawtransaction`). The change
  address is `m/84'/1'/0'/1/0` derived from the phrase by a separate in-memory BDK wallet. The
  balance is received − sent − fee to the satoshi. Two wallets agree on every txid, block and
  amount between them.
- **Persistence on every line.** Each `btcw` call is a new process that opens the SQLite file,
  so R11 is exercised by every step. To prove the numbers come from the database and not the
  node, the views are run once more with `--rpc-url http://127.0.0.1:1`, where nothing listens:
  the JSON is identical.
- **Boundaries between processes.** Two datadirs, two wallets, one chain. A `btcw send` paused at
  its `Send? [y/N]` prompt in a pseudo-terminal holds the wallet lock, and a second `btcw` gets
  `wallet_in_use`. The test process also holds the wallet through the core API, the way the desktop
  app does for each command.

### The journeys

| Journey | What happens | PRD |
|---|---|---|
| **Money cycle, restart, restore** (`money_cycle_survives_restarts_and_restores_exactly`) | `create` (birthday = tip; address #0 = the BIP84 derivation) → `backup verify` with the phrase on stdin, after which the reminder stops → `address new` (the same address until it's paid) → the faucet pays → `sync`: unconfirmed, 1 mempool tx → mine → 1 confirmation, then 2 → `send --yes`: same fee as Core, change to `…/1/0` → `status` unconfirmed → mine 2 → `status --watch --until 2` → `history`: exact net, fee, sent and received for both txs → `balance` = 1 000 000 − 250 000 − fee → `utxos` = only the change, `internal` #0, not a receive address → every view the same in a new process and with no node → `restore --birthday <height before the payment>` into a new datadir and `restore` from genesis: identical balance, history (txids, net, fee, status, block time), coins and used flags → all three wallets hand out #1 next | R1–R11 |
| **Two wallets, two processes** (`two_wallets_pay_each_other_and_lock_out_a_second_process`) | Alice pays Bob twice, Bob pays some back. Bob sees the first payment unconfirmed, then confirmed. Both histories agree on txid and block, and the two nets of each tx add up to minus the payer's fee. Balances are exact and equal the sum of the coins. The three change addresses are distinct, never a receive address of either wallet, and Alice's second payment moves her change from internal #0 to #1. Then the lock: a `send` waiting at its prompt makes `balance` (CLI) and `open_watch_only` (core) fail with `wallet_in_use`, while Alice's wallet stays usable. With the test holding Bob's wallet, `balance`, `history`, `address new`, `sync` and `send` all fail with `wallet_in_use` and the exact message, while `status --watch` retries ("trying again in 1 s") and finishes once the wallet is free. | R4–R10, PLAN §4.2 lock |
| **Reorgs and the mempool cache** (`reorgs_and_the_mempool_cache_through_the_cli`) | A birthday above the tip: a wallet whose birthday block is invalidated before its first sync (and again after it scanned the replacement), and a `restore --birthday <tip+1>`. Both sync fine, watch the mempool while they wait, and scan once the chain gets there (bug 1). Incoming reorg: confirmed → `invalidateblock` → `sync` warns "shorter than the wallet's" → `generateblock` with no transactions → `sync`: 1 block, the payment unconfirmed, balance moved to unconfirmed → mine → confirmed one block higher. Outgoing reorg: our payment un-confirms, the spent coin stays spent, the change is unconfirmed, then it re-confirms. Mempool cache: a proxy counts `getrawtransaction` calls (below). 5 new mempool txs → 5 downloads; the next sync → **0**, and `mempool-cache.txt` holds exactly the node's mempool. Strangers' txs never reach the history or the database. Then both coins are double-spent at a higher fee: a stranger's tx is replaced, and so is a payment *to* the wallet. The next sync downloads only the 2 replacements, and the evicted payment disappears from history, balance and `status` (`tx_not_found`). | R5, R6, R7, R10 |
| **Desktop on a CLI wallet** (`desktop_and_cli_share_one_wallet_end_to_end`) | The CLI side (the `btcw_core` calls `btcw create` makes) creates the wallet in the app's datadir → the app finds it locked and unverified, with the CLI's address → fund, `sync` from the CLI's birthday → `backup_challenge` + `verify_backup` with the CLI's phrase, and the CLI's flag reads verified too → `prepare_send` fails `locked`, wrong password, unlock, lock, `locked` again, unlock → `prepare_send` (the CLI can open the wallet meanwhile) → `confirm_send` → `tx_status` unconfirmed, and the CLI's `history`/`status` see it with the same fee before any sync → mine → 1 confirmation → the CLI sends from the same wallet while the app stays unlocked (the app gets `wallet_in_use` while the CLI holds it) → the app's history and balance include the CLI's payment | R1, R4–R11, shared datadir |
| **Stale preview** (`a_payment_prepared_in_the_app_is_refused_after_the_cli_spent_its_coin`) | The app prepares a payment; the CLI spends the only coin; the app's `confirm_send` is refused (`tx_build`, "review the payment again"); the CLI's payment is still the one in the mempool; a new review goes through (bug 2) | R8, R9 |

The CLI journeys use `--json` for assertions and integer satoshis throughout: no float parsing of
our own amounts. Core's amounts (BTC with 8 decimals) go through `Amount::from_btc`.

### Counting downloads instead of timing them
"The second sync doesn't download the mempool again" could be tested with a stopwatch, but a
stopwatch is flaky on a loaded CI machine, and on a local node the difference is milliseconds (we
measured about 85 ms with 5 new transactions and 50 ms with 5 cached). So the test puts a
20-line TCP proxy between `btcw` and bitcoind. It forwards bytes both ways and counts how often
`"getrawtransaction"` appears in the requests. The Emitter makes one such call per mempool
transaction it doesn't already hold, so the count is exactly "what this sync downloaded", whatever
the timing. The needle can be split across two reads, so the proxy keeps the last 18 bytes of
each chunk.

Deterministic double-spends need coins nobody else touches. The test sends two 0.05 BTC coins to
the faucet, `lockunspent`s them so Core's own `sendtoaddress` calls can't pick them, and spends
them with `createrawtransaction` / `signrawtransactionwithwallet` / `sendrawtransaction`. Spending
the same outpoint again at a higher fee is a replacement (Core 28+ allows full RBF).

### Two things we learned that aren't bugs
- **A payee often knows the fee.** The fee is inputs − outputs, so whoever holds the transactions
  the inputs came from can work it out. Bob doesn't know the fee of Alice's first payment (it spends
  her funding coin, which Bob never saw). He does know the fee of her second payment, which spends
  her change from the first, and Bob has that transaction. The test now asserts exactly that.
- **BDK keeps a conflicting stranger.** When a payment to us is double-spent away, BDK stores the
  replacement in the wallet database even though it pays someone else
  (`is_tx_or_conflict_relevant`). It's how the wallet knows its payment lost. It never appears in
  history or the balance. Unrelated strangers are never stored.

### Bugs found and fixed
1. **`sync` failed when the birthday was above the node's tip** (`chain.rs`). The Emitter jumps
   to the birthday by *height* (`getblockhash(start_height)`), and Core answers
   `Block height out of range` when that height doesn't exist yet. This happens when a new
   wallet's birthday block (the tip at `create`) is reorged away, before or after the wallet
   scanned it, while the node has nothing at that height yet, when
   `restore --birthday` gets a height the chain hasn't reached, or when the node is still catching
   up after switching nodes. Every `sync` and `send` failed, and `restore` and `status` could
   only report "not synced", until the chain grew past it. The fix: when the birthday is above the node's tip, skip the block scan
   (log a warning), still apply the mempool, and scan on a later sync; a birthday block that comes
   back as a different block is then an ordinary reorg. The regression is the first part of the
   reorg journey (both cases); it failed with
   `rpc: fetching the next block: Block height out of range (code -8)` before the fix.
2. **The desktop app could sign a stale payment and replace a CLI payment** (`tx.rs`,
   `commands.rs`). `prepare_send` keeps the PSBT and *closes* the wallet (by design, so `btcw`
   keeps working). If `btcw send` spent the same coin before the user pressed Send, `confirm_send`
   signed and broadcast the old PSBT anyway. At a higher fee rate, Core accepted it as an RBF
   replacement, and the CLI's payment, already reported as sent, vanished from the mempool. Two
   payments approved, one made. The same gap let both payments share one change address when
   they used different coins. The fix is `tx::check_prepared(wallet, psbt)`, which `confirm_send`
   runs on the reopened wallet before signing. Every input must still be an unspent wallet coin
   (`get_utxo`), and no change output may pay a script the wallet has already seen paid
   (`list_output`). Otherwise the result is `tx_build` with "review the payment again", and the
   change address is released. The reopened wallet knows the CLI's payment without a sync,
   because `send` records its broadcast in the shared database (§7). Tests: the desktop regtest
   journey above (it failed with the CLI's payment replaced before the fix), and two offline unit
   tests in `tx.rs`, one for each branch.

### Open issues (not fixed here)
- **A coin spent from another copy of the wallet** (the same phrase restored on a second
  computer) isn't caught by `check_prepared`: the shared database never hears about it, and
  `confirm_send` doesn't sync before signing. A sync there would close the gap at the cost of one
  more node round trip per send.
- **A reorg with no competing block yet** leaves transactions confirmed until the node has a
  block at that height again (the known limitation in §3). The reorg journey only checks the
  warning.

### Running them
```bash
BITCOIND_EXE=~/.local/bin/bitcoind cargo test -p btcw-cli --test e2e          # the 3 CLI journeys
BITCOIND_EXE=~/.local/bin/bitcoind cargo test -p btcw-desktop end_to_end       # desktop on a CLI wallet
BITCOIND_EXE=~/.local/bin/bitcoind cargo test -p btcw-desktop a_payment_prepared
cargo test -p btcw-core --lib a_prepared_payment                               # offline, bug 2
```
Without bitcoind they print `skipping: no bitcoind` and pass. With `BITCOIND_EXE` set but broken,
they fail instead, so a broken setup can't pass as green. Each test starts its own node: about 20 s
to start it and mine 101 blocks, and the three CLI journeys run in parallel. On this machine (debug
build) the e2e file takes **44–51 s** (once 84 s on a busy machine), the desktop crate's 23 tests
**38–48 s**, and the whole workspace (`cargo test --workspace`, 141 tests, all node tests running)
**5 min 16 s**. The mempool-cache journey also prints the timings the proxy makes unnecessary to
assert: about 85 ms for a sync with 5 new mempool transactions and 50 ms with all 5 cached.
Everything is cleaned up even when an assertion fails. Datadirs are `TempDir`s, `TestNode` stops
its bitcoind on drop, and a spawned `btcw` (the watcher, the prompt) is wrapped in a guard that
kills it on drop.

## 11. Running the demo (regtest and testnet4) — _pending_
## 12. Lessons learned — _pending_
