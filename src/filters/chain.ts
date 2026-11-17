import { FILTERS, defaultChain, defaultParams, isFilterKind, type FilterCfg, type ParamDef, type ParamValue } from "./schema";
import { snap, toColorInt } from "./values";

/** Fingerprint of what a chain does, ignoring the random ids. Equal fingerprints mean "no unsaved changes". */
export const signature = (chain: FilterCfg[]): string =>
  JSON.stringify(
    chain.map((f) => [
      f.kind,
      f.enabled,
      Object.entries(f.params).sort(([a], [b]) => (a < b ? -1 : 1)),
    ]),
  );

export const DEFAULT_SIGNATURE = signature(defaultChain());

const finite = (v: unknown): number | null => {
  if (typeof v === "number") return Number.isFinite(v) ? v : null;
  if (typeof v === "string" && v.trim() !== "") {
    const n = Number(v);
    return Number.isFinite(n) ? n : null;
  }
  return null;
};

const clamp = (n: number, min: number, max: number) => Math.min(max, Math.max(min, n));

/** One value made valid for its definition: right type, inside its range, one of its options. Anything else gives the default. */
export const coerceParam = (p: ParamDef, v: unknown): ParamValue => {
  switch (p.type) {
    case "slider": {
      const n = finite(v);
      if (n === null) return p.def;
      const c = clamp(p.int ? Math.round(n) : n, p.min, p.max);
      return p.int ? c : snap(c, p.step);
    }
    case "int": {
      const n = finite(v);
      return n === null ? p.def : clamp(Math.round(n), p.min, p.max);
    }
    case "bool":
      if (typeof v === "boolean") return v;
      if (v === 1 || v === "true") return true;
      if (v === 0 || v === "false") return false;
      return p.def;
    case "color":
      return toColorInt(finite(v), p.def);
    case "select": {
      const hit = p.options.find((o) => o.value === v) ?? p.options.find((o) => String(o.value) === String(v));
      return hit ? hit.value : p.def;
    }
    case "combo":
      if (typeof v !== "string") return p.def;
      return p.options.includes(v) || (p.pattern ? p.pattern.test(v) : false) ? v : p.def;
    case "path":
      return typeof v === "string" ? v : p.def;
  }
};

const isObject = (v: unknown): v is Record<string, unknown> => !!v && typeof v === "object" && !Array.isArray(v);

/**
 * Makes a chain read from disk (saved state, a preset file) safe to use: unknown filters dropped, every param present
 * with a valid value (hidden `showIf` params included), unknown params dropped, ids kept when valid and unique, else fresh.
 */
export const normalizeChain = (raw: unknown): FilterCfg[] => {
  if (!Array.isArray(raw)) return [];
  const seen = new Set<string>();
  const out: FilterCfg[] = [];
  for (const f of raw) {
    if (!isObject(f) || !isFilterKind(f.kind)) continue;
    const given = isObject(f.params) ? f.params : {};
    const params = defaultParams(f.kind);
    for (const p of FILTERS[f.kind].params) if (p.key in given) params[p.key] = coerceParam(p, given[p.key]);
    const id = typeof f.id === "string" && f.id !== "" && !seen.has(f.id) ? f.id : crypto.randomUUID();
    seen.add(id);
    out.push({ id, kind: f.kind, enabled: f.enabled !== false, params });
  }
  return out;
};

/** Same rule the backend enforces for preset file names. */
export const PRESET_NAME = /^[\p{L}\p{N} _-]+$/u;

/** The preset file of plan 5.4: the chain without ids. Presets hold filters only, never device controls. */
export interface PresetFile {
  app: "better-cam";
  version: 1;
  chain: { kind: string; enabled: boolean; params: Record<string, ParamValue> }[];
}

export const presetFile = (chain: FilterCfg[]): PresetFile => ({
  app: "better-cam",
  version: 1,
  chain: chain.map(({ kind, enabled, params }) => ({ kind, enabled, params })),
});

/** The chain inside a preset file: the object of plan 5.4 or the old bare array. Other sections (`camera`) are ignored. */
export const presetChain = (raw: unknown): FilterCfg[] => {
  const list = Array.isArray(raw) ? raw : isObject(raw) ? raw.chain : undefined;
  if (!Array.isArray(list)) throw new Error("This is not a Better Cam preset");
  return normalizeChain(list);
};
