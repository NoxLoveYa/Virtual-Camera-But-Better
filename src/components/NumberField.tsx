import { useEffect, useRef, useState, type KeyboardEvent } from "react";

interface Props {
  value: number;
  min: number;
  max: number;
  onChange: (value: number) => void;
  /** Names the box for screen readers (the visible caption is a separate element). */
  label: string;
  unit?: string;
}

const clamp = (n: number, min: number, max: number) => Math.min(max, Math.max(min, n));

/**
 * An integer box with −/+ buttons. Typing is applied on Enter or when the box loses focus; the arrow keys and the mouse
 * wheel (while the box has focus) step by 1, Shift by 10. Everything is clamped to min…max.
 */
export function NumberField({ value, min, max, onChange, label, unit }: Props) {
  const input = useRef<HTMLInputElement>(null);
  const [draft, setDraft] = useState<string | null>(null);
  // The wheel handler needs the newest value without being re-attached on every change.
  const live = useRef({ value, min, max, onChange });
  live.current = { value, min, max, onChange };

  const set = (n: number) => {
    const next = clamp(Math.round(n), min, max);
    if (next !== value) onChange(next);
  };
  const commit = () => {
    if (draft === null) return;
    const n = Number(draft);
    if (draft.trim() !== "" && Number.isFinite(n)) set(n);
    setDraft(null);
  };

  useEffect(() => {
    const el = input.current;
    if (!el) return;
    const wheel = (e: WheelEvent) => {
      if (document.activeElement !== el || e.deltaY === 0) return;
      e.preventDefault();
      const { value, min, max, onChange } = live.current;
      const next = clamp(value + (e.deltaY < 0 ? 1 : -1) * (e.shiftKey ? 10 : 1), min, max);
      if (next !== value) onChange(next);
    };
    // Not a React handler: those are passive, and the page must not scroll while the wheel changes the number.
    el.addEventListener("wheel", wheel, { passive: false });
    return () => el.removeEventListener("wheel", wheel);
  }, []);

  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === "Enter") {
      e.preventDefault();
      commit();
    } else if (e.key === "Escape" && draft !== null) {
      e.stopPropagation();
      setDraft(null);
    } else if (e.key === "ArrowUp" || e.key === "ArrowDown") {
      e.preventDefault();
      set(value + (e.key === "ArrowUp" ? 1 : -1) * (e.shiftKey ? 10 : 1));
      setDraft(null);
    }
  };

  return (
    <div className="num-field">
      <button type="button" className="num-step" aria-label={`Decrease ${label}`} disabled={value <= min} onClick={() => set(value - 1)}>
        −
      </button>
      <input
        ref={input}
        type="text"
        inputMode="numeric"
        autoComplete="off"
        spellCheck={false}
        aria-label={label}
        value={draft ?? String(value)}
        onChange={(e) => setDraft(e.target.value)}
        onFocus={(e) => e.target.select()}
        onBlur={commit}
        onKeyDown={onKeyDown}
      />
      {unit && <span className="num-unit">{unit}</span>}
      <button type="button" className="num-step" aria-label={`Increase ${label}`} disabled={value >= max} onClick={() => set(value + 1)}>
        +
      </button>
    </div>
  );
}
