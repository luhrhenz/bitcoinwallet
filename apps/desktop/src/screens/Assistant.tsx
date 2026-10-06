import { useEffect, useRef, useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { PRESETS } from "../lib/assistant";
import { formatBtc } from "../lib/amount";
import { describeError, toApiError } from "../lib/errors";
import type { AssistantCard, AssistantSettings, ChatItem, PrepareRequest, PreparedSend } from "../lib/types";
import type { Go } from "../state/nav";
import { useWallet } from "../state/wallet";
import { ErrorNotice } from "../components/ErrorNotice";
import { ScreenHeader } from "../components/Frame";
import { Icon } from "../components/Icon";
import { PaymentPreview, sendLabel } from "../components/PaymentPreview";

/** What became of a card. Only one can be open: the backend keeps one prepared payment. */
type CardState =
  | { state: "open"; prepared: PreparedSend; bump: boolean }
  | { state: "unlock"; request: PrepareRequest }
  | { state: "sending"; prepared: PreparedSend; bump: boolean }
  | { state: "sent"; txid: string; prepared: PreparedSend }
  | { state: "closed"; reason: string };

type Entry = { item: ChatItem; cards: CardState[] };

const CLOSED_EARLIER = "This preview was closed. Ask again to prepare it anew.";

/**
 * Chat with the wallet assistant. It reads the wallet and prepares payments as cards; the user
 * confirms a card exactly like a payment on the Send screen. The chat lives in Rust's memory.
 */
export function Assistant({ go }: { go: Go }) {
  const wallet = useWallet();
  const { info } = wallet;
  const [settings, setSettings] = useState<AssistantSettings | null>(null);
  const [entries, setEntries] = useState<Entry[]>([]);
  const [text, setText] = useState("");
  const [thinking, setThinking] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [loadError, setLoadError] = useState<unknown>(null);

  // The open card's payment id, readable from cleanup code.
  const openId = useRef<string | null>(null);
  const endRef = useRef<HTMLDivElement | null>(null);

  async function reload() {
    try {
      const [s, items] = await Promise.all([api.getAssistantSettings(), api.assistantHistory()]);
      setSettings(s);
      // Cards from an earlier visit were cancelled when it ended.
      setEntries(items.map((item) => ({ item, cards: item.cards.map(() => closed(CLOSED_EARLIER)) })));
      setLoadError(null);
    } catch (err) {
      setLoadError(err);
    }
  }

  useEffect(() => {
    void reload();
  }, []);

  // Leaving the screen releases an open preview, as on the Send screen.
  useEffect(
    () => () => {
      const id = openId.current;
      openId.current = null;
      if (id) void api.cancelSend(id).catch(() => undefined);
    },
    [],
  );

  // Locking clears the chat (and the backend drops any prepared payment).
  const wasUnlocked = useRef(info.unlocked);
  useEffect(() => {
    if (wasUnlocked.current && !info.unlocked) {
      openId.current = null;
      setEntries([]);
    }
    wasUnlocked.current = info.unlocked;
  }, [info.unlocked]);

  useEffect(() => {
    endRef.current?.scrollIntoView?.({ block: "end" });
  }, [entries.length, thinking]);

  /** Replace one card's state; a newly opened card closes any other open one. */
  function setCard(entry: number, card: number, next: CardState) {
    setEntries((list) =>
      list.map((e, ei) => ({
        ...e,
        cards: e.cards.map((c, ci) => {
          if (ei === entry && ci === card) return next;
          if (next.state === "open" && c.state === "open") {
            return closed("Replaced by a newer preview. Nothing was sent.");
          }
          return c;
        }),
      })),
    );
    if (next.state === "open") openId.current = next.prepared.id;
  }

  async function submit(e: FormEvent) {
    e.preventDefault();
    const message = text.trim();
    if (!message || thinking) return;
    setThinking(true);
    setError(null);
    const pendingUser: Entry = { item: { role: "user", text: message, cards: [], tools: [] }, cards: [] };
    setEntries((list) => [...list, pendingUser]);
    setText("");
    try {
      const answer = await api.assistantSend(message);
      const lastOpen = answer.cards.map((c) => c.kind !== "needs_unlock").lastIndexOf(true);
      const cards = answer.cards.map((c, i) =>
        c.kind !== "needs_unlock" && i !== lastOpen ? closed("Replaced by a newer preview. Nothing was sent.") : cardState(c),
      );
      setEntries((list) => {
        // Any new open card replaces older ones (the backend keeps only the newest).
        const hasOpen = cards.some((c) => c.state === "open");
        const older = hasOpen
          ? list.map((en) => ({
              ...en,
              cards: en.cards.map((c) => (c.state === "open" ? closed("Replaced by a newer preview. Nothing was sent.") : c)),
            }))
          : list;
        return [...older, { item: answer, cards }];
      });
      const open = cards.find((c) => c.state === "open");
      if (open?.state === "open") openId.current = open.prepared.id;
      // Preparing a payment syncs; show the balance it used.
      if (cards.length > 0) void wallet.refreshWallet();
    } catch (err) {
      setEntries((list) => list.filter((en) => en !== pendingUser));
      setText(message);
      setError(err);
    } finally {
      setThinking(false);
    }
  }

  async function confirm(entry: number, card: number, current: CardState) {
    if (current.state !== "open") return;
    const { prepared, bump } = current;
    setCard(entry, card, { state: "sending", prepared, bump });
    try {
      const { txid } = await api.confirmSend(prepared.id);
      openId.current = null;
      setCard(entry, card, { state: "sent", txid, prepared });
      void wallet.refreshWallet();
    } catch (err) {
      openId.current = null;
      const { code } = toApiError(err);
      if (code === "locked") await wallet.refreshInfo().catch(() => undefined);
      setCard(entry, card, closed(`Not sent. ${describeError(err, { network: info.network }).title}`));
      void wallet.refreshWallet();
    }
  }

  async function cancel(entry: number, card: number, current: CardState) {
    if (current.state !== "open") return;
    openId.current = null;
    setCard(entry, card, closed("Cancelled. Nothing was sent."));
    await api.cancelSend(current.prepared.id).catch(() => undefined);
  }

  /** The wallet was locked when the assistant tried: unlock, then prepare it here. */
  async function unlockAndReview(entry: number, card: number, request: PrepareRequest) {
    if (!(await wallet.requestUnlock())) return;
    try {
      const prepared =
        request.action === "payment"
          ? await api.prepareSend(request.to, request.amount_sat, request.fee_rate_sat_vb)
          : await api.prepareFeeBump(request.txid, request.fee_rate_sat_vb);
      setCard(entry, card, { state: "open", prepared, bump: request.action === "fee_bump" });
      void wallet.refreshWallet();
    } catch (err) {
      setCard(entry, card, closed(`Couldn't prepare it. ${describeError(err, { network: info.network }).title}`));
    }
  }

  if (loadError) {
    return (
      <div className="screen">
        <ScreenHeader title="Assistant" />
        <ErrorNotice error={loadError} network={info.network} />
      </div>
    );
  }
  if (!settings) return <div className="screen" aria-busy="true" />;

  const providerLabel = PRESETS[settings.provider].label;
  if (!settings.enabled) {
    return (
      <div className="screen">
        <ScreenHeader
          title="Assistant"
          lead="Ask about your balance and history, or have a payment prepared for you to check and confirm."
        />
        <section className="panel stack">
          <p>The assistant is off.</p>
          <p className="muted small">
            It runs on Groq&apos;s cloud. Turning it on sends your
            balance, history, addresses and contact names to that provider. Your keys, recovery phrase and password never
            leave this computer, and only you can confirm a payment.
          </p>
          <div className="actions actions--start">
            <button type="button" className="btn btn--primary" onClick={() => go({ name: "settings" })}>
              Turn it on in Settings
            </button>
          </div>
        </section>
      </div>
    );
  }

  return (
    <div className="screen">
      <ScreenHeader
        title="Assistant"
        lead={`Answers come from ${providerLabel}. It can prepare a payment, but only you can send it.`}
      />
      {!settings.has_api_key && (
        <div className="notice notice--warning" role="note">
          <Icon name="alert" className="notice__icon" />
          <div className="notice__body">
            <p className="notice__title">No Groq API key: set GROQ_API_KEY in the project&apos;s .env file and restart.</p>
          </div>
        </div>
      )}
      <section className="chat" aria-label="Conversation" aria-live="polite">
        {entries.length === 0 && (
          <p className="muted small chat__empty">
            Try &ldquo;What&apos;s my balance?&rdquo;, &ldquo;Show my last transactions&rdquo; or &ldquo;Pay 5000 sat to Alice&rdquo;.
          </p>
        )}
        {entries.map((entry, ei) => (
          <div key={ei} className={`chat__msg chat__msg--${entry.item.role}`}>
            <p className="chat__bubble">{entry.item.text}</p>
            {entry.item.tools.length > 0 && (
              <p className="chat__tools muted small">Looked at: {[...new Set(entry.item.tools)].map(toolName).join(", ")}</p>
            )}
            {entry.cards.map((card, ci) => (
              <CardView
                key={ci}
                card={card}
                onConfirm={() => void confirm(ei, ci, card)}
                onCancel={() => void cancel(ei, ci, card)}
                onUnlock={(request) => void unlockAndReview(ei, ci, request)}
                onView={(txid, prepared) => go({ name: "tx", txid, sent: prepared.preview })}
              />
            ))}
          </div>
        ))}
        {thinking && <p className="chat__thinking muted small">Thinking…</p>}
        <div ref={endRef} />
      </section>
      <ErrorNotice error={error} network={info.network} />
      <form className="chat__form" onSubmit={submit}>
        <label className="sr-only" htmlFor="assistant-input">
          Message
        </label>
        <textarea
          id="assistant-input"
          className="input textarea chat__input"
          rows={2}
          maxLength={4000}
          value={text}
          placeholder="Ask about your wallet…"
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              e.currentTarget.form?.requestSubmit();
            }
          }}
          disabled={thinking}
        />
        <button type="submit" className="btn btn--primary" disabled={thinking || text.trim() === ""}>
          {thinking ? "Thinking…" : "Ask"}
        </button>
      </form>
      {entries.length > 0 && (
        <div className="actions actions--start">
          <button
            type="button"
            className="btn btn--quiet"
            onClick={() => {
              const id = openId.current;
              openId.current = null;
              if (id) void api.cancelSend(id).catch(() => undefined);
              setEntries([]);
              void api.assistantClear().catch(() => undefined);
            }}
            disabled={thinking}
          >
            Clear conversation
          </button>
        </div>
      )}
    </div>
  );
}

function CardView({
  card,
  onConfirm,
  onCancel,
  onUnlock,
  onView,
}: {
  card: CardState;
  onConfirm: () => void;
  onCancel: () => void;
  onUnlock: (request: PrepareRequest) => void;
  onView: (txid: string, prepared: PreparedSend) => void;
}) {
  switch (card.state) {
    case "open":
    case "sending":
      return (
        <PaymentPreview
          preview={card.prepared.preview}
          label={card.bump ? "Speed-up preview" : "Payment preview"}
          confirmLabel={card.bump ? "Send faster version" : sendLabel(card.prepared.preview)}
          sending={card.state === "sending"}
          onConfirm={onConfirm}
          onCancel={onCancel}
        />
      );
    case "unlock": {
      const r = card.request;
      return (
        <section className="panel stack chat__card" aria-label="Locked payment request">
          <p>
            {r.action === "payment"
              ? `Pay ${formatBtc(r.amount_sat)} BTC to ${r.to}`
              : `Speed up ${r.txid.slice(0, 12)}…`}
          </p>
          <p className="muted small">The wallet is locked. Unlock it to see the full preview; nothing is sent until you confirm.</p>
          <div className="actions actions--start">
            <button type="button" className="btn btn--primary" onClick={() => onUnlock(r)}>
              <Icon name="unlock" size={16} /> Unlock and review
            </button>
          </div>
        </section>
      );
    }
    case "sent":
      return (
        <section className="notice notice--success chat__card" role="status">
          <Icon name="check" className="notice__icon" />
          <div className="notice__body">
            <p className="notice__title">Sent.</p>
            <p className="notice__detail mono break">{card.txid}</p>
            <div className="notice__actions">
              <button type="button" className="btn btn--quiet" onClick={() => onView(card.txid, card.prepared)}>
                View transaction
              </button>
            </div>
          </div>
        </section>
      );
    case "closed":
      return <p className="chat__closed muted small">{card.reason}</p>;
  }
}

function cardState(card: AssistantCard): CardState {
  switch (card.kind) {
    case "payment":
      return { state: "open", prepared: { id: card.id, preview: card.preview }, bump: false };
    case "fee_bump":
      return { state: "open", prepared: { id: card.id, preview: card.preview }, bump: true };
    case "needs_unlock":
      return { state: "unlock", request: card.request };
  }
}

function closed(reason: string): CardState {
  return { state: "closed", reason };
}

const TOOL_NAMES: Record<string, string> = {
  get_balance: "balance",
  list_transactions: "transactions",
  get_transaction: "a transaction",
  new_receive_address: "a new address",
  list_contacts: "contacts",
  estimate_fee: "fee rates",
  get_btc_price: "the BTC price",
  prepare_payment: "prepared a payment",
  prepare_fee_bump: "prepared a speed-up",
};

const toolName = (name: string) => TOOL_NAMES[name] ?? name;
