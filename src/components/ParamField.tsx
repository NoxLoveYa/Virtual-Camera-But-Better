import type { CSSProperties, KeyboardEvent, ReactNode } from "react";
import { isShown, type ParamDef, type ParamValue, type SliderDef } from "../filters/schema";
import { formatValue, snap, toColorInt } from "../filters/values";
import { ColorField } from "./ColorField";
import { ComboField } from "./ComboField";
import { NumberField } from "./NumberField";
import { Select } from "./Select";

interface Props {
  def: ParamDef;
  /** All params of the filter: the value of this one and, for `showIf`, the one it depends on. */
  params: Record<string, ParamValue>;
  onChange: (key: string, value: ParamValue) => void;
}

/** One parameter of a filter card: a single switch on the definition's type. Renders nothing while its `showIf` says hidden. */
export function ParamField({ def, params, onChange }: Props) {
  if (!isShown(def, params)) return null;
  const value = params[def.key] ?? def.def;
  const set = (v: ParamValue) => onChange(def.key, v);
  const hint = def.hint && <small className="pf-hint">{def.hint}</small>;

  switch (def.type) {
    case "slider":
      return <Slider def={def} value={Number(value)} onChange={set} hint={hint} />;

    case "select":
      return (
        <label data-key={def.key} title={def.hint}>
          {def.label}
          <Select
            value={String(value)}
            options={def.options.map((o) => ({ value: String(o.value), label: o.label }))}
            onChange={(v) => set(def.options.find((o) => String(o.value) === v)?.value ?? def.def)}
          />
          {hint}
        </label>
      );

    case "bool":
      return (
        <label className="pf-row" data-key={def.key} title={def.hint}>
          <span>{def.label}</span>
          <button type="button" role="switch" aria-checked={value === true} aria-label={def.label} className="switch" onClick={() => set(value !== true)} />
          {hint}
        </label>
      );

    case "int":
      return (
        <div className="pf" data-key={def.key} title={def.hint}>
          <span>{def.label}</span>
          <NumberField value={Number(value)} min={def.min} max={def.max} unit={def.unit} label={def.label} onChange={set} />
          {hint}
        </div>
      );

    case "color":
      return (
        <div className="pf" data-key={def.key} title={def.hint}>
          <span>{def.label}</span>
          <ColorField value={toColorInt(value, def.def)} alpha={def.alpha} label={def.label} onChange={set} />
          {hint}
        </div>
      );

    case "combo":
      return (
        <div className="pf" data-key={def.key} title={def.hint}>
          <span>{def.label}</span>
          <ComboField value={String(value)} options={def.options} pattern={def.pattern} placeholder={def.placeholder} label={def.label} onChange={set} />
          {hint}
        </div>
      );

    case "path":
      // The file picker command does not exist yet; the first filter that needs it brings PathField.
      return (
        <div className="pf" data-key={def.key} title={def.hint}>
          <span>{def.label}</span>
          <div className="pf-path" aria-disabled="true">
            File picker arrives with the first filter that needs it
          </div>
          {hint}
        </div>
      );
  }
}

function Slider({ def, value, onChange, hint }: { def: SliderDef; value: number; onChange: (v: number) => void; hint: ReactNode }) {
  const fill = ((value - def.min) / (def.max - def.min)) * 100;
  const reset = () => onChange(def.def);
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === "Delete") {
      e.preventDefault();
      reset();
    }
  };
  return (
    <label data-key={def.key} title={def.hint}>
      <span>
        {def.label}
        <b>{formatValue(value, def.int ? 1 : def.step, def.unit)}</b>
      </span>
      <input
        type="range"
        aria-label={def.label}
        title="Double-click or press Delete to reset"
        style={{ "--p": `${fill}%` } as CSSProperties}
        min={def.min}
        max={def.max}
        step={def.step}
        value={value}
        onChange={(e) => onChange(snap(Number(e.target.value), def.step))}
        onDoubleClick={reset}
        onKeyDown={onKeyDown}
      />
      {hint}
    </label>
  );
}
