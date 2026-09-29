//! `btcw-core`: the wallet engine shared by the `btcw` CLI and the desktop app.
//!
//! Layout (see `docs/PLAN.md` §4):
//! - [`config`]   network policy, data directories, node RPC settings
//! - [`keys`]     BIP39 mnemonics → BIP84 descriptors
//! - [`keystore`] mnemonic encrypted at rest (Argon2id + XChaCha20-Poly1305)
//! - [`wallet`]   [`wallet::WalletService`]: BDK wallet + SQLite persistence
//! - [`chain`]    [`chain::Node`]: Bitcoin Core RPC (sync, broadcast, fees, tip)
//! - [`tx`]       build → sign → extract a PSBT, and transaction status
//! - [`api`]      high-level flows (create / restore / unlock) used by both frontends
//! - [`types`]    serializable views returned to the CLI (`--json`) and the UI
//!
//! Rust-bitcoin is always used through the re-export below, so every crate in the
//! workspace agrees on one `bitcoin` version.

pub use bdk_wallet;
pub use bdk_wallet::bitcoin;

pub mod api;
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
