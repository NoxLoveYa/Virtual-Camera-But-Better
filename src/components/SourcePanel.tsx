import { useState } from "react";
import { useStore } from "../store";
import {
  BUFFERING_OPTIONS,
  COLOR_RANGE_OPTIONS,
  COLOR_SPACE_OPTIONS,
  HIGHEST_FPS,
  MATCH_OUTPUT_FPS,
  RES_TYPE_OPTIONS,
  cameraFormats,
  cameraIntervals,
  cameraSizes,
  effectiveSource,
  findCamera,
  formatLabel,
  fpsLabel,
  type Option,
  type SourceCfg,
} from "../camera/schema";
import { Field } from "./Field";
import { Icon } from "./Icon";
import { InspectorHeader } from "./InspectorHeader";
import { Select } from "./Select";
import { ToggleRow } from "./ToggleRow";
import { useSummary } from "../ui/useSummary";

const intervalLabel = (i: number) => (i === MATCH_OUTPUT_FPS ? "Match output FPS" : i === HIGHEST_FPS ? "Highest FPS" : fpsLabel(i));

export function SourcePanel() {
  const saved = useStore((s) => s.source);
  const inUse = useStore((s) => s.inUse);
  const cameras = useStore((s) => s.cameras);
  const { patchSource, refreshCameras, trySavedFormat } = useStore.getState();
  // The fields show what runs: while the camera cannot deliver the saved format that is the device default, and the
  // saved format is named under them (it stays saved; the next Start, or the button, tries it again).
  const source = effectiveSource(saved, inUse);
  const advanced = useStore((s) => s.ui.advanced);
  const { setUi } = useStore.getState();
  const [scanning, setScanning] = useState(false);
  const summary = useSummary("camera");

  const cam = cameras ? findCamera(cameras, source.video_device_id) : null;
  const missing = !!cameras && !!source.video_device_id && !cam;
  // A camera whose formats could not be read offers the device default only.
  const known = !!cam && cam.formats.length > 0;
  const custom = source.res_type === "custom";
  const editable = known && custom;

  const deviceOptions: Option[] = (cameras ?? []).map((c) => ({ value: c.id, label: c.name }));
  if (missing) deviceOptions.push({ value: source.video_device_id!, label: "Saved camera (disconnected)" });

  const sizes = editable ? cameraSizes(cam, source.video_format) : [source.resolution];
  const rates = editable ? cameraIntervals(cam, source.resolution, source.video_format) : [];
  const rateOptions: Option[] = editable
    ? [MATCH_OUTPUT_FPS, HIGHEST_FPS, ...rates].map((i) => ({ value: String(i), label: intervalLabel(i) }))
    : [{ value: String(source.frame_interval), label: intervalLabel(source.frame_interval) }];
  const formats = known ? (custom ? cameraFormats(cam, source.resolution) : cameraFormats(cam)) : [];
  const formatOptions: Option[] = [
    { value: "any", label: "Any" },
    ...formats.map((f) => ({ value: f, label: formatLabel(f) })),
  ];
  if (!formatOptions.some((o) => o.value === source.video_format)) {
    formatOptions.push({ value: source.video_format, label: formatLabel(source.video_format) });
  }

  const flag = (key: "flip_vertically" | "autorotation" | "hw_decode" | "deactivate_when_not_showing") => (on: boolean) =>
    patchSource({ [key]: on });

  const rescan = async () => {
    setScanning(true);
    try {
      await refreshCameras();
    } finally {
      setScanning(false);
    }
  };

  return (
    <section className="insp-panel" aria-label="Camera">
      <InspectorHeader
        title="Camera"
        sub={summary}
        end={
          <button type="button" className="ghost insp-refresh" onClick={rescan} disabled={scanning} title="Rescan cameras" aria-label="Refresh cameras">
            <Icon name="reset" />
            Refresh
          </button>
        }
      />
      <div className="insp-scroll">
        <div className="props">
          <Field label="Device">
            <Select
              value={cam?.id ?? source.video_device_id ?? ""}
              options={deviceOptions}
              onChange={(id) => patchSource({ video_device_id: id })}
              placeholder={cameras ? "No camera found" : "Loading…"}
            />
          </Field>
          <Field label="Resolution/FPS type">
            <Select
              value={source.res_type}
              options={RES_TYPE_OPTIONS}
              disabled={!known}
              onChange={(v) => patchSource({ res_type: v as SourceCfg["res_type"] })}
            />
          </Field>
          <Field label="Resolution">
            <Select
              value={source.resolution}
              options={sizes.map((v) => ({ value: v, label: v }))}
              disabled={!editable}
              onChange={(v) => patchSource({ resolution: v })}
            />
          </Field>
          <Field label="FPS">
            <Select
              value={String(source.frame_interval)}
              options={rateOptions}
              disabled={!editable}
              onChange={(v) => patchSource({ frame_interval: Number(v) })}
            />
          </Field>
          <Field label="Video format">
            <Select
              value={source.video_format}
              options={formatOptions}
              disabled={!known}
              onChange={(v) => patchSource({ video_format: v as SourceCfg["video_format"] })}
            />
          </Field>
          {inUse && (
            <div className="format-in-use" role="status">
              <span className="format-in-use-text" title={`The camera does not deliver ${inUse.asked}. It stays your saved choice.`}>
                <b>Using the device default</b>
                <small>Saved: {inUse.asked}</small>
              </span>
              <button onClick={trySavedFormat} title="Open the camera with the saved format again">
                Try saved format
              </button>
            </div>
          )}

          <h3 className={`prop-group fold${advanced ? "" : " folded"}`}>
            <button type="button" aria-expanded={advanced} onClick={() => setUi({ advanced: !advanced })}>
              <Icon name="chevron" />
              Advanced
            </button>
          </h3>
          {advanced && (
            <>
              <Field label="Color space">
                <Select
                  value={source.color_space}
                  options={COLOR_SPACE_OPTIONS}
                  onChange={(v) => patchSource({ color_space: v as SourceCfg["color_space"] })}
                />
              </Field>
              <Field label="Color range">
                <Select
                  value={source.color_range}
                  options={COLOR_RANGE_OPTIONS}
                  onChange={(v) => patchSource({ color_range: v as SourceCfg["color_range"] })}
                />
              </Field>
              <Field label="Buffering">
                <Select
                  value={source.buffering}
                  options={BUFFERING_OPTIONS}
                  onChange={(v) => patchSource({ buffering: v as SourceCfg["buffering"] })}
                />
              </Field>
              <ToggleRow label="Flip vertically" on={source.flip_vertically} onChange={flag("flip_vertically")} />
              <ToggleRow label="Apply rotation data from camera" on={source.autorotation} onChange={flag("autorotation")} />
              <ToggleRow label="Use hardware decoding when available" on={source.hw_decode} onChange={flag("hw_decode")} />
              <ToggleRow
                label="Turn off camera when no app is using Better Cam"
                on={source.deactivate_when_not_showing}
                onChange={flag("deactivate_when_not_showing")}
              />
            </>
          )}
        </div>
      </div>
    </section>
  );
}
