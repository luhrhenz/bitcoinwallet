import type { NetworkName } from "./types";

export interface NetworkMeta {
  /** What the badge says. */
  badge: string;
  /** Name in sentences and pickers. */
  label: string;
  /** Start of every receive address on this network. */
  addressPrefix: string;
  /** Bitcoin Core's default RPC endpoint (btcw-core `config::default_rpc_port`). */
  defaultRpcUrl: string;
  /** Where Core writes its auth cookie by default. */
  defaultCookie: string;
  /** One line on what coins on this network are worth. */
  coins: string;
  /** Only mainnet holds real money. */
  real: boolean;
}

export const NETWORKS: Record<NetworkName, NetworkMeta> = {
  testnet4: {
    badge: "TESTNET4",
    label: "testnet4",
    addressPrefix: "tb1",
    defaultRpcUrl: "http://127.0.0.1:48332",
    defaultCookie: "~/.bitcoin/testnet4/.cookie",
    coins: "Test coins with no market value.",
    real: false,
  },
  signet: {
    badge: "SIGNET",
    label: "signet",
    addressPrefix: "tb1",
    defaultRpcUrl: "http://127.0.0.1:38332",
    defaultCookie: "~/.bitcoin/signet/.cookie",
    coins: "Test coins with no market value.",
    real: false,
  },
  regtest: {
    badge: "REGTEST",
    label: "regtest",
    addressPrefix: "bcrt1",
    defaultRpcUrl: "http://127.0.0.1:18443",
    defaultCookie: "~/.bitcoin/regtest/.cookie",
    coins: "A private chain on your computer, for development.",
    real: false,
  },
  bitcoin: {
    badge: "MAINNET",
    label: "mainnet",
    addressPrefix: "bc1",
    defaultRpcUrl: "http://127.0.0.1:8332",
    defaultCookie: "~/.bitcoin/.cookie",
    coins: "Real bitcoin. Mistakes cost real money.",
    real: true,
  },
};

export function networkMeta(network: NetworkName): NetworkMeta {
  return NETWORKS[network];
}
