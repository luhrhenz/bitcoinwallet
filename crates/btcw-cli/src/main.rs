//! `btcw`: terminal interface over `btcw-core`.
//!
//! OWNER: Agent D (Phase 1; `send`/`status` by Agent F in Phase 2). Contract: PLAN §4 "CLI surface".
//! The clap surface below is the contract; keep flag names stable (the demo script and e2e
//! tests use them). Human-readable output by default, `--json` prints `btcw_core::types`
//! values (errors as `{"error": {"code", "message"}}` with a non-zero exit).
//!
//! This crate holds no wallet logic. Each command opens the wallet through `btcw_core::api`,
//! calls one or two core functions and renders the result:
//! - [`commands`]: one module per command family.
//! - [`output`]: the network badge, colors, amounts, tables, JSON and error rendering.
//! - [`prompt`]: passwords and the recovery phrase (hidden TTY prompts, stdin, `BTCW_PASSWORD`).
//!
//! Passwords: prompted with `rpassword`. `BTCW_PASSWORD` is honoured for scripted demos and
//! tests only (documented as insecure).

mod commands;
mod output;
mod prompt;

use std::convert::Infallible;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Result, bail};
use btcw_core::config::{Config, Overrides};
use btcw_core::keys::WordCount;
use clap::{Args, Parser, Subcommand};
use secrecy::SecretString;

use crate::output::Ui;

const AFTER_HELP: &str = "\
Environment:
  BTCW_NETWORK, BTCW_DATADIR, BTCW_RPC_URL, BTCW_RPC_COOKIE, BTCW_RPC_USER, BTCW_RPC_PASS
                   same as the flags; a flag beats its variable, which beats <datadir>/btcw.toml
  BTCW_PASSWORD    wallet password for scripts and tests. INSECURE: the environment leaks
                   (shell history, child processes, /proc, crash reports); people should
                   type the password at the prompt instead
  NO_COLOR         disable colors
  RUST_LOG         log level on stderr (default: warn)

Exit status: 0 success, 1 error, 2 invalid command line.
With --json, stdout holds exactly one JSON value; errors are {\"error\":{\"code\",\"message\"}}.";

#[derive(Debug, Parser)]
#[command(
    name = "btcw",
    version,
    about = "A non-custodial Bitcoin wallet (testnet4 / signet / regtest)",
    after_help = AFTER_HELP
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

    /// Bitcoin Core RPC user (with --rpc-pass; instead of the cookie file)
    #[arg(long, global = true)]
    rpc_user: Option<String>,

    /// Bitcoin Core RPC password. Other local users can see it in `ps`; prefer the cookie
    /// file or BTCW_RPC_PASS
    #[arg(long, global = true, value_parser = parse_secret)]
    rpc_pass: Option<SecretString>,

    /// Machine-readable JSON output
    #[arg(long, global = true)]
    json: bool,

    /// Opt in to mainnet (only has an effect in builds with `--features mainnet`)
    #[arg(long, global = true, hide = !cfg!(feature = "mainnet"))]
    i_understand_mainnet_risk: bool,
}

impl GlobalArgs {
    fn into_overrides(self) -> Overrides {
        Overrides {
            network: self.network,
            datadir: self.datadir,
            rpc_url: self.rpc_url,
            rpc_user: self.rpc_user,
            rpc_pass: self.rpc_pass,
            rpc_cookie: self.rpc_cookie,
            mainnet_opt_in: self.i_understand_mainnet_risk,
        }
    }
}

/// `SecretString` redacts itself in `Debug`, so the derived `Debug` above can't leak it.
fn parse_secret(s: &str) -> Result<SecretString, Infallible> {
    Ok(SecretString::from(s))
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a new wallet and show its recovery phrase once
    Create {
        /// 12 or 24 words
        #[arg(long, default_value = "12", value_parser = ["12", "24"])]
        words: String,
    },
    /// Restore a wallet from a recovery phrase (prompted, hidden; or the first line of stdin)
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
    /// Confirmed / unconfirmed / immature balance (offline, as of the last sync)
    Balance,
    /// Transaction history with status and confirmations (offline, as of the last sync)
    History,
    /// Spendable outputs (offline, as of the last sync)
    Utxos,
    /// Build, sign and broadcast a payment (asks for the wallet password)
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
        /// Skip the confirmation prompt (required with --json)
        #[arg(long, short)]
        yes: bool,
        /// Also write the unsigned PSBT (base64) to this new file
        #[arg(long)]
        psbt_out: Option<PathBuf>,
    },
    /// Show a transaction's status; --watch polls until confirmed
    Status {
        txid: String,
        #[arg(long)]
        watch: bool,
        /// With --watch: seconds between polls
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..))]
        interval: u64,
        /// With --watch: stop once the transaction has this many confirmations
        #[arg(long, default_value_t = 1)]
        until: u32,
    },
    /// Regtest only: mine blocks (to --to, or to a fresh wallet address)
    Mine {
        /// Number of blocks to mine
        #[arg(value_parser = clap::value_parser!(u64).range(1..))]
        blocks: u64,
        /// Address that receives the block rewards [default: the wallet's next receive address]
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

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => return output::usage_error(&e),
    };
    let ui = Ui::new(cli.global.json);
    init_logging(&ui);
    match run(cli, &ui) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            ui.report_error(&e);
            ExitCode::FAILURE
        }
    }
}

/// Core warnings (fee fallback, node still syncing, shortened chain) go to stderr at `warn`, so
/// they are visible but never mix with stdout or `--json`. `RUST_LOG` overrides the level.
fn init_logging(ui: &Ui) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    // `try_init` only fails if a subscriber is already installed, which can't happen here.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(ui.err.enabled())
        .with_target(false)
        .without_time()
        .try_init();
}

fn run(cli: Cli, ui: &Ui) -> Result<()> {
    let Cli { global, command } = cli;
    let cfg = Config::load(global.into_overrides())?;
    match command {
        Command::Create { words } => commands::create::run(&cfg, ui, word_count(&words)?),
        Command::Restore { birthday } => commands::restore::run(&cfg, ui, birthday),
        Command::Address(AddressCmd::New) => commands::address::new(&cfg, ui),
        Command::Address(AddressCmd::List) => commands::address::list(&cfg, ui),
        Command::Sync => commands::sync::run(&cfg, ui),
        Command::Balance => commands::view::balance(&cfg, ui),
        Command::History => commands::view::history(&cfg, ui),
        Command::Utxos => commands::view::utxos(&cfg, ui),
        Command::Mine { blocks, to } => commands::mine::run(&cfg, ui, blocks, to.as_deref()),
        Command::Send {
            to,
            amount,
            fee_rate,
            yes,
            psbt_out,
        } => commands::send::run(
            &cfg,
            ui,
            &commands::send::Request {
                to: &to,
                amount_sat: amount,
                fee_rate_sat_vb: fee_rate,
                yes,
                psbt_out: psbt_out.as_deref(),
            },
        ),
        Command::Status {
            txid,
            watch,
            interval,
            until,
        } => {
            let watch = watch.then_some(commands::status::Watch {
                interval: Duration::from_secs(interval),
                until,
            });
            commands::status::run(&cfg, ui, &txid, watch)
        }
    }
}

/// clap already restricts `--words` to "12" or "24".
fn word_count(words: &str) -> Result<WordCount> {
    match words {
        "12" => Ok(WordCount::Words12),
        "24" => Ok(WordCount::Words24),
        other => bail!("--words must be 12 or 24, not {other}"),
    }
}
