/**
 * Checks of how extension cards (`ext:<id>`) live in chains and presets (E3). `npm run test:ext` bundles and runs it.
 * It throws at the first failure and prints `ok` otherwise.
 */
import { PresetFile, normalizeChain, presetChain, presetFile, refreshExtParams, signature } from "./chain";
import { defOf, defaultParams, extOf, isFilterKind, labelOf, makeFilter, type ExtensionInfo, type FilterCfg } from "./schema";

function eq<T>(actual: T, expected: T, what: string) {
  if (actual !== expected) throw new Error(`${what}: expected ${String(expected)}, got ${String(actual)}`);
}
const same = (actual: unknown, expected: unknown, what: string) => eq(JSON.stringify(actual), JSON.stringify(expected), what);

const vignette = (over: Partial<ExtensionInfo> = {}): ExtensionInfo => ({
  id: "vignette",
  name: "Vignette",
  version: "1.0.0",
  author: "Better Cam examples",
  license: "MIT",
  hint: "Darken the edges",
  kind: "filter",
  render: null,
  api_version: 1,
  folder: "C:\\ext\\vignette",
  params: [
    { type: "slider", key: "amount", label: "Strength", min: 0, max: 1, step: 0.01, def: 0.6 },
    { type: "slider", key: "cell", label: "Pixel size", min: 1, max: 64, step: 1, def: 1, int: true },
    { type: "select", key: "mode", label: "Mode", def: "round", options: [{ value: "round", label: "Round" }, { value: "square", label: "Square" }] },
    { type: "bool", key: "on", label: "On", def: true },
    { type: "text", key: "label", label: "Label", def: "Hi", maxLength: 5 },
    { type: "combo", key: "size", label: "Size", def: "16:9", options: ["16:9"], pattern: "^\\d+x\\d+$" },
  ],
  actions: [],
  state: "ready",
  enabled: true,
  error: null,
  ...over,
});

const entry = (kind: string, params: Record<string, unknown>, extra: Record<string, unknown> = {}) => ({ id: "a", kind, enabled: true, params, ...extra });

// Kinds.
eq(isFilterKind("ext:vignette"), true, "ext kind");
eq(isFilterKind("ext:a"), false, "ext id too short");
eq(isFilterKind("ext:Bad Id"), false, "ext id with a space");
eq(isFilterKind("color_filter"), true, "built-in kind");
eq(isFilterKind("nope"), false, "unknown kind");

// A missing extension: the entry is kept as it is (only plain values), so nothing is lost until it is back.
const missing = normalizeChain([entry("ext:vignette", { amount: 5, label: "x", on: true, junk: { a: 1 }, list: [1] })]);
eq(missing.length, 1, "missing: kept");
same(missing[0].params, { amount: 5, label: "x", on: true }, "missing: params as they were, objects dropped");
eq(missing[0].kind, "ext:vignette", "missing: kind");
eq(normalizeChain([entry("ext:Bad Id", {}), entry("nope", {})]).length, 0, "bad kinds dropped");

// An installed extension: params coerced against its manifest.
const ext = [vignette()];
const coerced = normalizeChain([entry("ext:vignette", { amount: 7, cell: 2.6, mode: "triangle", label: "toolong", size: "640x480", gone: 1 })], ext)[0];
same(coerced.params, { amount: 1, cell: 3, mode: "round", on: true, label: "toolo", size: "640x480" }, "installed: clamped, rounded, defaults added, unknown dropped");
same(normalizeChain([entry("ext:vignette", { size: "nonsense" })], ext)[0].params.size, "16:9", "combo falls back to its default");
same(defaultParams("ext:vignette", ext), { amount: 0.6, cell: 1, mode: "round", on: true, label: "Hi", size: "16:9" }, "defaultParams of an extension");
same(defaultParams("ext:vignette"), {}, "defaultParams without the extension");
same(makeFilter("ext:vignette", true, ext).params.amount, 0.6, "makeFilter fills defaults");

// Built-ins are unchanged by passing extensions.
same(normalizeChain([entry("color_filter", { gamma: 99, saturation: "x" })], ext)[0].params.gamma, 3, "built-in clamped");

// A manifest that failed to load has no params to coerce against: the entry stays as it was.
const broken = vignette({ state: "error", params: [], error: "extension.json: version: must be a semantic version" });
same(normalizeChain([entry("ext:vignette", { amount: 5 })], [broken])[0].params, { amount: 5 }, "error state keeps params");
eq(defOf("ext:vignette", [broken]), undefined, "no def for a broken extension");
eq(defOf("ext:vignette", []), undefined, "no def for a missing extension");
eq(defOf("ext:vignette", [vignette({ state: "disabled", enabled: false })])?.label, "Vignette", "a switched-off extension keeps its def");

// Names and definitions.
eq(labelOf("ext:vignette", ext), "Vignette", "label of an installed extension");
eq(labelOf("ext:vignette", []), "vignette", "label of a missing extension is its id");
eq(labelOf("color_filter"), "Color Correction", "label of a built-in");
eq(extOf("ext:vignette", ext)?.id, "vignette", "extOf");
eq(extOf("color_filter", ext), undefined, "extOf for a built-in");
const combo = defOf("ext:vignette", ext)?.params.find((p) => p.key === "size");
eq(combo?.type === "combo" && combo.pattern instanceof RegExp, true, "a combo pattern becomes a RegExp");

// refreshExtParams: same array when nothing changes, new entries when the manifest did.
const chain: FilterCfg[] = normalizeChain([entry("ext:vignette", {}), entry("color_filter", {}, { id: "b" }), entry("ext:other", { q: 1 }, { id: "c" })], ext);
eq(refreshExtParams(chain, ext), chain, "unchanged chain is the same array");
eq(refreshExtParams(chain, []), chain, "no extensions: nothing to refresh");
const edited = vignette({ params: [...vignette().params.filter((p) => p.key !== "cell"), { type: "bool", key: "soft", label: "Soft", def: true }] });
const refreshed = refreshExtParams(chain, [edited]);
eq(refreshed === chain, false, "a changed manifest gives a new chain");
same(Object.keys(refreshed[0].params).sort(), ["amount", "label", "mode", "on", "size", "soft"], "dropped param gone, new one added");
eq(refreshed[1], chain[1], "built-in entry untouched");
eq(refreshed[2], chain[2], "entry of a missing extension untouched");

// signature: the chain's own fingerprint is unchanged without overlays; it covers overlays when there are some.
const color = normalizeChain([entry("color_filter", { gamma: 1 })]);
eq(signature(color), JSON.stringify([["color_filter", true, Object.entries(color[0].params).sort(([a], [b]) => (a < b ? -1 : 1))]]), "signature without overlays keeps its old form");
eq(signature(color, []), signature(color), "empty overlays change nothing");
const overlay = normalizeChain([entry("ext:clock", { size: 40 })], []);
const overlay2 = normalizeChain([entry("ext:clock", { size: 41 })], []);
eq(signature(color, overlay) !== signature(color), true, "an overlay changes the signature");
eq(signature(color, overlay) !== signature(color, overlay2), true, "an overlay's param changes the signature");
eq(signature(color, overlay), signature(color, normalizeChain([entry("ext:clock", { size: 40 })], [])), "same overlay, same signature (ids are ignored)");
eq(signature(color, overlay) !== signature(overlay, color), true, "chain and overlays are not interchangeable");

// Presets: an ext entry survives a save and load, with or without the extension; `overlays` is written only when there are some.
const saved = presetFile(chain);
eq("overlays" in saved, false, "no overlays key without overlays");
eq(presetFile(chain, overlay).overlays?.[0].kind, "ext:clock", "overlays written when present");
const file = JSON.parse(JSON.stringify(presetFile(chain, overlay))) as PresetFile;
const back = presetChain(file, []);
same(back.map((f) => f.kind), ["ext:vignette", "color_filter", "ext:other"], "preset load keeps ext entries, ignores overlays");
same(back[0].params, chain[0].params, "preset load without the extension keeps the params");
same(presetChain(file, [edited])[0].params, refreshed[0].params, "preset load with the extension coerces");

console.log("ok");
