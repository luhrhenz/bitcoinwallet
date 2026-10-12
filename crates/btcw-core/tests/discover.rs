//! Birthday lookup against a fake Esplora server (plus one live check, `--ignored`).

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use btcw_core::WalletError;
use btcw_core::bdk_wallet::{KeychainKind, Wallet};
use btcw_core::bitcoin::Network;
use btcw_core::discover::{GAP_LIMIT, find_birthday};
use btcw_core::keys::{self, Descriptors};

const ABANDON: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const TIP: u32 = 160_000;

fn descriptors() -> Descriptors {
    let mnemonic = keys::parse_mnemonic(ABANDON).unwrap();
    keys::derive_account(&mnemonic, "", Network::Testnet4, 0)
        .unwrap()
        .0
}

fn address(keychain: KeychainKind, index: u32) -> String {
    let d = descriptors();
    let wallet = Wallet::create(d.external().to_owned(), d.internal().to_owned())
        .network(Network::Testnet4)
        .create_wallet_no_persist()
        .unwrap();
    wallet.peek_address(keychain, index).address.to_string()
}

/// A tiny Esplora: per-address confirmed heights (newest first, as the real one pages them) and
/// unconfirmed counts, a tip, and optionally a few 429s first.
struct FakeEsplora {
    url: String,
    hits: Arc<Mutex<Vec<String>>>,
    _thread: JoinHandle<()>,
}

impl FakeEsplora {
    fn start(
        history: HashMap<String, Vec<u32>>,
        mempool: HashMap<String, u64>,
        rate_limits: u32,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api", listener.local_addr().unwrap());
        let hits = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&hits);
        let thread = std::thread::spawn(move || {
            let mut limited = 0;
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_owned();
                seen.lock().unwrap().push(path.clone());
                let (status, body) = if limited < rate_limits {
                    limited += 1;
                    (429, "slow down".to_owned())
                } else {
                    respond(&path, &history, &mempool)
                };
                let mut stream = reader.into_inner();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self {
            url,
            hits,
            _thread: thread,
        }
    }

    fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
}

fn respond(
    path: &str,
    history: &HashMap<String, Vec<u32>>,
    mempool: &HashMap<String, u64>,
) -> (u16, String) {
    let path = path.strip_prefix("/api").unwrap_or(path);
    if path == "/blocks/tip/height" {
        return (200, TIP.to_string());
    }
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match parts.as_slice() {
        ["address", addr] => {
            let chain = history.get(*addr).map_or(0, Vec::len);
            let pool = mempool.get(*addr).copied().unwrap_or(0);
            (
                200,
                format!(
                    r#"{{"chain_stats":{{"tx_count":{chain}}},"mempool_stats":{{"tx_count":{pool}}}}}"#
                ),
            )
        }
        ["address", addr, "txs", "chain", rest @ ..] => {
            let heights = history.get(*addr).cloned().unwrap_or_default();
            // txids are "<addr>-<i>"; a page starts after the given txid.
            let start = rest
                .first()
                .and_then(|after| after.rsplit('-').next()?.parse::<usize>().ok())
                .map_or(0, |i| i + 1);
            let page: Vec<String> = heights
                .iter()
                .enumerate()
                .skip(start)
                .take(25)
                .map(|(i, h)| format!(r#"{{"txid":"{addr}-{i}","status":{{"block_height":{h}}}}}"#))
                .collect();
            (200, format!("[{}]", page.join(",")))
        }
        _ => (404, "not found".to_owned()),
    }
}

#[test]
fn a_used_wallet_starts_just_before_its_first_transaction() {
    let mut history = HashMap::new();
    history.insert(address(KeychainKind::External, 0), vec![150_500, 150_100]);
    history.insert(address(KeychainKind::External, 2), vec![151_000]);
    // Change address 3: the earliest use of all.
    history.insert(address(KeychainKind::Internal, 3), vec![149_990]);
    let explorer = FakeEsplora::start(history, HashMap::new(), 0);

    let found = find_birthday(&descriptors(), Network::Testnet4, &explorer.url).unwrap();
    assert_eq!(found.first_used, Some(149_990));
    assert_eq!(found.birthday, 149_984); // six blocks of margin
    // External: 0..=2 plus 20 unused after the last used one; internal: 0..=3 plus 20.
    assert_eq!(found.addresses_checked, 3 + GAP_LIMIT + 4 + GAP_LIMIT);
    assert!(found.explorer.starts_with("http://127.0.0.1"));
}

#[test]
fn a_never_used_wallet_starts_at_the_tip() {
    let explorer = FakeEsplora::start(HashMap::new(), HashMap::new(), 0);
    let found = find_birthday(&descriptors(), Network::Testnet4, &explorer.url).unwrap();
    assert_eq!(found.first_used, None);
    assert_eq!(found.birthday, TIP);
    assert_eq!(found.addresses_checked, 2 * GAP_LIMIT);
}

#[test]
fn unconfirmed_use_keeps_scanning_but_starts_at_the_tip() {
    let mut mempool = HashMap::new();
    mempool.insert(address(KeychainKind::External, 0), 1);
    let explorer = FakeEsplora::start(HashMap::new(), mempool, 0);
    let found = find_birthday(&descriptors(), Network::Testnet4, &explorer.url).unwrap();
    assert_eq!(found.first_used, None);
    assert_eq!(found.birthday, TIP);
    assert_eq!(found.addresses_checked, 1 + GAP_LIMIT + GAP_LIMIT);
}

#[test]
fn long_histories_are_paged_to_the_oldest_transaction() {
    let mut history = HashMap::new();
    // 60 transactions, newest first: three pages, the oldest on the last.
    let heights: Vec<u32> = (0..60).map(|i| 158_000 - i * 10).collect();
    history.insert(address(KeychainKind::External, 0), heights);
    let explorer = FakeEsplora::start(history, HashMap::new(), 0);
    let found = find_birthday(&descriptors(), Network::Testnet4, &explorer.url).unwrap();
    assert_eq!(found.first_used, Some(158_000 - 59 * 10));
    let pages = explorer
        .hits()
        .iter()
        .filter(|p| p.contains("/txs/chain"))
        .count();
    assert_eq!(pages, 3);
}

#[test]
fn rate_limits_are_retried() {
    let explorer = FakeEsplora::start(HashMap::new(), HashMap::new(), 2);
    let found = find_birthday(&descriptors(), Network::Testnet4, &explorer.url).unwrap();
    assert_eq!(found.birthday, TIP);
}

#[test]
fn plain_http_is_refused_except_on_this_machine() {
    let err = find_birthday(
        &descriptors(),
        Network::Testnet4,
        "http://mempool.space/testnet4/api",
    )
    .unwrap_err();
    assert!(matches!(err, WalletError::Config(_)), "{err}");
}

#[test]
fn an_unreachable_explorer_is_an_explorer_error() {
    let err =
        find_birthday(&descriptors(), Network::Testnet4, "http://127.0.0.1:9/api").unwrap_err();
    assert_eq!(err.code(), "explorer");
}

/// The public test phrase is used on testnet4 by many people; mempool.space must find it.
#[test]
#[ignore = "needs the internet (mempool.space)"]
fn live_mempool_space_finds_the_public_test_wallet() {
    let found = find_birthday(
        &descriptors(),
        Network::Testnet4,
        "https://mempool.space/testnet4/api",
    )
    .unwrap();
    eprintln!("{found:?}");
    let first = found
        .first_used
        .expect("the public test wallet has history");
    assert!(first < 154_405, "{found:?}");
    assert_eq!(found.birthday, first - 6);
}
