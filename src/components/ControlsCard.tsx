import { useEffect, type CSSProperties, type KeyboardEvent } from "react";
import { useStore } from "../store";
import { CONTROL_LABELS, controlKind, controlValueText, findCamera, powerlineOptions, type ControlInfo } from "../camera/schema";
import { Icon } from "./Icon";
import { Select } from "./Select";

/** The device controls of the selected camera (plan 6.2): the device's own ranges, Auto switches, double-click to reset. */
export function ControlsCard() {
  const cameras = useStore((s) => s.cameras);
  const deviceId = useStore((s) => s.source.video_device_id);
  const infos = useStore((s) => s.controlInfos);
  const controlsFor = useStore((s) => s.controlsFor);
  const error = useStore((s) => s.controlsError);
  const resetting = useStore((s) => s.controlsResetting);
  const state = useStore((s) => s.engine.state);
  // Remembered across launches (`Saved.ui`); a folded card does not read the camera.
  const open = useStore((s) => s.ui.controls);
  const { refreshControls, resetAllControls, setUi } = useStore.getState();

  const cam = cameras ? findCamera(cameras, deviceId) : null;
  const shown = cam && controlsFor === cam.id ? infos : null;
  // The camera is read when it is open for sure (live) or closed for good (stopped), and only while somebody looks: a
  // read without an open camera lights the camera's LED for a moment.
  const settled = state === "live" || state === "stopped" ? state : null;

  useEffect(() => {
    if (!open || !cam || !settled) return;
    const read = () => {
      if (!document.hidden) void refreshControls();
    };
    read();
    document.addEventListener("visibilitychange", read);
    return () => document.removeEventListener("visibilitychange", read);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, cam?.id, settled]);

  let body;
  if (!cameras) body = <p className="ctl-note">Looking for cameras…</p>;
  else if (!cam) body = <p className="ctl-note">Select a camera to adjust its controls.</p>;
  else if (error) body = <p className="ctl-note bad">{error}</p>;
  else if (!shown) body = <p className="ctl-note">Reading the camera’s controls…</p>;
  else if (shown.length === 0) body = <p className="ctl-note">This camera does not report any adjustable controls.</p>;
  else
    body = (
      <div className="ctl-rows">
        {shown.map((i) => (
          <ControlRow key={i.key} info={i} />
        ))}
      </div>
    );

  return (
    <section className="card controls" aria-label="Camera controls">
      <div className="card-head">
        <h3 className={`card-title${open ? "" : " folded"}`}>
          <button aria-expanded={open} title={open ? "Collapse" : "Expand"} onClick={() => setUi({ controls: !open })}>
            <Icon name="chevron" />
            <span className="label">Camera controls</span>
          </button>
        </h3>
        <span className="fill" />
        <button
          className="ghost"
          disabled={!shown || shown.length === 0 || resetting}
          title="Back to the camera’s own defaults, Auto on where the camera has it"
          onClick={() => void resetAllControls()}
        >
          <Icon name="reset" />
          Reset all
        </button>
      </div>
      {open && body}
    </section>
  );
}

function ControlRow({ info }: { info: ControlInfo }) {
  const { setControl, resetControl } = useStore.getState();
  const kind = controlKind(info.key);
  const label = CONTROL_LABELS[info.key];
  const reset = () => resetControl(info.key);
  // With Auto on the camera sets the value; the slider only shows it.
  const locked = info.auto || !info.can_manual;

  return (
    <div className={`ctl${locked ? " auto" : ""}`} data-key={info.key}>
      <div className="ctl-head">
        <span className="ctl-name" title={label}>
          {label}
        </span>
        {kind !== "select" && <b>{controlValueText(info)}</b>}
        {info.can_auto && (
          <label className="ctl-auto" title={info.auto ? "The camera sets this" : "Let the camera set this"}>
            <span>Auto</span>
            <button
              type="button"
              role="switch"
              aria-checked={info.auto}
              aria-label={`${label} auto`}
              className="switch"
              onClick={() => setControl(info.key, { auto: !info.auto })}
            />
          </label>
        )}
        {kind === "switch" && (
          <button
            type="button"
            role="switch"
            aria-checked={info.value !== 0}
            aria-label={label}
            className="switch"
            disabled={locked}
            onClick={() => setControl(info.key, { value: info.value !== 0 ? 0 : 1 })}
          />
        )}
      </div>
      {kind === "slider" && <Slider info={info} label={label} locked={locked} onReset={reset} />}
      {kind === "select" && (
        <Select
          value={String(info.value)}
          options={powerlineOptions(info)}
          disabled={locked}
          onChange={(v) => setControl(info.key, { value: Number(v) })}
        />
      )}
    </div>
  );
}

function Slider({ info, label, locked, onReset }: { info: ControlInfo; label: string; locked: boolean; onReset: () => void }) {
  const { setControl } = useStore.getState();
  const span = info.max - info.min;
  const fill = span > 0 ? ((info.value - info.min) / span) * 100 : 0;
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === "Delete" && !locked) {
      e.preventDefault();
      onReset();
    }
  };
  return (
    <input
      type="range"
      aria-label={label}
      title="Double-click or press Delete to reset"
      style={{ "--p": `${fill}%` } as CSSProperties}
      min={info.min}
      max={info.max}
      step={info.step}
      value={info.value}
      disabled={locked}
      onChange={(e) => setControl(info.key, { value: Number(e.target.value) })}
      onDoubleClick={onReset}
      onKeyDown={onKeyDown}
    />
  );
}
