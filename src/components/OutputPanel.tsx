import { useStore } from "../store";
import {
  FIT_OPTIONS,
  OUTPUT_FPS_OPTIONS,
  OUTPUT_RESOLUTION_OPTIONS,
  abgrToHex,
  hexToAbgr,
  type FitMode,
  type Option,
} from "../camera/schema";
import { Field } from "./Field";
import { Select } from "./Select";
import { ToggleRow } from "./ToggleRow";

/** The list plus the saved value when it is not in it (a size typed into the saved settings), so the Select can show it. */
const withCurrent = (options: Option[], value: string): Option[] =>
  options.some((o) => o.value === value) ? options : [...options, { value, label: value }];

export function OutputPanel() {
  const output = useStore((s) => s.output);
  const { patchOutput, setBackground } = useStore.getState();
  const hex = abgrToHex(output.background_color);

  return (
    <section className="panel output" aria-label="Output">
      <span className="label panel-title">Output</span>
      <div className="fields">
        <Field label="Resolution">
          <Select
            value={output.resolution}
            options={withCurrent(OUTPUT_RESOLUTION_OPTIONS, output.resolution)}
            onChange={(v) => patchOutput({ resolution: v })}
          />
        </Field>
        <Field label="FPS">
          <Select
            value={String(output.fps)}
            options={withCurrent(OUTPUT_FPS_OPTIONS, String(output.fps))}
            onChange={(v) => patchOutput({ fps: v === "source" ? "source" : Number(v) })}
          />
        </Field>
        <Field label="Aspect">
          <Select value={output.fit} options={FIT_OPTIONS} onChange={(v) => patchOutput({ fit: v as FitMode })} />
        </Field>
        <Field label="Background">
          {/* A later phase swaps this for the full ColorField popover. */}
          <span className="swatch">
            <input type="color" value={hex} aria-label="Background color" onChange={(e) => setBackground(hexToAbgr(e.target.value))} />
            <span className="chip" style={{ background: hex }} />
            <span className="hex">{hex}</span>
          </span>
        </Field>
      </div>
      <div className="toggles">
        <ToggleRow label="Also offer 720p and 360p to apps" on={output.extra_sizes} onChange={(on) => patchOutput({ extra_sizes: on })} />
      </div>
    </section>
  );
}
