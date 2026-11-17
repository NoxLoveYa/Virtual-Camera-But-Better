// The filter catalogue: the single source of truth for labels, ranges, steps and defaults (plan 4.5, 6.4).
// Rust reads the same defaults from `src-tauri/src/fx/defaults.json`, which `npm run schema:export` writes from FILTERS.
// This file must stay free of imports other than types: the export script loads it with Node's own TypeScript support.

export type ParamValue = number | string | boolean;

export interface FilterCfg {
  id: string;
  kind: FilterKind;
  enabled: boolean;
  params: Record<string, ParamValue>;
}

/** Every ParamDef may carry these. `showIf` hides the param (its value is kept) unless the named param has one of the values. */
interface ParamBase {
  key: string;
  label: string;
  showIf?: { key: string; in: ParamValue[] };
  hint?: string;
}

export interface SliderDef extends ParamBase {
  type: "slider";
  min: number;
  max: number;
  step: number;
  def: number;
  unit?: string;
  /** Whole numbers only (the value is rounded). */
  int?: boolean;
}
export interface SelectDef extends ParamBase {
  type: "select";
  def: string | number;
  options: { value: string | number; label: string }[];
}
export interface BoolDef extends ParamBase {
  type: "bool";
  def: boolean;
}
/** A number box with âˆ’/+ buttons, for crop and sizes. */
export interface IntDef extends ParamBase {
  type: "int";
  min: number;
  max: number;
  def: number;
  unit?: string;
}
/** OBS colour int 0xAABBGGRR. Without `alpha` the colour field writes RGB only and leaves the alpha byte as it is. */
export interface ColorDef extends ParamBase {
  type: "color";
  def: number;
  alpha?: boolean;
}
/** A file chosen with the file picker. */
export interface PathDef extends ParamBase {
  type: "path";
  def: "";
  filter: "image" | "lut";
}
/** A list of choices plus a typed value; typed values must match `pattern`. */
export interface ComboDef extends ParamBase {
  type: "combo";
  def: string;
  options: string[];
  pattern?: RegExp;
  /** Placeholder of the text box that "Customâ€¦" reveals. */
  placeholder?: string;
}

export type ParamDef = SliderDef | SelectDef | BoolDef | IntDef | ColorDef | PathDef | ComboDef;

export interface FilterDef {
  label: string;
  /** One line for the "Add filter" menu. */
  hint: string;
  params: ParamDef[];
  /** Names a special arrangement of the params in the card (`layout-<name>` on `.params`, `data-key` on every field). */
  layout?: "crop";
}

/**
 * Adding a filter = one entry here: the key is the OBS filter id. Keys, labels, ranges, steps and defaults are OBS's.
 * Then run `npm run schema:export`.
 */
const CATALOGUE = {
  color_filter: {
    label: "Color Correction",
    hint: "Brightness, contrast, saturation, hue and gamma",
    params: [
      { type: "slider", key: "gamma", label: "Gamma", min: -3, max: 3, step: 0.01, def: 0 },
      { type: "slider", key: "contrast", label: "Contrast", min: -4, max: 4, step: 0.01, def: 0 },
      { type: "slider", key: "brightness", label: "Brightness", min: -1, max: 1, step: 0.0001, def: 0 },
      { type: "slider", key: "saturation", label: "Saturation", min: -1, max: 5, step: 0.01, def: 0 },
      { type: "slider", key: "hue_shift", label: "Hue Shift", min: -180, max: 180, step: 0.01, def: 0 },
      { type: "slider", key: "opacity", label: "Opacity", min: 0, max: 1, step: 0.0001, def: 1 },
      { type: "color", key: "color_multiply", label: "Color Multiply", def: 0x00ffffff },
      { type: "color", key: "color_add", label: "Color Add", def: 0x00000000 },
    ],
  },
} satisfies Record<string, FilterDef>;

export type FilterKind = keyof typeof CATALOGUE;

export const FILTERS: Record<FilterKind, FilterDef> = CATALOGUE;

/** One-liners for the "Add filter" menu. */
export const FILTER_HINTS = Object.fromEntries(Object.entries(FILTERS).map(([k, f]) => [k, f.hint])) as Record<FilterKind, string>;

export const isFilterKind = (kind: unknown): kind is FilterKind =>
  typeof kind === "string" && Object.prototype.hasOwnProperty.call(FILTERS, kind);

export const defaultParams = (kind: FilterKind): Record<string, ParamValue> =>
  Object.fromEntries(FILTERS[kind].params.map((p) => [p.key, p.def]));

/** Whether a param is shown for these values (a hidden param keeps its value and still goes to the backend). */
export const isShown = (p: ParamDef, params: Record<string, ParamValue>): boolean =>
  !p.showIf || p.showIf.in.includes(params[p.showIf.key]);

export const makeFilter = (kind: FilterKind, enabled = true): FilterCfg => ({
  id: crypto.randomUUID(),
  kind,
  enabled,
  params: defaultParams(kind),
});

/** The chain a fresh install starts with: empty. */
export const defaultChain = (): FilterCfg[] => [];
