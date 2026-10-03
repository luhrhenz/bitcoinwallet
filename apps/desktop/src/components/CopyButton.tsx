import { useEffect, useRef, useState } from "react";
import { Icon } from "./Icon";

/** Copies `text`; says "Copied" for two seconds (announced to screen readers too). */
export function CopyButton({ text, label = "Copy", className }: { text: string; label?: string; className?: string }) {
  const [state, setState] = useState<"idle" | "copied" | "failed">("idle");
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(
    () => () => {
      if (timer.current) clearTimeout(timer.current);
    },
    [],
  );

  async function copy() {
    let next: "copied" | "failed" = "copied";
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      next = "failed";
    }
    setState(next);
    if (timer.current) clearTimeout(timer.current);
    timer.current = setTimeout(() => setState("idle"), 2000);
  }

  return (
    <button type="button" className={`btn btn--quiet copy ${className ?? ""}`} onClick={copy} data-state={state}>
      <Icon name={state === "copied" ? "check" : "copy"} size={16} />
      <span aria-live="polite">
        {state === "copied" ? "Copied" : state === "failed" ? "Couldn't copy; select the text instead" : label}
      </span>
    </button>
  );
}
