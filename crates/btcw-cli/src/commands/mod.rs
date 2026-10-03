//! One module per command family. Every command follows the same shape:
//! open the wallet through `btcw_core::api` (watch-only unless it needs keys), call the core,
//! drop the wallet (releasing its lock), then render either human text or one JSON value.
//!
//! | Command | Wallet access | Needs a node |
//! |---|---|---|
//! | `create`, `restore` | creates it (new password) | optional (birthday / first sync) |
//! | `address new/list`, `balance`, `history`, `utxos` | watch-only, no password | no |
//! | `sync` | watch-only, no password | yes |
//! | `mine` | watch-only (only without `--to`) | yes, regtest |
//! | `send` | watch-only, then the password **after** the user confirms (signer dropped right after signing) | yes |
//! | `status` | watch-only, reopened for every poll | yes (falls back to the last sync) |
//! | `backup verify` | watch-only + password (decrypts the phrase to compare) | no |
//! | `backup show` | password only (reads the encrypted phrase, not the wallet) | no |

pub mod address;
pub mod backup;
pub mod create;
pub mod mine;
pub mod restore;
pub mod send;
pub mod status;
pub mod sync;
pub mod view;
