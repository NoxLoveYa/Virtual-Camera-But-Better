import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { Chooser } from "../ui/useFileDrop";

interface Props {
  chooser: Chooser;
  /** The filter to add, or `null` to cancel. */
  onChoose: (use: "clut_filter" | "mask_filter" | null) => void;
}

const OPTIONS = [
  { use: "clut_filter" as const, label: "Apply LUT" },
  { use: "mask_filter" as const, label: "Image mask" },
  { use: null, label: "Cancel" },
];

/**
 * "Use as: Apply LUT · Image mask · Cancel" for a `.png` dropped on the window (it can be a LUT or a picture). It opens
 * at the drop point, kept inside the window. Arrow keys move between the choices, Enter takes one, Escape or a click
 * outside cancels.
 */
export function DropChooser({ chooser, onChoose }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ left: chooser.x, top: chooser.y });

  // Keep the box inside the window: it is measured before it is painted.
  useLayoutEffect(() => {
    const r = ref.current?.getBoundingClientRect();
    if (!r) return;
    const margin = 8;
    setPos({
      left: Math.max(margin, Math.min(chooser.x, window.innerWidth - r.width - margin)),
      top: Math.max(margin, Math.min(chooser.y, window.innerHeight - r.height - margin)),
    });
  }, [chooser.x, chooser.y]);

  useEffect(() => {
    ref.current?.querySelector<HTMLElement>("button")?.focus();
    const away = (e: PointerEvent) => {
      if (!ref.current?.contains(e.target as Node)) onChoose(null);
    };
    const esc = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      e.stopPropagation();
      onChoose(null);
    };
    window.addEventListener("pointerdown", away, true);
    window.addEventListener("keydown", esc, true);
    return () => {
      window.removeEventListener("pointerdown", away, true);
      window.removeEventListener("keydown", esc, true);
    };
  }, [onChoose]);

  const onKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    const buttons = Array.from(e.currentTarget.querySelectorAll<HTMLElement>("button"));
    const i = buttons.indexOf(document.activeElement as HTMLElement);
    const to = e.key === "ArrowDown" ? i + 1 : e.key === "ArrowUp" ? i - 1 : e.key === "Home" ? 0 : e.key === "End" ? buttons.length - 1 : null;
    if (to === null) return;
    e.preventDefault();
    buttons[(to + buttons.length) % buttons.length]?.focus();
  };

  return (
    <div ref={ref} className="drop-chooser" role="menu" aria-label="Use this picture as" style={pos} onKeyDown={onKeyDown}>
      <span className="label">Use as</span>
      {OPTIONS.map((o) => (
        <button key={o.label} type="button" role="menuitem" className={o.use ? "pm-item" : "pm-item ghost"} onClick={() => onChoose(o.use)}>
          {o.label}
        </button>
      ))}
    </div>
  );
}
