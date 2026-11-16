/** No filters exist yet, so this is plain `string`; the first filter phase turns it into a union of its kinds. */
export type FilterKind = string;

export type ParamValue = number | string;

export interface FilterCfg {
  id: string;
  kind: FilterKind;
  enabled: boolean;
  params: Record<string, ParamValue>;
}

interface Slider {
  key: string;
  label: string;
  min: number;
  max: number;
  step: number;
  def: number;
  unit?: string;
}

interface Select {
  key: string;
  label: string;
  def: string;
  options: { value: string; label: string }[];
}

export type ParamDef = Slider | Select;

export const isSelect = (p: ParamDef): p is Select => "options" in p;

export interface FilterDef {
  label: string;
  params: ParamDef[];
}

export const FILTERS: Record<FilterKind, FilterDef> = {};

/** One-liners for the "Add filter" menu. */
export const FILTER_HINTS: Record<FilterKind, string> = {};

export const defaultParams = (kind: FilterKind): Record<string, ParamValue> =>
  Object.fromEntries(FILTERS[kind].params.map((p) => [p.key, p.def]));

export const makeFilter = (kind: FilterKind, enabled = true): FilterCfg => ({
  id: crypto.randomUUID(),
  kind,
  enabled,
  params: defaultParams(kind),
});

/** The chain a fresh install starts with: empty. */
export const defaultChain = (): FilterCfg[] => [];
