import type { SendPreview } from "../lib/types";

/** Where the main window is. Plain state, no router: there are no URLs to share or restore. */
export type Route =
  | { name: "dashboard" }
  | { name: "receive" }
  | { name: "send" }
  | { name: "history" }
  | { name: "settings" }
  /** `sent` is set when arriving straight from a confirmed send. */
  | { name: "tx"; txid: string; sent?: SendPreview };

export type Go = (route: Route) => void;
