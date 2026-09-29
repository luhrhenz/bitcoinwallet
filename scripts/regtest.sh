#!/usr/bin/env bash
# Local regtest node for demos and manual testing.
#
#   scripts/regtest.sh start          start bitcoind (datadir ./regtest-data) + "faucet" wallet with 101 blocks
#   scripts/regtest.sh env            print the exports that point btcw at this node
#   scripts/regtest.sh mine [N]       mine N blocks (default 1) to the faucet
#   scripts/regtest.sh fund ADDR BTC  send BTC from the faucet to ADDR (then `mine` to confirm)
#   scripts/regtest.sh cli ARGS...    run bitcoin-cli against this node
#   scripts/regtest.sh stop           stop the node
#   scripts/regtest.sh reset          stop and delete all regtest data
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DATADIR="$ROOT/regtest-data"
RPCPORT="${RPCPORT:-18443}"
BITCOIND="${BITCOIND_EXE:-bitcoind}"
BITCOIN_CLI="${BITCOIN_CLI:-bitcoin-cli}"

cli() { "$BITCOIN_CLI" -regtest -datadir="$DATADIR" -rpcport="$RPCPORT" "$@"; }

mine() {
  local n="${1:-1}" addr
  addr="$(cli -rpcwallet=faucet getnewaddress)"
  # Batches of 25: Core's RPC can take ~200 ms per regtest block.
  while (( n > 0 )); do
    local b=$(( n < 25 ? n : 25 ))
    cli generatetoaddress "$b" "$addr" >/dev/null
    n=$(( n - b ))
  done
  echo "tip: $(cli getblockcount)"
}

case "${1:-}" in
  start)
    mkdir -p "$DATADIR"
    "$BITCOIND" -regtest -datadir="$DATADIR" -rpcport="$RPCPORT" -port=$((RPCPORT + 1)) \
      -listen=0 -fallbackfee=0.0002 -daemon
    for _ in $(seq 1 50); do cli getblockcount >/dev/null 2>&1 && break; sleep 0.2; done
    cli createwallet faucet >/dev/null 2>&1 || cli loadwallet faucet >/dev/null 2>&1 || true
    if (( $(cli getblockcount) < 101 )); then mine 101; fi
    echo "regtest node up on 127.0.0.1:$RPCPORT"
    "$0" env
    ;;
  env)
    echo "export BTCW_NETWORK=regtest"
    echo "export BTCW_RPC_URL=http://127.0.0.1:$RPCPORT"
    echo "export BTCW_RPC_COOKIE=$DATADIR/regtest/.cookie"
    ;;
  mine) mine "${2:-1}" ;;
  fund)
    [[ $# -eq 3 ]] || { echo "usage: $0 fund ADDR BTC" >&2; exit 1; }
    cli -rpcwallet=faucet sendtoaddress "$2" "$3"
    ;;
  cli) shift; cli "$@" ;;
  stop) cli stop ;;
  reset) cli stop 2>/dev/null || true; sleep 1; rm -rf "$DATADIR"; echo "regtest data removed" ;;
  *) sed -n '2,11p' "$0"; exit 1 ;;
esac
