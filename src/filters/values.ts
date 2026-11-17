// Small pure helpers for parameter values: number text and OBS colour ints. No imports, so scripts can load it directly.

/** How many decimals a step has: 1 → 0, 0.5 → 1, 0.01 → 2, 0.0001 → 4. */
export const stepDecimals = (step: number): number => {
  const [mantissa, exp = "0"] = String(step).toLowerCase().split("e");
  const frac = mantissa.split(".")[1]?.length ?? 0;
  return Math.min(8, Math.max(0, frac - Number(exp)));
};

/** Rounds to the precision of `step`, so a value that went through float arithmetic prints and compares cleanly. */
export const snap = (value: number, step: number): number => Number(value.toFixed(stepDecimals(step)));

/** The value as text with exactly the step's precision ("0.0100" never becomes "0.010000000000000002"). */
export const formatValue = (value: number, step: number, unit?: string): string =>
  `${value.toFixed(stepDecimals(step))}${unit ? ` ${unit}` : ""}`;

// ---- OBS colour ints: 0xAABBGGRR (alpha in the top byte, then blue, green, red), kept as unsigned numbers ----

const byte = (n: number) => Math.max(0, Math.min(255, Math.round(n)));

/** The int as an unsigned 32-bit number; anything that is not a finite number gives `fallback`. */
export const toColorInt = (v: unknown, fallback = 0): number =>
  typeof v === "number" && Number.isFinite(v) ? Math.trunc(v) >>> 0 : fallback;

export const colorAlpha = (c: number): number => (c >>> 24) & 0xff;

/** "#rrggbb" of the colour (alpha is not part of it). */
export const colorToHex = (c: number): string => {
  const part = (shift: number) => ((c >>> shift) & 0xff).toString(16).padStart(2, "0");
  return `#${part(0)}${part(8)}${part(16)}`;
};

/** "#rrggbb", "rrggbb" or "#rgb" to [r, g, b]; null when it is not a colour. */
export const parseHex = (text: string): [number, number, number] | null => {
  const m = /^#?([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(text.trim());
  if (!m) return null;
  const h = m[1].length === 3 ? [...m[1]].map((c) => c + c).join("") : m[1];
  return [parseInt(h.slice(0, 2), 16), parseInt(h.slice(2, 4), 16), parseInt(h.slice(4, 6), 16)];
};

/** The colour with new red, green and blue and the alpha byte of `c` (or `alpha` when given). */
export const withRgb = (c: number, r: number, g: number, b: number, alpha = colorAlpha(c)): number =>
  (((byte(alpha) << 24) | (byte(b) << 16) | (byte(g) << 8) | byte(r)) >>> 0);

/** The colour with a new alpha byte and the same red, green and blue. */
export const withAlpha = (c: number, alpha: number): number => (((byte(alpha) << 24) | (c & 0x00ffffff)) >>> 0);

/** The colour for hex text, keeping the alpha byte of `c`; null when the text is not a colour. */
export const hexToColor = (text: string, c: number): number | null => {
  const rgb = parseHex(text);
  return rgb ? withRgb(c, ...rgb) : null;
};
