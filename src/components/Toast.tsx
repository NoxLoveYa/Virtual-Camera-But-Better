import { useEffect, useState } from "react";
import { useStore } from "../store";
import { Icon } from "./Icon";

/** A field where Ctrl+Z means "undo my typing": the toast's Undo leaves it alone. */
const TEXT_FIELD = 'textarea, [contenteditable=""], [contenteditable="true"], input:not([type="range"], [type="checkbox"], [type="button"], [type="color"])';

/**
 * What happened, in the footer next to the stats (it never covers a control). It stays until its timer runs out; the
 * timer waits while the pointer is over it or focus is inside it, and starts over in full when they leave.
 * Ctrl+Z presses its Undo while one is offered.
 */
export function Toast() {
  const notice = useStore((s) => s.notice);
  const seq = useStore((s) => s.noticeSeq);
  const canUndo = useStore((s) => s.undo !== null);
  const { dismissNotice, undoChain } = useStore.getState();
  const [hold, setHold] = useState(false);

  // A new or gone toast is a new element: whatever held the old one's timer is not on it.
  useEffect(() => setHold(false), [seq, notice === null]);

  useEffect(() => {
    if (!notice || hold) return;
    const t = setTimeout(dismissNotice, 4000 + notice.length * 40);
    return () => clearTimeout(t);
  }, [notice, seq, hold, dismissNotice]);

  useEffect(() => {
    if (!canUndo) return;
    const key = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey) || e.shiftKey || e.altKey || e.key.toLowerCase() !== "z") return;
      if ((e.target as Element | null)?.closest?.(TEXT_FIELD)) return;
      e.preventDefault();
      undoChain();
    };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, [canUndo, undoChain]);

  if (!notice) return null;
  return (
    <div
      className="toast"
      role="status"
      key={seq}
      onMouseEnter={() => setHold(true)}
      onMouseLeave={() => setHold(false)}
      onFocus={() => setHold(true)}
      onBlur={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setHold(false);
      }}
    >
      <span>{notice}</span>
      {canUndo && (
        <button className="toast-undo" onClick={undoChain} title="Undo (Ctrl+Z)">
          <Icon name="undo" /> Undo
        </button>
      )}
      <button className="ghost icon" aria-label="Dismiss" onClick={dismissNotice}>
        <Icon name="close" />
      </button>
    </div>
  );
}
