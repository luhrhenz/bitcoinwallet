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

pub mod address;
pub mod create;
pub mod mine;
pub mod restore;
pub mod sync;
pub mod view;
