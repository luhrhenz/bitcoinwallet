import { useEffect, useId, useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { PRESETS } from "../lib/assistant";
import type { AssistantSettings } from "../lib/types";
import { useWallet } from "../state/wallet";
import { ErrorNotice } from "./ErrorNotice";
import { Icon } from "./Icon";

interface Draft {
  enabled: boolean;
  provider: AssistantSettings["provider"];
  baseUrl: string;
  model: string;
  /** A new key typed here; the saved one is never shown. */
  apiKey: string;
  livePrice: boolean;
}

const draftOf = (s: AssistantSettings): Draft => ({
  enabled: s.enabled,
  provider: s.provider,
  baseUrl: s.base_url,
  model: s.model,
  apiKey: "",
  livePrice: s.live_price,
});

/** Settings → Assistant: on/off, live price, and the one-time data consent. */
export function AssistantSettingsPanel() {
  const wallet = useWallet();
  const titleId = useId();
  const [saved, setSaved] = useState<AssistantSettings | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [agree, setAgree] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [consentError, setConsentError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    let alive = true;
    api
      .getAssistantSettings()
      .then((s) => {
        if (!alive) return;
        setSaved(s);
        setDraft(draftOf(s));
      })
      .catch((err) => alive && setError(err));
    return () => {
      alive = false;
    };
  }, []);

  if (!saved || !draft) {
    return (
      <section className="panel stack" aria-labelledby={titleId}>
        <h2 className="panel__legend" id={titleId}>
          Assistant
        </h2>
        <ErrorNotice error={error} />
      </section>
    );
  }

  const preset = PRESETS.groq;
  const needsConsent = draft.enabled && !saved.consented;
  const update = (patch: Partial<Draft>) => {
    setDraft((d) => (d ? { ...d, ...patch } : d));
    setError(null);
    setConsentError(null);
  };

  async function save(e: FormEvent) {
    e.preventDefault();
    if (!draft || saving) return;
    if (needsConsent && !agree) {
      setConsentError("Tick the box to agree before turning the assistant on.");
      return;
    }
    setSaving(true);
    try {
      const next = await api.setAssistantSettings({
        enabled: draft.enabled,
        provider: "groq",
        base_url: PRESETS.groq.baseUrl,
        model: PRESETS.groq.model,
        api_key: null,
        clear_api_key: false,
        live_price: draft.livePrice,
        consent: needsConsent && agree,
      });
      setSaved(next);
      setDraft(draftOf(next));
      setAgree(false);
      wallet.notify("success", "Assistant settings saved.");
    } catch (err) {
      setError(err);
    } finally {
      setSaving(false);
    }
  }

  return (
    <form className="panel stack" aria-labelledby={titleId} onSubmit={(e) => void save(e)} noValidate>
      <h2 className="panel__legend" id={titleId}>
        Assistant
      </h2>
      <p className="muted small">
        A chat that answers questions about this wallet and can prepare payments for you to confirm. It runs on Groq
        (key from the project&apos;s .env file).
      </p>
      <fieldset className="stack" disabled={saving}>
        <label className="check">
          <input type="checkbox" checked={draft.enabled} onChange={(e) => update({ enabled: e.target.checked })} />
          <span>Turn on the assistant</span>
        </label>
        {needsConsent && (
          <div className="notice notice--warning" role="note" aria-label="Data notice">
            <Icon name="alert" className="notice__icon" />
            <div className="notice__body">
              <p className="notice__title">Your wallet data will go to {preset.label}&apos;s cloud.</p>
              <p className="notice__detail">
                When you ask something, btcw sends your balance, transaction history, addresses and contact names to the
                Groq, and Groq may keep them under its own terms. Your recovery phrase, keys
                and password never leave this computer. The assistant can prepare a payment, but only you can confirm
                it.
              </p>
              <label className="check">
                <input type="checkbox" checked={agree} onChange={(e) => setAgree(e.target.checked)} />
                <span>I understand and agree</span>
              </label>
              {consentError && <p className="field__error">{consentError}</p>}
            </div>
          </div>
        )}
        <label className="check">
          <input type="checkbox" checked={draft.livePrice} onChange={(e) => update({ livePrice: e.target.checked })} />
          <span>Let it look up the live BTC price (CoinGecko)</span>
        </label>
      </fieldset>
      <ErrorNotice error={error} />
      <div className="actions">
        <button type="submit" className="btn btn--primary" disabled={saving}>
          {saving ? "Saving…" : "Save assistant settings"}
        </button>
      </div>
    </form>
  );
}
