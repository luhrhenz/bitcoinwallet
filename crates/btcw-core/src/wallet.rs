//! [`WalletService`]: a BDK wallet persisted to SQLite, plus read-only views of its state.
//!
//! OWNER: Agent C (Phase 1). Contract: PLAN §4, §5.3–5.7.
//!
//! - One wallet per `<datadir>/<network>/wallet.sqlite`.
//! - While open, holds an exclusive `File::try_lock` on `cfg.lock_path()` so the CLI and the
//!   desktop app can't write the same wallet at once (`WalletInUse`).
//! - The wallet holds **public descriptors only**; signing lives in `keys::Signer`. So `open`
//!   needs no password: it loads descriptors from the database, checks the network
//!   (`.check_network(..)`), and, if `expected` is given, checks they match
//!   (`.descriptor(keychain, Some(..))`).
//! - Stores a *birthday height* in its own SQLite table (`btcw_meta`, same file). `Node::sync`
//!   starts scanning there, so a new wallet never rescans the whole chain.
//! - Confirmation counts come from the wallet's own latest checkpoint (the synced tip),
//!   so the views work offline.
#![allow(unused_variables, dead_code)] // remove once implemented

use bdk_wallet::PersistedWallet;
use bdk_wallet::rusqlite::Connection;

use crate::bitcoin::Network;
use crate::config::Config;
use crate::error::Result;
use crate::keys::Descriptors;
use crate::types::{AddressRow, BalanceView, TxRow, UtxoRow};

/// Addresses scanned beyond the last used one (BIP44 gap limit is 20).
pub const LOOKAHEAD: u32 = 25;

pub struct WalletService {
    wallet: PersistedWallet<Connection>,
    db: Connection,
    network: Network,
    _lock: std::fs::File,
}

impl std::fmt::Debug for WalletService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalletService")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

impl WalletService {
    /// Create a brand-new wallet database. `WalletExists` if one is already there.
    /// `birthday`: first block height that can contain our transactions (`None` = genesis).
    pub fn create(cfg: &Config, descriptors: &Descriptors, birthday: Option<u32>) -> Result<Self> {
        todo!("Agent C")
    }

    /// Open an existing wallet database. `WalletNotFound` if missing; `NetworkMismatch` if it
    /// belongs to another network; `Persist` if `expected` descriptors don't match.
    pub fn open(cfg: &Config, expected: Option<&Descriptors>) -> Result<Self> {
        todo!("Agent C")
    }

    pub fn network(&self) -> Network {
        self.network
    }

    /// Height sync starts from on a fresh wallet (0 = genesis).
    pub fn birthday_height(&self) -> u32 {
        todo!("Agent C")
    }

    /// Height of the wallet's latest checkpoint (0 before the first sync).
    pub fn synced_height(&self) -> u32 {
        todo!("Agent C")
    }

    /// Next unused receive address (reuses a revealed-but-unused one). Persists the reveal.
    pub fn new_address(&mut self) -> Result<AddressRow> {
        todo!("Agent C")
    }

    /// All revealed receive addresses, ascending index, with `used` flags.
    pub fn addresses(&self) -> Vec<AddressRow> {
        todo!("Agent C")
    }

    pub fn balance(&self) -> BalanceView {
        todo!("Agent C")
    }

    /// Wallet transactions: unconfirmed first, then confirmed by height descending.
    pub fn history(&self) -> Vec<TxRow> {
        todo!("Agent C")
    }

    pub fn utxos(&self) -> Vec<UtxoRow> {
        todo!("Agent C")
    }

    /// Write staged changes to SQLite.
    pub fn persist(&mut self) -> Result<()> {
        todo!("Agent C")
    }

    /// Escape hatches for `chain` and `tx`. Not public API.
    pub(crate) fn bdk(&self) -> &PersistedWallet<Connection> {
        &self.wallet
    }

    pub(crate) fn bdk_mut(&mut self) -> &mut PersistedWallet<Connection> {
        &mut self.wallet
    }
}
