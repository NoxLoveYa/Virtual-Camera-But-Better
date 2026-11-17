import { useRef, useState, type CSSProperties, type KeyboardEvent } from "react";
import { colorAlpha, colorToHex, hexToColor, parseHex, withAlpha } from "../filters/values";
import { Popover } from "./Popover";

interface Props {
  /** OBS colour int 0xAABBGGRR. */
  value: number;
  onChange: (value: number) => void;
  /** Names the swatch button for screen readers. */
  label: string;
  /** Shows an alpha slider and edits the alpha byte. Without it only red, green and blue change; the alpha byte is kept. */
  alpha?: boolean;
}

/** Twelve swatches to start from: neutrals, the six hues, and a few in between. */
const PRESETS = ["#000000", "#ffffff", "#808080", "#ff0000", "#ff8000", "#ffff00", "#00ff00", "#00ffff", "#0000ff", "#8000ff", "#ff00ff", "#ff80c0"];

const percent = (a: number) => Math.round((a / 255) * 100);

/**
 * A swatch button that opens a small panel: hex text box, 12 preset swatches, the system colour picker and, with
 * `alpha`, an alpha slider. Enter or Space on the swatch opens it, Escape closes it and puts focus back on the swatch.
 */
export function ColorField({ value, onChange, label, alpha }: Props) {
  const [open, setOpen] = useState(false);
  const swatch = useRef<HTMLButtonElement>(null);
  const panel = useRef<HTMLDivElement>(null);
  const hex = colorToHex(value);
  const a = colorAlpha(value);
  // The text of the hex box while the user is typing in it; null shows the colour's own hex. The ref is what the
  // handlers read, so a blur that arrives before the next render never acts on an old draft.
  const [draftText, setDraftText] = useState<string | null>(null);
  const draftRef = useRef<string | null>(null);
  const setDraft = (text: string | null) => {
    draftRef.current = text;
    setDraftText(text);
  };
  const draft = draftText ?? hex;

  /** Closes the panel (text not yet applied is dropped); when focus was inside it (or nowhere), puts it back on the swatch. */
  const close = () => {
    setDraft(null);
    const f = document.activeElement;
    const inside = !f || f === document.body || !!panel.current?.contains(f);
    setOpen(false);
    if (inside) swatch.current?.focus();
  };

  /** A colour chosen with a preset swatch or the system picker. */
  const pick = (next: number | null) => {
    setDraft(null);
    if (next !== null && next !== value) onChange(next);
  };

  const typed = (text: string) => {
    setDraft(text);
    // Live while typing a full #rrggbb; the 3-digit short form waits for Enter or blur so "f00" does not jump the colour.
    if (/^#?[0-9a-f]{6}$/i.test(text.trim())) {
      const next = hexToColor(text, value);
      if (next !== null && next !== value) onChange(next);
    }
  };
  const settle = () => {
    const text = draftRef.current;
    if (text === null) return;
    setDraft(null);
    const next = hexToColor(text, value);
    if (next !== null && next !== value) onChange(next);
  };
  const onHexKey = (e: KeyboardEvent) => {
    if (e.key !== "Enter") return;
    e.preventDefault();
    settle();
    close();
  };

  const badHex = draft.trim() !== "" && !parseHex(draft);
  const chip = { "--c": `rgba(${parseInt(hex.slice(1, 3), 16)}, ${parseInt(hex.slice(3, 5), 16)}, ${parseInt(hex.slice(5, 7), 16)}, ${alpha ? a / 255 : 1})` } as CSSProperties;

  return (
    <Popover
      open={open}
      onClose={close}
      role="dialog"
      className="color-pop"
      anchor={
        <button
          ref={swatch}
          type="button"
          className="swatch"
          aria-haspopup="dialog"
          aria-expanded={open}
          aria-label={`${label}: ${hex}${alpha ? `, alpha ${percent(a)}%` : ""}`}
          onClick={() => (open ? close() : setOpen(true))}
        >
          <span className="chip" style={chip} />
          <span className="hex">{hex}</span>
          {alpha && <span className="hex">{percent(a)}%</span>}
        </button>
      }
    >
      <div className="cf" ref={panel} role="group" aria-label={label}>
        <div className="cf-top">
          <input
            autoFocus
            className={`cf-hex${badHex ? " bad" : ""}`}
            aria-label="Hex color"
            aria-invalid={badHex}
            spellCheck={false}
            maxLength={7}
            value={draft}
            onFocus={(e) => e.target.select()}
            onChange={(e) => typed(e.target.value)}
            onBlur={settle}
            onKeyDown={onHexKey}
          />
          <input
            type="color"
            className="cf-native"
            aria-label="Pick a color"
            title="Pick a color"
            value={hex}
            onChange={(e) => pick(hexToColor(e.target.value, value))}
          />
        </div>
        <div className="cf-presets" role="group" aria-label="Preset colors">
          {PRESETS.map((p) => (
            <button
              key={p}
              type="button"
              className="cf-preset"
              style={{ background: p }}
              aria-label={p}
              aria-pressed={p === hex}
              title={p}
              onClick={() => pick(hexToColor(p, value))}
            />
          ))}
        </div>
        {alpha && (
          <label className="cf-alpha">
            <span>
              Alpha<b>{percent(a)}%</b>
            </span>
            <input
              type="range"
              min={0}
              max={255}
              step={1}
              value={a}
              style={{ "--p": `${(a / 255) * 100}%` } as CSSProperties}
              onChange={(e) => onChange(withAlpha(value, Number(e.target.value)))}
            />
          </label>
        )}
      </div>
    </Popover>
  );
}
