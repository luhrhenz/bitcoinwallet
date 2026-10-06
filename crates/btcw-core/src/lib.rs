//! `btcw-core`: the wallet engine shared by the `btcw` CLI and the desktop app.
//!
//! Modules:
//! - [`config`]   network policy, data directories, node RPC settings
//! - [`keys`]     BIP39 mnemonics → BIP84 descriptors
//! - [`keystore`] mnemonic encrypted at rest (Argon2id + XChaCha20-Poly1305)
//! - [`wallet`]   [`wallet::WalletService`]: BDK wallet + SQLite persistence
//! - [`book`]     the address book (contacts) and transaction labels, stored next to the wallet
//! - [`chain`]    [`chain::Node`]: Bitcoin Core RPC (sync, broadcast, fees, tip)
//! - [`tx`]       build → sign → extract a PSBT, fee bumps (RBF), and transaction status
//! - [`api`]      high-level flows (create / restore / unlock) used by both frontends
//! - [`types`]    serializable views returned to the CLI (`--json`) and the UI
//!
//! Use `bitcoin` through the re-export below so the workspace agrees on one version.

pub use bdk_wallet;
pub use bdk_wallet::bitcoin;

pub mod api;
pub mod book;
pub mod chain;
pub mod config;
pub mod error;
pub mod keys;
pub mod keystore;
pub mod tx;
pub mod types;
pub mod wallet;

#[cfg(feature = "test-utils")]
pub mod testnode;

pub use error::{Result, WalletError};
