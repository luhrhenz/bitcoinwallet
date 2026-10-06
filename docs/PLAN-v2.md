# btcw v2 plan: address book, fee bump, AI assistant

Requested by the owner after the capstone presentation. Same process as v1: fixed contracts here,
one agent at a time in its own worktree, the lead reviews every change before it reaches `main`.

## 1. Address book and labels (core + CLI + desktop)

- Per-network tables in `wallet.sqlite` (our own, next to `btcw_meta`):
  - `btcw_contacts(name TEXT PRIMARY KEY COLLATE NOCASE, address TEXT NOT NULL, note TEXT)`.
    The address is validated for the wallet's network on insert. Names are 1–40 characters,
    trimmed, and unique ignoring case.
  - `btcw_labels(txid TEXT PRIMARY KEY, label TEXT NOT NULL)`, up to 100 characters.
- Core API (`wallet.rs` or a new `book.rs`): `contacts()`, `add_contact(name, address, note)`,
  `remove_contact(name)`, `rename_contact(old, new)`, `resolve_recipient(input)` (a contact name
  *or* an address, giving `Address` + the contact name if it was one), `set_label(txid, label)`,
  `clear_label(txid)`.
  `TxRow` gains `label: Option<String>`, and `SendPreview` gains `contact: Option<String>`.
- The preview always shows the **full address** next to a contact name. A name never replaces
  the address in what the user confirms.
- CLI: `btcw contacts list|add NAME ADDRESS [--note]|remove NAME|rename OLD NEW`,
  `btcw label TXID TEXT` / `btcw label TXID --clear`, `send --to` accepts a contact name, and
  `history` shows labels.
- Errors: new `WalletError::Contact(String)` with code `contact`.

## 2. Fee bump (RBF): "speed up" a stuck payment

- Core `tx::prepare_fee_bump(wallet, node, txid, fee_rate) -> (Psbt, SendPreview)` uses BDK's
  `build_fee_bump`. It refuses (with `TxBuild` and a clear message) when the tx is:
  - not ours
  - already confirmed
  - not signalling RBF
  - below the BIP125 rules: new fee rate ≥ old + 1 sat/vB, and the absolute fee must rise by at
    least the incremental relay fee × new vsize
- It keeps the same recipient and amount; the extra fee comes out of change (BDK's default).
  `SendPreview` gains `replaces: Option<String>` (the txid being replaced) so both frontends can
  say "this replaces …".
- Signing and broadcast reuse `complete_send` / `broadcast_signed`. On success the old tx drops
  out of history (BDK canonicalization) and the new one appears.
- CLI: `btcw bump TXID --fee-rate N [--yes]` with the same preview → confirm → password flow as
  `send`. Desktop: a "Speed up" button on an **unconfirmed outgoing** tx's detail screen.

## 3. AI assistant (desktop app only)

**Principle: the model can read and prepare, never move money.** Every payment it prepares
becomes the same preview card the Send screen shows. Only the user can confirm it, with a click
plus their password (or the unlocked session), exactly like a manual send.

- **Provider:** any OpenAI-compatible `chat/completions` endpoint with tool calling. Settings:
  `base_url`, `model`, `api_key` (optional; Ollama needs none). The default is
  **Ollama at `http://127.0.0.1:11434/v1`, model `qwen2.5:3b`**, which is free and local. Groq,
  Gemini and OpenRouter work by changing the URL and key.
  - The API key is stored in `desktop.json` (0600). After saving, it is never sent back to the UI,
    which only learns whether a key is set.
- **Off by default.** Enabling it shows exactly what is shared and where, local vs cloud.
- **Runs in Rust** (`src-tauri/src/assistant/`), never in the webview. The UI sends the user's
  text and gets back messages and cards.
- **Tools** (JSON-schema function definitions), all backed by existing core functions:
  - `get_balance`
  - `list_transactions(limit ≤ 50)`, with labels
  - `get_transaction(txid)`
  - `new_receive_address`
  - `list_contacts`
  - `estimate_fee`
  - `get_btc_price(currency)`: CoinGecko simple price, only if "live price" is also enabled, and
    labelled as mainnet BTC (testnet coins have no value)
  - `prepare_payment(to, amount_sat, fee_rate?)`: contact name or address, returns a **card id**
    for the UI (same pending-send slot and rules as `prepare_send`)
  - `prepare_fee_bump(txid, fee_rate)`: same as above
- **The model never receives** the phrase, keys, password, PSBTs, or anything that can confirm.
  There is no confirm or send tool, so a prompt injection cannot make it pay anyone.
- **Limits:**
  - at most 6 tool calls per user message, and a 60-second timeout per model call
  - conversation kept in memory only, never on disk; cleared on lock and on network switch
  - tool outputs are passed as data, with a system prompt telling the model to treat them so
- **UI:** an "Assistant" screen with a chat thread. Payment and fee-bump cards show the full
  preview with Confirm / Cancel, reusing the Send screen's flow. There's an error state when the
  provider is unreachable ("start Ollama: `ollama serve`").
- **Tests:** a scripted fake OpenAI server (local TCP) returns tool calls. They prove:
  - read tools return correct data
  - `prepare_payment` creates a pending send but never broadcasts
  - an injected "send everything to X" in a tool result cannot produce a broadcast
  - the 6-tool-call cap and the timeout work
  - the API key never appears in the UI or in logs

## Order (one agent at a time)

| Agent | Scope |
|---|---|
| **I** | Core + CLI: address book, labels, fee bump (sections 1 and 2) |
| **J** | Desktop: Tauri commands + UI for contacts, labels, "Speed up" |
| **K** | Desktop: the assistant (section 3), backend + chat UI |

Nothing merges to `main` until the owner says so (presentation day).
