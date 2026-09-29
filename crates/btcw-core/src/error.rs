//! One typed error for the whole library.
//!
//! Rule: error messages must never contain secret material (mnemonic words, xprvs,
//! passwords). Wrap foreign errors as strings only after checking they can't leak secrets.

use std::path::PathBuf;

use crate::bitcoin::{Amount, Network};

pub type Result<T> = std::result::Result<T, WalletError>;

#[derive(Debug, thiserror::Error)]
pub enum WalletError {
    #[error("invalid mnemonic: {0}")]
    InvalidMnemonic(String),

    #[error("wrong password or corrupted keystore")]
    WrongPassword,

    #[error("a wallet already exists in {}", .0.display())]
    WalletExists(PathBuf),

    #[error("no wallet found in {}; create or restore one first", .0.display())]
    WalletNotFound(PathBuf),

    #[error("wallet is open in another btcw process")]
    WalletInUse,

    #[error("network mismatch: expected {expected}, found {found}")]
    NetworkMismatch { expected: Network, found: String },

    #[error("mainnet is disabled; build with `--features mainnet` and opt in explicitly")]
    MainnetDisabled,

    #[error("invalid address: {0}")]
    InvalidAddress(String),

    #[error("amount {0} is below the dust limit")]
    DustAmount(Amount),

    #[error("insufficient funds: need {needed}, available {available}")]
    InsufficientFunds { needed: Amount, available: Amount },

    #[error("could not build transaction: {0}")]
    TxBuild(String),

    #[error("signing failed: {0}")]
    Sign(String),

    #[error("transaction not found: {0}")]
    TxNotFound(String),

    #[error("bitcoin node RPC error: {0}")]
    Rpc(String),

    #[error("wallet database error: {0}")]
    Persist(String),

    #[error("keystore error: {0}")]
    Keystore(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl WalletError {
    /// Stable machine-readable code, used by `--json` output and the desktop UI.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidMnemonic(_) => "invalid_mnemonic",
            Self::WrongPassword => "wrong_password",
            Self::WalletExists(_) => "wallet_exists",
            Self::WalletNotFound(_) => "wallet_not_found",
            Self::WalletInUse => "wallet_in_use",
            Self::NetworkMismatch { .. } => "network_mismatch",
            Self::MainnetDisabled => "mainnet_disabled",
            Self::InvalidAddress(_) => "invalid_address",
            Self::DustAmount(_) => "dust_amount",
            Self::InsufficientFunds { .. } => "insufficient_funds",
            Self::TxBuild(_) => "tx_build",
            Self::Sign(_) => "sign",
            Self::TxNotFound(_) => "tx_not_found",
            Self::Rpc(_) => "rpc",
            Self::Persist(_) => "persist",
            Self::Keystore(_) => "keystore",
            Self::Config(_) => "config",
            Self::Io(_) => "io",
        }
    }
}
