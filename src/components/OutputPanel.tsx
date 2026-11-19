import { useStore } from "../store";
import {
  FIT_OPTIONS,
  OUTPUT_FPS_OPTIONS,
  OUTPUT_RESOLUTION_OPTIONS,
  type FitMode,
  type Option,
} from "../camera/schema";
import { ColorField } from "./ColorField";
import { Field } from "./Field";
import { InspectorHeader } from "./InspectorHeader";
import { Select } from "./Select";
import { ToggleRow } from "./ToggleRow";
import { useSummary } from "../ui/useSummary";

/** The list plus the saved value when it is not in it (a size typed into the saved settings), so the Select can show it. */
const withCurrent = (options: Option[], value: string): Option[] =>
  options.some((o) => o.value === value) ? options : [...options, { value, label: value }];

export function OutputPanel() {
  const output = useStore((s) => s.output);
  const { patchOutput, setBackground } = useStore.getState();
  const summary = useSummary("output");

  return (
    <section className="insp-panel" aria-label="Output">
      <InspectorHeader title="Output" sub={summary} />
      <div className="insp-scroll">
        <div className="props">
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
            {/* No alpha: the background stays opaque, ColorField keeps the alpha byte (0xFF) as it is. */}
            <ColorField value={output.background_color} onChange={setBackground} label="Background color" />
          </Field>
          <ToggleRow label="Also offer 720p and 360p to apps" on={output.extra_sizes} onChange={(on) => patchOutput({ extra_sizes: on })} />
        </div>
      </div>
    </section>
  );
}
