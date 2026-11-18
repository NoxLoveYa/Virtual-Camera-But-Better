import { memo, useId, type CSSProperties, type KeyboardEvent, type ReactNode } from "react";
import { isShown, type ParamDef, type ParamValue, type SliderDef } from "../filters/schema";
import { formatValue, snap, toColorInt } from "../filters/values";
import { ColorField } from "./ColorField";
import { ComboField } from "./ComboField";
import { Icon } from "./Icon";
import { NumberField } from "./NumberField";
import { PathField } from "./PathField";
import { Select } from "./Select";

interface Props {
  def: ParamDef;
  /** All params of the filter: the value of this one and, for `showIf`, the one it depends on. */
  params: Record<string, ParamValue>;
  onChange: (key: string, value: ParamValue) => void;
}

/** One parameter of a filter card: a single switch on the definition's type. Renders nothing while its `showIf` says hidden. */
export const ParamField = memo(ParamFieldView, (a, b) => {
  // The field reads its own value and the one its `showIf` names: a change of any other param of the card leaves it be.
  const own = a.def.key;
  const cond = a.def.showIf?.key;
  return a.def === b.def && a.onChange === b.onChange && a.params[own] === b.params[own] && (cond === undefined || a.params[cond] === b.params[cond]);
});

function ParamFieldView({ def, params, onChange }: Props) {
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

    case "text":
      return (
        <label data-key={def.key} title={def.hint}>
          {def.label}
          <input type="text" value={String(value)} maxLength={def.maxLength ?? 80} aria-label={def.label} spellCheck={false} onChange={(e) => set(e.target.value)} />
          {hint}
        </label>
      );

    case "path":
      return (
        <div className="pf pf-wide" data-key={def.key} title={def.hint}>
          <span>{def.label}</span>
          <PathField value={String(value)} kind={def.filter} label={def.label} onChange={set} />
          {hint}
        </div>
      );
  }
}

function Slider({ def, value, onChange, hint }: { def: SliderDef; value: number; onChange: (v: number) => void; hint: ReactNode }) {
  const fill = ((value - def.min) / (def.max - def.min)) * 100;
  const id = useId();
  const reset = () => onChange(def.def);
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === "Delete") {
      e.preventDefault();
      reset();
    }
  };
  const text = (v: number) => formatValue(v, def.int ? 1 : def.step, def.unit);
  return (
    // `htmlFor`: the reset button inside is the first labelable descendant, which would take the label's clicks.
    <label data-key={def.key} title={def.hint} htmlFor={id}>
      <span>
        {def.label}
        <span className="pf-val">
          <b>{text(value)}</b>
          {value !== def.def && (
            <button
              type="button"
              className="reset-btn"
              aria-label={`Reset ${def.label}`}
              title={`Reset to ${text(def.def)}`}
              onClick={() => {
                reset();
                // The button leaves with the change: keep the keyboard on the slider.
                document.getElementById(id)?.focus();
              }}
            >
              <Icon name="reset" size={16} />
            </button>
          )}
        </span>
      </span>
      <input
        id={id}
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
