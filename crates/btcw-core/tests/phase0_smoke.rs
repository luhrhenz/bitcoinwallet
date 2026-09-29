//! Phase 0 de-risking: proves bdk_wallet + bdk_bitcoind_rpc work against the local
//! Bitcoin Core version (sync, balance, build/sign/broadcast, confirmation).
//! Uses BDK directly with a fixed test descriptor, independent of the stubbed modules.

#![allow(clippy::unwrap_used)]

use bdk_bitcoind_rpc::bitcoincore_rpc::{Auth, Client, RpcApi};
use bdk_bitcoind_rpc::{Emitter, NO_EXPECTED_MEMPOOL_TXS};
use btcw_core::bdk_wallet::{KeychainKind, SignOptions, Wallet};
use btcw_core::bitcoin::bip32::Xpriv;
use btcw_core::bitcoin::secp256k1::Secp256k1;
use btcw_core::bitcoin::{Amount, FeeRate, Network};
use btcw_core::config::RpcAuth;
use btcw_core::testnode::TestNode;

const MASTER: &str = "tprv8ZgxMBicQKsPdy6LMhUtFHAgpocR8GC6QmwMSFpZs7h6Eziw3SpThFfczTDh5rW2krkqffa11UpX3XkeTTB2FvzZKWXqPY54Y6Rq4AQ5R8L";
const EXT: &str = "wpkh(tprv8ZgxMBicQKsPdy6LMhUtFHAgpocR8GC6QmwMSFpZs7h6Eziw3SpThFfczTDh5rW2krkqffa11UpX3XkeTTB2FvzZKWXqPY54Y6Rq4AQ5R8L/84'/1'/0'/0/*)";
const INT: &str = "wpkh(tprv8ZgxMBicQKsPdy6LMhUtFHAgpocR8GC6QmwMSFpZs7h6Eziw3SpThFfczTDh5rW2krkqffa11UpX3XkeTTB2FvzZKWXqPY54Y6Rq4AQ5R8L/84'/1'/0'/1/*)";

fn sync(wallet: &mut Wallet, client: &Client) {
    let mut emitter = Emitter::new(
        client,
        wallet.latest_checkpoint(),
        0,
        NO_EXPECTED_MEMPOOL_TXS,
    );
    while let Some(ev) = emitter.next_block().unwrap() {
        wallet
            .apply_block_connected_to(&ev.block, ev.block_height(), ev.connected_to())
            .unwrap();
    }
    let mempool = emitter.mempool().unwrap();
    wallet.apply_unconfirmed_txs(mempool.update);
}

#[test]
#[allow(clippy::unwrap_used)]
fn bdk_syncs_and_spends_against_local_core() {
    if !TestNode::available() {
        eprintln!("skipping: no bitcoind (set BITCOIND_EXE)");
        return;
    }
    let node = TestNode::start().unwrap();
    let rpc = node.rpc_config();
    let RpcAuth::Cookie(cookie) = rpc.auth else {
        unreachable!()
    };
    let client = Client::new(&rpc.url, Auth::CookieFile(cookie)).unwrap();

    let mut wallet = Wallet::create(EXT, INT)
        .network(Network::Regtest)
        .create_wallet_no_persist()
        .unwrap();
    let addr = wallet.reveal_next_address(KeychainKind::External).address;
    assert!(addr.to_string().starts_with("bcrt1q"));

    // Receive: unconfirmed, then confirmed.
    node.fund(&addr, Amount::from_btc(1.0).unwrap()).unwrap();
    sync(&mut wallet, &client);
    let b = wallet.balance();
    assert_eq!(b.untrusted_pending, Amount::from_btc(1.0).unwrap());
    node.mine(1).unwrap();
    sync(&mut wallet, &client);
    assert_eq!(wallet.balance().confirmed, Amount::from_btc(1.0).unwrap());

    // Send back to the faucet via PSBT, broadcast with the typed helper Agent B would use.
    let to = node.faucet_address().unwrap();
    let mut builder = wallet.build_tx();
    builder
        .add_recipient(to.script_pubkey(), Amount::from_sat(50_000))
        .fee_rate(FeeRate::from_sat_per_vb_u32(2));
    let mut psbt = builder.finish().unwrap();
    // BDK 3.2 path: rust-bitcoin signs with the master xprv, BDK finalizes.
    let master: Xpriv = MASTER.parse().unwrap();
    psbt.sign(&master, &Secp256k1::new()).unwrap();
    assert!(
        wallet
            .finalize_psbt(&mut psbt, SignOptions::default())
            .unwrap()
    );
    let tx = psbt.extract_tx().unwrap();
    let txid = client.send_raw_transaction(&tx).unwrap();
    assert_eq!(txid, tx.compute_txid());

    node.mine(1).unwrap();
    sync(&mut wallet, &client);
    let wtx = wallet.get_tx(txid).unwrap();
    assert!(wtx.chain_position.is_confirmed());
    let fee = wallet.calculate_fee(&tx).unwrap();
    assert_eq!(
        wallet.balance().confirmed,
        Amount::from_btc(1.0).unwrap() - Amount::from_sat(50_000) - fee
    );
    // Typed helpers that `chain.rs` may want: check they parse Core v31 responses.
    client.get_block_count().unwrap();
    client.estimate_smart_fee(6, None).unwrap();
    client.get_blockchain_info().unwrap();
}
