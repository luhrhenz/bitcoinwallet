import { NETWORKS } from "../lib/network";
import { useWallet } from "../state/wallet";
import { Icon } from "../components/Icon";

export function Welcome({
  onCreate,
  onRestore,
  onSettings,
}: {
  onCreate: () => void;
  onRestore: () => void;
  onSettings: () => void;
}) {
  const { info } = useWallet();
  const meta = NETWORKS[info.network];
  return (
    <div className="welcome">
      <div className="welcome__intro">
        <p className="eyebrow">No wallet on {meta.label} yet</p>
        <h1 className="welcome__title">Your keys stay on this computer. Your node checks the chain.</h1>
        <p className="lead">
          btcw keeps its keys encrypted here and talks only to your own Bitcoin node, never to a third-party
          server. {meta.coins}
        </p>
      </div>
      <div className="choice-list">
        <button type="button" className="choice" onClick={onCreate}>
          <span className="choice__title">Create a new wallet</span>
          <span className="choice__text">Get a fresh 12- or 24-word recovery phrase to write down on paper.</span>
          <Icon name="chevron" className="choice__chevron" />
        </button>
        <button type="button" className="choice" onClick={onRestore}>
          <span className="choice__title">Restore from a recovery phrase</span>
          <span className="choice__text">Type the words of a wallet you already have to get its coins back.</span>
          <Icon name="chevron" className="choice__chevron" />
        </button>
      </div>
      <p className="welcome__foot muted">
        Wrong network, or your node isn&apos;t on the default port?{" "}
        <button type="button" className="link" onClick={onSettings}>
          Change network and node settings
        </button>
      </p>
    </div>
  );
}
