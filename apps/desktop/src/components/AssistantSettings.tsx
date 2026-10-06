import { useEffect, useId, useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { PRESETS } from "../lib/assistant";
import type { AssistantProvider, AssistantSettings } from "../lib/types";
import { useWallet } from "../state/wallet";
import { ErrorNotice } from "./ErrorNotice";
import { Field, NO_ASSIST } from "./Field";
import { Icon } from "./Icon";

interface Draft {
  enabled: boolean;
  provider: AssistantProvider;
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

/** Settings → Assistant: provider, model, a write-only API key, and the one-time data consent. */
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

  const preset = PRESETS[draft.provider];
  const needsConsent = draft.enabled && !saved.consented;
  const update = (patch: Partial<Draft>) => {
    setDraft((d) => (d ? { ...d, ...patch } : d));
    setError(null);
    setConsentError(null);
  };

  function pickProvider(id: AssistantProvider) {
    const next = PRESETS[id];
    update(id === "custom" ? { provider: id } : { provider: id, baseUrl: next.baseUrl, model: next.model });
  }

  async function save(e: FormEvent, clearKey = false) {
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
        provider: draft.provider,
        base_url: draft.baseUrl.trim(),
        model: draft.model.trim(),
        api_key: draft.apiKey.trim() === "" ? null : draft.apiKey.trim(),
        clear_api_key: clearKey,
        live_price: draft.livePrice,
        consent: needsConsent && agree,
      });
      setSaved(next);
      setDraft(draftOf(next));
      setAgree(false);
      wallet.notify("success", clearKey ? "API key removed." : "Assistant settings saved.");
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
        A chat that answers questions about this wallet and can prepare payments for you to confirm. It runs on a cloud
        AI provider of your choice.
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
                provider you choose here, and the provider may keep them under its own terms. Your recovery phrase, keys
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
        <div className="segmented" role="radiogroup" aria-label="Provider">
          {(Object.keys(PRESETS) as AssistantProvider[]).map((id) => (
            <label key={id} className="segmented__option">
              <input type="radio" name="assistant-provider" checked={draft.provider === id} onChange={() => pickProvider(id)} />
              <span>{PRESETS[id].label}</span>
            </label>
          ))}
        </div>
        <Field
          label="Provider URL"
          value={draft.baseUrl}
          onChange={(e) => update({ baseUrl: e.target.value })}
          placeholder="https://…/v1"
          hint="Any OpenAI-compatible endpoint with tool calling. https only."
          mono
          {...NO_ASSIST}
        />
        <Field
          label="Model"
          value={draft.model}
          onChange={(e) => update({ model: e.target.value })}
          placeholder={preset.model || "model name"}
          hint="Providers rename models now and then; use the name they list."
          mono
          {...NO_ASSIST}
        />
        <Field
          label="API key"
          type="password"
          value={draft.apiKey}
          onChange={(e) => update({ apiKey: e.target.value })}
          placeholder={saved.has_api_key ? "Saved. Type a new key to replace it." : "Paste your key"}
          hint={`${preset.keyHint}. Stored on this computer only; it is never shown again.`}
          mono
          {...NO_ASSIST}
        />
        {saved.has_api_key && (
          <div className="row">
            <span className="muted small">A key is saved.</span>
            <button type="button" className="btn btn--quiet" onClick={(e) => void save(e, true)}>
              Remove key
            </button>
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
