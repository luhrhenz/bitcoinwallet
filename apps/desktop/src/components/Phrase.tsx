import type { SyntheticEvent } from "react";
import { Icon } from "./Icon";

const block = (e: SyntheticEvent) => e.preventDefault();

/**
 * The numbered words: a sheet of paper, not a form. Not selectable (CSS), and copy, cut and the
 * context menu are blocked too, because clipboard managers keep a history.
 */
export function PhraseGrid({ words }: { words: string[] }) {
  return (
    <ol className="phrase" aria-label="Recovery phrase" onCopy={block} onCut={block} onContextMenu={block}>
      {words.map((word, i) => (
        <li key={i} className="phrase__item">
          <span className="phrase__n" aria-hidden="true">
            {i + 1}
          </span>
          <span className="phrase__word">{word}</span>
        </li>
      ))}
    </ol>
  );
}

/** The warning that goes with the words, when they are created and whenever they are shown again. */
export function PhraseWarning() {
  return (
    <div className="notice notice--warning" role="note">
      <Icon name="alert" className="notice__icon" />
      <div className="notice__body">
        <p className="notice__title">Anyone with these words can take your coins.</p>
        <ul className="notice__list">
          <li>Write them on paper, in order, and keep the paper somewhere safe and private.</li>
          <li>Don&apos;t take a screenshot or photo, and don&apos;t paste them into a file, email or chat.</li>
          <li>
            They are the only real backup. btcw can show them again (in Settings, with your password), but only while
            this computer works: lose the paper and this computer, and the coins are gone.
          </li>
        </ul>
      </div>
    </div>
  );
}
