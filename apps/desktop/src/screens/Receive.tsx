import { QRCodeSVG } from "qrcode.react";
import { useCallback, useEffect, useState } from "react";
import { api } from "../lib/api";
import { NETWORKS } from "../lib/network";
import type { AddressRow } from "../lib/types";
import { useWallet } from "../state/wallet";
import { AddressChunks } from "../components/Address";
import { CopyButton } from "../components/CopyButton";
import { ErrorNotice } from "../components/ErrorNotice";
import { ScreenHeader } from "../components/Frame";
import { Icon } from "../components/Icon";

/** The current receive address (QR + copy) and every address handed out so far. */
export function Receive() {
  const { info, sync, runSync } = useWallet();
  const meta = NETWORKS[info.network];
  const [current, setCurrent] = useState<AddressRow | null>(null);
  const [all, setAll] = useState<AddressRow[] | null>(null);
  const [error, setError] = useState<unknown>(null);

  const load = useCallback(async () => {
    try {
      // `newAddress` returns the same address until it has been paid (BDK's next_unused).
      const next = await api.newAddress();
      const list = await api.listAddresses();
      setCurrent(next);
      setAll(list);
      setError(null);
    } catch (e) {
      setError(e);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  async function checkForPayments() {
    await runSync();
    await load();
  }

  return (
    <div className="screen">
      <ScreenHeader
        title="Receive"
        lead={`Give this address to whoever is paying you. Only ${meta.label} coins can be sent to it.`}
      />
      <ErrorNotice error={error} network={info.network} />

      {current && (
        <section className="receive" aria-label="Current receive address">
          <div className="receive__qr">
            {/* BIP21 URI so wallets that scan it know it's a bitcoin address. Always dark on light. */}
            <QRCodeSVG value={`bitcoin:${current.address}`} size={176} marginSize={2} level="M" title={`QR code for ${current.address}`} />
          </div>
          <div className="receive__info">
            <p className="eyebrow">Address #{current.index}</p>
            <p className="receive__address">
              <AddressChunks address={current.address} size="lg" />
            </p>
            <div className="receive__actions">
              <CopyButton text={current.address} label="Copy address" />
              <button type="button" className="btn btn--quiet" onClick={() => void checkForPayments()} disabled={sync.running}>
                <Icon name="sync" size={16} className={sync.running ? "spin" : undefined} />
                {sync.running ? "Checking…" : "Check for payments"}
              </button>
            </div>
            <p className="muted small">
              btcw shows this same address until it receives a payment, then moves on to a fresh one. A new address for
              every payment keeps others from linking your payments together, so an address that has been paid is never
              shown here again.
            </p>
          </div>
        </section>
      )}

      {all && all.length > 0 && (
        <section aria-labelledby="addresses-title">
          <div className="section-head">
            <h2 className="eyebrow" id="addresses-title">
              Addresses handed out
            </h2>
          </div>
          <ul className="addrlist">
            {[...all].reverse().map((row) => (
              <li key={row.index} className="addrlist__row">
                <span className="addrlist__index">#{row.index}</span>
                <span className="addrlist__addr mono">{row.address}</span>
                <span className={`tag ${row.used ? "tag--used" : "tag--fresh"}`}>
                  {row.used ? "Used" : row.index === current?.index ? "Current" : "Unused"}
                </span>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}
