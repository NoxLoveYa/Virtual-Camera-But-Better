// The filter catalogue: the single source of truth for labels, ranges, steps and defaults (plan 4.5, 6.4).
// Rust reads the same defaults from `src-tauri/src/fx/defaults.json`, which `npm run schema:export` writes from FILTERS.
// This file must stay free of imports other than types: the export script loads it with Node's own TypeScript support.

export type ParamValue = number | string | boolean;

export interface FilterCfg {
  id: string;
  /** A built-in filter's OBS id, or `ext:<id>` for a card of an installed extension (it stays when the extension is gone). */
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

/** A line of text typed by the user (extensions only). */
export interface TextDef extends ParamBase {
  type: "text";
  def: string;
  /** At most 200, default 80. */
  maxLength?: number;
}

export type ParamDef = SliderDef | SelectDef | BoolDef | IntDef | ColorDef | PathDef | ComboDef | TextDef;

export interface FilterDef {
  label: string;
  /** One line for the "Add filter" menu. */
  hint: string;
  params: ParamDef[];
  /** Names a special arrangement of the params in the card (`layout-<name>` on `.params`, `data-key` on every field). */
  layout?: "crop";
  /** Runs on NVIDIA's Video Effects runtime and needs an RTX GPU: the card explains what is missing, the menu says "RTX only". */
  rtx?: boolean;
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
  // Geometry (P10)
  crop_filter: {
    label: "Crop/Pad",
    hint: "Trim edges; negative values add padding",
    layout: "crop",
    params: [
      { type: "bool", key: "relative", label: "Relative", def: true, hint: "Off: a window at Left and Top; a Width or Height of 0 runs to the edge" },
      { type: "int", key: "left", label: "Left", min: -8192, max: 8192, def: 0 },
      { type: "int", key: "top", label: "Top", min: -8192, max: 8192, def: 0 },
      { type: "int", key: "right", label: "Right", min: -8192, max: 8192, def: 0, showIf: { key: "relative", in: [true] } },
      { type: "int", key: "bottom", label: "Bottom", min: -8192, max: 8192, def: 0, showIf: { key: "relative", in: [true] } },
      { type: "int", key: "cx", label: "Width", min: 0, max: 8192, def: 0, showIf: { key: "relative", in: [false] } },
      { type: "int", key: "cy", label: "Height", min: 0, max: 8192, def: 0, showIf: { key: "relative", in: [false] } },
    ],
  },
  scale_filter: {
    label: "Scaling/Aspect Ratio",
    hint: "Resize the image or force an aspect ratio",
    params: [
      {
        type: "select",
        key: "sampling",
        label: "Scale Filtering",
        def: "bicubic",
        options: [
          { value: "point", label: "Point" },
          { value: "bilinear", label: "Bilinear" },
          { value: "bicubic", label: "Bicubic (16 samples)" },
          { value: "lanczos", label: "Lanczos (32 samples)" },
          { value: "area", label: "Area (4/6/9 samples)" },
        ],
      },
      {
        type: "combo",
        key: "resolution",
        label: "Resolution",
        def: "None",
        options: ["None", "Base (Canvas) Resolution", "16:9", "16:10", "4:3", "1:1"],
        pattern: /^([1-9]\d{0,4}x[1-9]\d{0,4}|[1-9]\d{0,2}:[1-9]\d{0,2})$/,
        placeholder: "1280x720 or 4:3",
        hint: "A wider aspect ratio keeps the height and sets the width; a narrower one keeps the width and sets the height",
      },
      {
        type: "bool",
        key: "undistort",
        label: "Undistort center of image when scaling from ultrawide",
        def: false,
        showIf: { key: "sampling", in: ["bicubic", "lanczos"] },
      },
    ],
  },
  transform: {
    label: "Transform",
    hint: "Mirror or rotate the image",
    params: [
      { type: "bool", key: "flip_h", label: "Flip horizontal (mirror)", def: false },
      { type: "bool", key: "flip_v", label: "Flip vertical", def: false },
      { type: "slider", key: "rotation", label: "Rotation", min: -180, max: 180, step: 0.1, def: 0, unit: "°" },
    ],
  },
  sharpness_filter: {
    label: "Sharpen",
    hint: "Crisper edges (unsharp mask)",
    params: [{ type: "slider", key: "sharpness", label: "Sharpness", min: 0, max: 1, step: 0.01, def: 0.08 }],
  },
  clut_filter: {
    label: "Apply LUT",
    hint: "Color grade with a .cube or .png LUT",
    params: [
      { type: "path", key: "image_path", label: "Path", def: "", filter: "lut" },
      { type: "slider", key: "clut_amount", label: "Amount", min: 0, max: 1, step: 0.0001, def: 1 },
      { type: "bool", key: "passthrough_alpha", label: "Passthrough Alpha", def: false },
    ],
  },
  // Keys and Image Mask/Blend (P12)
  chroma_key_filter: {
    label: "Chroma Key",
    hint: "Remove a green or blue screen",
    params: [
      {
        type: "select",
        key: "key_color_type",
        label: "Key Color Type",
        def: "green",
        options: [
          { value: "green", label: "Green" },
          { value: "blue", label: "Blue" },
          { value: "magenta", label: "Magenta" },
          { value: "custom", label: "Custom Color" },
        ],
      },
      { type: "color", key: "key_color", label: "Key Color", def: 0x0000ff00, showIf: { key: "key_color_type", in: ["custom"] } },
      { type: "slider", key: "similarity", label: "Similarity (1-1000)", min: 1, max: 1000, step: 1, def: 400, int: true },
      { type: "slider", key: "smoothness", label: "Smoothness (1-1000)", min: 1, max: 1000, step: 1, def: 80, int: true },
      { type: "slider", key: "spill", label: "Key Color Spill Reduction (1-1000)", min: 1, max: 1000, step: 1, def: 100, int: true },
      { type: "slider", key: "opacity", label: "Opacity", min: 0, max: 1, step: 0.0001, def: 1 },
      { type: "slider", key: "contrast", label: "Contrast", min: -4, max: 4, step: 0.01, def: 0 },
      { type: "slider", key: "brightness", label: "Brightness", min: -1, max: 1, step: 0.0001, def: 0 },
      { type: "slider", key: "gamma", label: "Gamma", min: -1, max: 1, step: 0.01, def: 0 },
    ],
  },
  color_key_filter: {
    label: "Color Key",
    hint: "Make one color transparent",
    params: [
      {
        type: "select",
        key: "key_color_type",
        label: "Key Color Type",
        def: "green",
        options: [
          { value: "green", label: "Green" },
          { value: "blue", label: "Blue" },
          { value: "red", label: "Red" },
          { value: "magenta", label: "Magenta" },
          { value: "custom", label: "Custom Color" },
        ],
      },
      { type: "color", key: "key_color", label: "Key Color", def: 0x0000ff00, showIf: { key: "key_color_type", in: ["custom"] } },
      { type: "slider", key: "similarity", label: "Similarity (1-1000)", min: 1, max: 1000, step: 1, def: 80, int: true },
      { type: "slider", key: "smoothness", label: "Smoothness (1-1000)", min: 1, max: 1000, step: 1, def: 50, int: true },
      { type: "slider", key: "opacity", label: "Opacity", min: 0, max: 1, step: 0.0001, def: 1 },
      { type: "slider", key: "contrast", label: "Contrast", min: -4, max: 4, step: 0.01, def: 0 },
      { type: "slider", key: "brightness", label: "Brightness", min: -1, max: 1, step: 0.0001, def: 0 },
      { type: "slider", key: "gamma", label: "Gamma", min: -1, max: 1, step: 0.01, def: 0 },
    ],
  },
  luma_key_filter: {
    label: "Luma Key",
    hint: "Make dark or bright areas transparent",
    params: [
      { type: "slider", key: "luma_max", label: "Luma Max", min: 0, max: 1, step: 0.0001, def: 1 },
      { type: "slider", key: "luma_max_smooth", label: "Luma Max Smooth", min: 0, max: 1, step: 0.0001, def: 0 },
      { type: "slider", key: "luma_min", label: "Luma Min", min: 0, max: 1, step: 0.0001, def: 0 },
      { type: "slider", key: "luma_min_smooth", label: "Luma Min Smooth", min: 0, max: 1, step: 0.0001, def: 0 },
    ],
  },
  mask_filter: {
    label: "Image Mask/Blend",
    hint: "Shape or blend the image with a picture",
    params: [
      {
        type: "select",
        key: "type",
        label: "Type",
        def: "mask_color_filter.effect",
        options: [
          { value: "mask_color_filter.effect", label: "Alpha Mask (Color Channel)" },
          { value: "mask_alpha_filter.effect", label: "Alpha Mask (Alpha Channel)" },
          { value: "blend_mul_filter.effect", label: "Blend (Multiply)" },
          { value: "blend_add_filter.effect", label: "Blend (Addition)" },
          { value: "blend_sub_filter.effect", label: "Blend (Subtraction)" },
        ],
      },
      { type: "path", key: "image_path", label: "Path", def: "", filter: "image" },
      { type: "color", key: "color", label: "Color", def: 0x00ffffff },
      { type: "slider", key: "opacity", label: "Opacity", min: 0, max: 1, step: 0.0001, def: 1 },
      { type: "bool", key: "stretch", label: "Stretch Image (discard image aspect ratio)", def: false },
    ],
  },
  // Auto Adjust (P16): Better Cam's own filter, no OBS counterpart.
  auto_adjust: {
    label: "Auto Adjust",
    hint: "Automatic exposure, white balance, contrast and saturation in software",
    params: [
      { type: "bool", key: "exposure", label: "Auto exposure", def: true },
      { type: "slider", key: "target", label: "Target brightness", min: 0.2, max: 0.8, step: 0.01, def: 0.45, showIf: { key: "exposure", in: [true] } },
      { type: "bool", key: "contrast", label: "Auto contrast", def: false },
      { type: "slider", key: "target_contrast", label: "Target contrast", min: 0.1, max: 0.9, step: 0.01, def: 0.4, showIf: { key: "contrast", in: [true] }, hint: "Spread of dark to bright: 0.1 flat, 0.9 punchy" },
      { type: "bool", key: "saturation", label: "Auto saturation", def: false },
      { type: "slider", key: "target_saturation", label: "Target saturation", min: 0.05, max: 0.5, step: 0.01, def: 0.2, showIf: { key: "saturation", in: [true] }, hint: "Colour strength: 0.05 gray, 0.5 vivid" },
      { type: "bool", key: "white_balance", label: "Auto white balance", def: true },
      { type: "slider", key: "temperature", label: "Warmth", min: -1, max: 1, step: 0.01, def: 0, showIf: { key: "white_balance", in: [true] }, hint: "0 is neutral. Up keeps warm light a little warm, down cools." },
      { type: "slider", key: "tint", label: "Tint", min: -1, max: 1, step: 0.01, def: 0, showIf: { key: "white_balance", in: [true] }, hint: "Aim toward magenta (up) or green (down)" },
      { type: "slider", key: "strength", label: "Strength", min: 0, max: 1, step: 0.01, def: 1 },
      { type: "slider", key: "speed", label: "Adaptation speed", min: 0.05, max: 1, step: 0.01, def: 0.3 },
    ],
  },
  // NVIDIA Video Effects (P13b): keys, ranges and defaults are OBS's nv-filters.
  nv_greenscreen_filter: {
    label: "NVIDIA Background Removal",
    hint: "AI background removal (RTX only)",
    rtx: true,
    params: [
      {
        type: "select",
        key: "mode",
        label: "Mode",
        def: 0,
        options: [
          { value: 0, label: "Quality" },
          { value: 1, label: "Performance" },
          { value: 2, label: "Quality + chair removal" },
          { value: 3, label: "Performance + chair removal" },
        ],
      },
      { type: "slider", key: "threshold", label: "Threshold", min: 0, max: 1, step: 0.05, def: 1 },
      {
        type: "slider",
        key: "processing_interval",
        label: "Mask refresh frequency in frames",
        min: 1,
        max: 4,
        step: 1,
        def: 1,
        int: true,
        hint: "The mask is made every this many frames and reused in between. Higher is lighter on the GPU",
      },
    ],
  },
  nv_blur_filter: {
    label: "NVIDIA Blur",
    hint: "Blur the whole image (RTX only)",
    rtx: true,
    params: [{ type: "slider", key: "intensity", label: "Blur Intensity", min: 0, max: 1, step: 0.05, def: 0.5 }],
  },
  nv_background_blur_filter: {
    label: "NVIDIA Background Blur",
    hint: "Blur only the background (RTX only)",
    rtx: true,
    params: [{ type: "slider", key: "intensity", label: "Blur Intensity", min: 0, max: 1, step: 0.05, def: 0.5 }],
  },
} satisfies Record<string, FilterDef>;

/** The built-in filters: the keys of the catalogue. */
export type BuiltinKind = keyof typeof CATALOGUE;

/** A built-in filter, or `ext:<extension id>`. */
export type FilterKind = BuiltinKind | `ext:${string}`;

export const FILTERS: Record<BuiltinKind, FilterDef> = CATALOGUE;

/** One-liners for the "Add filter" menu. */
export const FILTER_HINTS = Object.fromEntries(Object.entries(FILTERS).map(([k, f]) => [k, f.hint])) as Record<BuiltinKind, string>;

export const isBuiltinKind = (kind: unknown): kind is BuiltinKind =>
  typeof kind === "string" && Object.prototype.hasOwnProperty.call(FILTERS, kind);

/** `ext:` plus an extension id (`^[a-z0-9][a-z0-9-]{1,47}$`). */
const EXT_KIND = /^ext:[a-z0-9][a-z0-9-]{1,47}$/;

export const isExtKind = (kind: unknown): kind is `ext:${string}` => typeof kind === "string" && EXT_KIND.test(kind);

export const isFilterKind = (kind: unknown): kind is FilterKind => isBuiltinKind(kind) || isExtKind(kind);

// ---- Extensions (docs/EXTENSIONS.md): what the app reports about the folders under %APPDATA%\dev.bettercam.app\extensions ----

/** A `ParamDef` as the backend writes it: a combo's `pattern` is the source of a regular expression, not a `RegExp`. */
export type ExtParamJson = Exclude<ParamDef, ComboDef> | (Omit<ComboDef, "pattern"> & { pattern?: string });

/** `list_extensions` and the `extensions-changed` event. */
export interface ExtensionInfo {
  /** The folder name; `ext:<id>` is the kind of its cards. */
  id: string;
  name: string;
  version: string;
  author: string | null;
  license: string | null;
  hint: string | null;
  /** `null` when the manifest could not be read. */
  kind: "filter" | "overlay" | "widget" | null;
  render: "scene" | "shader" | null;
  api_version: number | null;
  folder: string;
  /** Empty when the manifest is invalid. */
  params: ExtParamJson[];
  actions: { id: string; label: string }[];
  /** `ready`; `disabled`: switched off in Settings, its cards are bypassed; `error`: see `error`. */
  state: "ready" | "disabled" | "error";
  enabled: boolean;
  error: string | null;
}

export const extIdOf = (kind: string): string => kind.slice("ext:".length);

/** The installed extension behind an `ext:` kind (also one that failed to load, or is switched off). */
export const extOf = (kind: string, extensions: readonly ExtensionInfo[]): ExtensionInfo | undefined =>
  isExtKind(kind) ? extensions.find((e) => e.id === extIdOf(kind)) : undefined;

const regexOf = (source: string): RegExp | undefined => {
  try {
    return new RegExp(source);
  } catch {
    return undefined; // a pattern the page cannot compile limits nothing
  }
};

const toParamDef = (p: ExtParamJson): ParamDef => {
  if (p.type !== "combo") return p;
  const { pattern, ...rest } = p;
  const re = pattern === undefined ? undefined : regexOf(pattern);
  return re ? { ...rest, pattern: re } : rest;
};

const defCache = new WeakMap<ExtensionInfo, FilterDef>();

/**
 * The definition of a filter kind: the catalogue entry of a built-in, the manifest of an installed extension (a
 * disabled one included, its cards keep their values), `undefined` for an `ext:` kind that is not installed or whose
 * manifest is invalid (its params are unknown).
 */
export const defOf = (kind: string, extensions: readonly ExtensionInfo[] = []): FilterDef | undefined => {
  if (isBuiltinKind(kind)) return FILTERS[kind];
  const ext = extOf(kind, extensions);
  if (!ext || ext.state === "error") return undefined;
  let def = defCache.get(ext);
  if (!def) {
    def = { label: ext.name, hint: ext.hint ?? "", params: ext.params.map(toParamDef) };
    defCache.set(ext, def);
  }
  return def;
};

/** The name a card shows: the label of a built-in, the extension's name, or its id when it is not installed. */
export const labelOf = (kind: string, extensions: readonly ExtensionInfo[] = []): string =>
  isBuiltinKind(kind) ? FILTERS[kind].label : (extOf(kind, extensions)?.name ?? (isExtKind(kind) ? extIdOf(kind) : kind));

export const defaultParams = (kind: FilterKind, extensions: readonly ExtensionInfo[] = []): Record<string, ParamValue> =>
  Object.fromEntries((defOf(kind, extensions)?.params ?? []).map((p) => [p.key, p.def]));

/** Whether a param is shown for these values (a hidden param keeps its value and still goes to the backend). */
export const isShown = (p: ParamDef, params: Record<string, ParamValue>): boolean =>
  !p.showIf || p.showIf.in.includes(params[p.showIf.key]);

export const makeFilter = (kind: FilterKind, enabled = true, extensions: readonly ExtensionInfo[] = []): FilterCfg => ({
  id: crypto.randomUUID(),
  kind,
  enabled,
  params: defaultParams(kind, extensions),
});

/** The chain a fresh install starts with: empty. */
export const defaultChain = (): FilterCfg[] => [];
