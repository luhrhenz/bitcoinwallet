//! `btcw`: terminal interface over `btcw-core`.
//!
//! OWNER: Agent D (Phase 1; `send`/`status` by Agent F in Phase 2). Contract: PLAN §4 "CLI surface".
//! The clap surface below is the contract; keep flag names stable (the demo script and e2e
//! tests use them). Human-readable output by default, `--json` prints `btcw_core::types`
//! values (errors as `{"error": {"code", "message"}}` with a non-zero exit).
//!
//! Passwords: prompted with `rpassword`. `BTCW_PASSWORD` is honoured for scripted demos and
//! tests only (documented as insecure).

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "btcw",
    version,
    about = "A non-custodial Bitcoin wallet (testnet4 / signet / regtest)"
)]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Args)]
struct GlobalArgs {
    /// testnet4 (default), signet or regtest
    #[arg(long, global = true, env = "BTCW_NETWORK")]
    network: Option<String>,

    /// Wallet data directory [default: ~/.local/share/btcw]
    #[arg(long, global = true, env = "BTCW_DATADIR")]
    datadir: Option<PathBuf>,

    /// Bitcoin Core RPC URL [default: http://127.0.0.1:<network port>]
    #[arg(long, global = true)]
    rpc_url: Option<String>,

    /// Bitcoin Core cookie file [default: ~/.bitcoin/<network>/.cookie]
    #[arg(long, global = true)]
    rpc_cookie: Option<PathBuf>,

    /// Machine-readable JSON output
    #[arg(long, global = true)]
    json: bool,

    /// Opt in to mainnet (only has an effect in builds with `--features mainnet`)
    #[arg(long, global = true, hide = !cfg!(feature = "mainnet"))]
    i_understand_mainnet_risk: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a new wallet and show its recovery phrase once
    Create {
        /// 12 or 24 words
        #[arg(long, default_value = "12", value_parser = ["12", "24"])]
        words: String,
    },
    /// Restore a wallet from a recovery phrase (prompted, hidden)
    Restore {
        /// Block height to start scanning from; omit to scan from genesis
        #[arg(long)]
        birthday: Option<u32>,
    },
    /// Receive addresses
    #[command(subcommand)]
    Address(AddressCmd),
    /// Pull new blocks and mempool transactions from the node
    Sync,
    /// Confirmed / unconfirmed / immature balance
    Balance,
    /// Transaction history with status and confirmations
    History,
    /// Spendable outputs
    Utxos,
    /// Build, sign and broadcast a payment
    Send {
        /// Destination address (must match --network)
        #[arg(long)]
        to: String,
        /// Amount in satoshis
        #[arg(long)]
        amount: u64,
        /// Fee rate in sat/vB [default: node estimate for 6 blocks]
        #[arg(long)]
        fee_rate: Option<u64>,
        /// Skip the confirmation prompt
        #[arg(long, short)]
        yes: bool,
        /// Also write the unsigned PSBT (base64) to this file
        #[arg(long)]
        psbt_out: Option<PathBuf>,
    },
    /// Show a transaction's status; --watch polls until confirmed
    Status {
        txid: String,
        #[arg(long)]
        watch: bool,
        /// Seconds between polls
        #[arg(long, default_value_t = 10)]
        interval: u64,
        /// Stop watching after this many confirmations
        #[arg(long, default_value_t = 1)]
        until: u32,
    },
    /// Regtest only: mine blocks (to --to, or to a fresh wallet address)
    Mine {
        blocks: u64,
        #[arg(long)]
        to: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum AddressCmd {
    /// Next unused receive address
    New,
    /// All revealed receive addresses with used/unused state
    List,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let _ = (&cli.global, &cli.command);
    bail!("not implemented yet (Agent D)")
}
