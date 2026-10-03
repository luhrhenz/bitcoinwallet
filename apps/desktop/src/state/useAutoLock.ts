import { useEffect, useRef } from "react";

const ACTIVITY_EVENTS = ["pointerdown", "pointermove", "keydown", "wheel", "touchstart"] as const;

/**
 * Calls `onIdle` once nobody has touched the window for `minutes` minutes, while `enabled`
 * (the wallet is unlocked). Any key, click, pointer move or scroll restarts the countdown.
 */
export function useAutoLock(enabled: boolean, minutes: number, onIdle: () => void): void {
  const idle = useRef(onIdle);
  idle.current = onIdle;

  useEffect(() => {
    if (!enabled || !(minutes > 0)) return;
    const ms = minutes * 60_000;
    let last = Date.now();
    let timer = setTimeout(() => idle.current(), ms);

    const onActivity = () => {
      const now = Date.now();
      // Pointer moves fire constantly; restarting the timer once a second is plenty.
      if (now - last < 1000) return;
      last = now;
      clearTimeout(timer);
      timer = setTimeout(() => idle.current(), ms);
    };

    for (const name of ACTIVITY_EVENTS) window.addEventListener(name, onActivity, { passive: true });
    return () => {
      clearTimeout(timer);
      for (const name of ACTIVITY_EVENTS) window.removeEventListener(name, onActivity);
    };
  }, [enabled, minutes]);
}
