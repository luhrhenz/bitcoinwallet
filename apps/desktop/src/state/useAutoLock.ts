import { useEffect, useRef } from "react";

const ACTIVITY_EVENTS = ["pointerdown", "pointermove", "keydown", "wheel", "touchstart"] as const;

/** How often, at most, activity is reported to the backend (`onActive`). */
export const KEEP_ALIVE_MS = 30_000;

/**
 * Calls `onIdle` once nobody has touched the window for `minutes` minutes, while `enabled`
 * (the wallet is unlocked). Any key, click, pointer move or scroll restarts the countdown.
 *
 * `onActive` reports that activity, at most every `KEEP_ALIVE_MS`: the Rust side keeps its own
 * auto-lock as a backstop (it locks a minute after this timer would) and counts only real use,
 * not the background polls, so it needs to hear that the user is still there.
 */
export function useAutoLock(enabled: boolean, minutes: number, onIdle: () => void, onActive?: () => void): void {
  const idle = useRef(onIdle);
  idle.current = onIdle;
  const active = useRef(onActive);
  active.current = onActive;

  useEffect(() => {
    if (!enabled || !(minutes > 0)) return;
    const ms = minutes * 60_000;
    let last = Date.now();
    let reported = last;
    let timer = setTimeout(() => idle.current(), ms);

    const onActivity = () => {
      const now = Date.now();
      // Pointer moves fire constantly; restarting the timer once a second is plenty.
      if (now - last < 1000) return;
      last = now;
      clearTimeout(timer);
      timer = setTimeout(() => idle.current(), ms);
      if (now - reported >= KEEP_ALIVE_MS) {
        reported = now;
        active.current?.();
      }
    };

    for (const name of ACTIVITY_EVENTS) window.addEventListener(name, onActivity, { passive: true });
    return () => {
      clearTimeout(timer);
      for (const name of ACTIVITY_EVENTS) window.removeEventListener(name, onActivity);
    };
  }, [enabled, minutes]);
}
