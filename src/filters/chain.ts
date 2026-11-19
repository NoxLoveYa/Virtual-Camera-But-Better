import {
  defOf,
  isBuiltinKind,
  isFilterKind,
  type ExtensionInfo,
  type FilterCfg,
  type ParamDef,
  type ParamValue,
} from "./schema";
import { snap, toColorInt } from "./values";

const part = (list: FilterCfg[]) =>
  list.map((f) => [f.kind, f.enabled, Object.entries(f.params).sort(([a], [b]) => (a < b ? -1 : 1))]);

/**
 * Fingerprint of what a chain (and the overlays above the output, when there are any) does, ignoring the random ids.
 * Equal fingerprints mean "no unsaved changes". Without overlays it is the chain's own fingerprint, as it always was,
 * so a baseline saved by an older version still matches.
 */
export const signature = (chain: FilterCfg[], overlays: FilterCfg[] = []): string =>
  JSON.stringify(overlays.length === 0 ? part(chain) : { chain: part(chain), overlays: part(overlays) });

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
    case "text":
      return typeof v === "string" ? v.slice(0, p.maxLength ?? 80) : p.def;
  }
};

const isObject = (v: unknown): v is Record<string, unknown> => !!v && typeof v === "object" && !Array.isArray(v);

const isValue = (v: unknown): v is ParamValue => typeof v === "number" || typeof v === "string" || typeof v === "boolean";

/**
 * The params of one entry for its filter kind. A kind with a definition (a built-in, an installed extension) gets every
 * param present with a valid value (hidden `showIf` params included) and unknown params dropped. An `ext:` kind
 * without one (not installed, or its manifest is invalid) keeps what it was given, so nothing is lost until the
 * extension is back.
 */
const paramsFor = (kind: FilterCfg["kind"], given: Record<string, unknown>, extensions: readonly ExtensionInfo[]): Record<string, ParamValue> => {
  const def = defOf(kind, extensions);
  if (!def) return Object.fromEntries(Object.entries(given).filter((e): e is [string, ParamValue] => isValue(e[1])));
  const params: Record<string, ParamValue> = Object.fromEntries(def.params.map((p) => [p.key, p.def]));
  for (const p of def.params) if (p.key in given) params[p.key] = coerceParam(p, given[p.key]);
  return params;
};

/**
 * Makes a chain read from disk (saved state, a preset file) safe to use: unknown filters dropped, every param present
 * with a valid value (hidden `showIf` params included), unknown params dropped, ids kept when valid and unique, else fresh.
 * An `ext:` entry is coerced against its extension's manifest when `extensions` has it, and kept as it is otherwise
 * (the extension may not be installed, or not read yet).
 */
export const normalizeChain = (raw: unknown, extensions: readonly ExtensionInfo[] = []): FilterCfg[] => {
  if (!Array.isArray(raw)) return [];
  const seen = new Set<string>();
  const out: FilterCfg[] = [];
  for (const f of raw) {
    if (!isObject(f) || !isFilterKind(f.kind)) continue;
    const given = isObject(f.params) ? f.params : {};
    const params = paramsFor(f.kind, given, extensions);
    const id = typeof f.id === "string" && f.id !== "" && !seen.has(f.id) ? f.id : crypto.randomUUID();
    seen.add(id);
    out.push({ id, kind: f.kind, enabled: f.enabled !== false, params });
  }
  return out;
};

/**
 * The chain with its `ext:` entries coerced against the extensions as they are now (the list arrived, or an extension
 * was edited and reloaded). The same array when nothing changed, so callers can compare by identity.
 */
export const refreshExtParams = (chain: FilterCfg[], extensions: readonly ExtensionInfo[]): FilterCfg[] => {
  let changed = false;
  const next = chain.map((f) => {
    if (isBuiltinKind(f.kind) || !defOf(f.kind, extensions)) return f;
    const params = paramsFor(f.kind, f.params, extensions);
    if (JSON.stringify(Object.entries(params).sort()) === JSON.stringify(Object.entries(f.params).sort())) return f;
    changed = true;
    return { ...f, params };
  });
  return changed ? next : chain;
};

/** Same rule the backend enforces for preset file names. */
export const PRESET_NAME = /^[\p{L}\p{N} _-]+$/u;
