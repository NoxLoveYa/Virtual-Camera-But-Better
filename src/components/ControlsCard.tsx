import { Fragment, useEffect, type CSSProperties, type KeyboardEvent } from "react";
import { useStore } from "../store";
import {
  CONTROL_LABELS,
  MODE_LABELS,
  autoCfgOf,
  capText,
  controlKind,
  controlValueText,
  findCamera,
  hasModeList,
  limitFraction,
  maxGainValue,
  modeOf,
  powerlineOptions,
  showsStatus,
  statusOf,
  type ControlInfo,
  type ControlMode,
} from "../camera/schema";
import { groupControls } from "../ui/controlGroups";
import { useSummary } from "../ui/useSummary";
import { CameraAutoHint, ExposureAuto, LimitMark, LoopStatus, WhiteBalanceAuto, type AutoCtx } from "./AutoControls";
import { Icon } from "./Icon";
import { InspectorHeader } from "./InspectorHeader";
import { Select, type SelectOption } from "./Select";

/** The device controls of the selected camera (plan 6.2): the device's own ranges, Auto toggles, double-click to reset. */
export function ControlsCard() {
  const cameras = useStore((s) => s.cameras);
  const deviceId = useStore((s) => s.source.video_device_id);
  const infos = useStore((s) => s.controlInfos);
  const controlsFor = useStore((s) => s.controlsFor);
  const error = useStore((s) => s.controlsError);
  const resetting = useStore((s) => s.controlsResetting);
  const state = useStore((s) => s.engine.state);
  const stored = useStore((s) => s.controls);
  const autoStatus = useStore((s) => s.autoStatus);
  const summary = useSummary("controls");
  const { refreshControls, resetAllControls } = useStore.getState();

  const cam = cameras ? findCamera(cameras, deviceId) : null;
  const shown = cam && controlsFor === cam.id ? infos : null;
  // The camera is read when it is open for sure (live) or closed for good (stopped), and only while somebody looks (this is
  // mounted only while Camera controls is selected): a read without an open camera lights the camera's LED for a moment.
  const settled = state === "live" || state === "stopped" ? state : null;

  useEffect(() => {
    if (!cam || !settled) return;
    const read = () => {
      if (!document.hidden) void refreshControls();
    };
    read();
    document.addEventListener("visibilitychange", read);
    return () => document.removeEventListener("visibilitychange", read);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cam?.id, settled]);

  let body;
  if (!cameras) body = <p className="ctl-note">Looking for cameras…</p>;
  else if (!cam) body = <p className="ctl-note">Select a camera to adjust its controls.</p>;
  else if (error) body = <p className="ctl-note bad">{error}</p>;
  else if (!shown) body = <p className="ctl-note">Reading the camera’s controls…</p>;
  else if (shown.length === 0) body = <p className="ctl-note">This camera does not report any adjustable controls.</p>;
  else {
    const ctx: AutoCtx = {
      infos: shown,
      cfg: autoCfgOf(stored, cam),
      exposure: statusOf(autoStatus, cam, "exposure"),
      whiteBalance: statusOf(autoStatus, cam, "white_balance"),
      live: state === "live",
    };
    body = (
      <div className="props ctl-props">
        {groupControls(shown).map((g) => (
          <Fragment key={g.title}>
            <h3 className="prop-group">{g.title}</h3>
            {g.items.map((i) => (
              <ControlRow key={i.key} info={i} ctx={ctx} />
            ))}
          </Fragment>
        ))}
      </div>
    );
  }

  return (
    <section className="insp-panel" aria-label="Camera controls">
      <InspectorHeader
        title="Camera controls"
        sub={summary}
        end={
          <button
            type="button"
            className="ghost insp-refresh"
            disabled={!shown || shown.length === 0 || resetting}
            title="Back to the camera’s own defaults, Auto on where the camera has it"
            onClick={() => void resetAllControls()}
          >
            <Icon name="reset" />
            Reset all
          </button>
        }
      />
      <div className="insp-scroll">{body}</div>
    </section>
  );
}

/** Manual, Camera auto, Better Cam auto: what the camera has; a Better Cam auto it cannot offer is listed, disabled, with the reason. */
const modeOptions = (info: ControlInfo): SelectOption[] => [
  ...(info.can_manual ? [{ value: "manual", label: MODE_LABELS.manual }] : []),
  ...(info.can_auto ? [{ value: "camera", label: MODE_LABELS.camera }] : []),
  { value: "app", label: MODE_LABELS.app, disabled: info.app == null, title: info.app == null ? (info.app_why ?? "Better Cam auto is not available for this control.") : undefined },
];

function ControlRow({ info, ctx }: { info: ControlInfo; ctx: AutoCtx }) {
  const { setControl, resetControl, setControlMode } = useStore.getState();
  const kind = controlKind(info.key);
  const label = CONTROL_LABELS[info.key];
  const reset = () => resetControl(info.key);
  const mode = modeOf(info);
  const app = mode === "app";
  const modal = hasModeList(info);
  // Better Cam auto sets the value and the control only shows it; a control without a manual mode cannot be set.
  // With the camera's Auto on it stays usable: moving it turns Auto off (the store does that).
  const locked = app || !info.can_manual;
  // The default state is Auto where the camera has it, else the default value; with Auto on the value is the camera's own.
  const changed = info.auto !== info.can_auto || (!info.auto && info.value !== info.def);
  const loop = info.key === "exposure" ? "exposure" : info.key === "white_balance" ? "white_balance" : null;
  const status = loop === "exposure" ? ctx.exposure : loop === "white_balance" ? ctx.whiteBalance : null;
  const gain = ctx.infos.find((i) => i.key === "gain");

  // The limits Better Cam auto keeps, as ticks on the sliders it moves (the range beyond is shaded).
  let limit: { fraction: number; title: string } | null = null;
  if (info.key === "exposure" && app) {
    const user = ctx.cfg.exposure.longest;
    const cap = status ? { stop: ctx.exposure!.cap, title: capText(ctx.exposure!.cap_reason, ctx.exposure!.target_fps) } : user !== "fit" ? { stop: user, title: "Your limit" } : null;
    const f = cap && limitFraction(cap.stop, info);
    if (cap && f != null) limit = { fraction: f, title: cap.title };
  } else if (info.key === "gain" && info.driven_by === "exposure" && gain && gain.can_manual) {
    const most = maxGainValue(ctx.cfg.exposure.max_gain, gain);
    const f = limitFraction(most, gain);
    if (f != null) limit = { fraction: f, title: `Most gain: ${most}` };
  }

  // "A": the camera sets this control itself (a control with a mode list has its own list instead).
  const autoToggle =
    !modal && info.can_auto ? (
      <button
        type="button"
        className="ctl-a"
        aria-pressed={info.auto}
        aria-label={`${label} auto`}
        title={info.auto ? "The camera sets this" : "Let the camera set this"}
        onClick={() => setControl(info.key, { auto: !info.auto })}
      >
        A
      </button>
    ) : null;
  const resetButton =
    kind === "slider" && !app && changed ? (
      <button
        type="button"
        className="reset-btn"
        aria-label={`Reset ${label}`}
        title={`Reset to ${info.can_auto ? "Auto" : controlValueText({ ...info, value: info.def })}`}
        onClick={(e) => {
          reset();
          // The button leaves with the change: keep the keyboard on the slider.
          (e.currentTarget.closest(".ctl")?.querySelector("input[type=range]") as HTMLElement | null)?.focus();
        }}
      >
        <Icon name="reset" size={16} />
      </button>
    ) : null;
  // The slots keep their width whether or not a button is in them, so the sliders do not resize when a value changes.
  const actions = (
    <span className="pf-act">
      <span className="pf-slot">{autoToggle}</span>
      <span className="pf-slot">{resetButton}</span>
    </span>
  );

  let row;
  if (kind === "slider") {
    row = (
      <div className={`prop slider${modal ? " has-mode" : ""}`}>
        <span className="pf-label" title={label}>
          {label}
        </span>
        {modal && (
          <label className="ctl-mode">
            <span className="sr-only">{label} mode</span>
            <Select value={mode} options={modeOptions(info)} onChange={(m) => void setControlMode(info.key, m as ControlMode)} />
          </label>
        )}
        <Slider info={info} label={label} locked={locked} onReset={reset} limit={limit} />
        <b className="pf-val">{controlValueText(info)}</b>
        {actions}
        {info.driven_by && <small className="pf-hint">Set by Better Cam auto exposure</small>}
      </div>
    );
  } else if (kind === "switch") {
    row = (
      <div className="prop bool">
        <span className="pf-label" title={label}>
          {label}
        </span>
        <button
          type="button"
          role="switch"
          aria-checked={info.value !== 0}
          aria-label={label}
          className="switch"
          disabled={locked}
          onClick={() => setControl(info.key, { value: info.value !== 0 ? 0 : 1 })}
        />
        {autoToggle && <span className="pf-act pf-act-only">{autoToggle}</span>}
      </div>
    );
  } else {
    row = (
      <div className={`prop${autoToggle ? " has-a" : ""}`}>
        <span className="pf-label" title={label}>
          {label}
        </span>
        <Select value={String(info.value)} options={powerlineOptions(info)} disabled={locked} onChange={(v) => setControl(info.key, { value: Number(v) })} />
        {autoToggle && <span className="pf-act pf-act-only">{autoToggle}</span>}
      </div>
    );
  }

  return (
    <div className={`ctl${locked || info.auto ? " auto" : ""}${modal ? " multi" : ""}`} data-key={info.key} data-mode={mode}>
      {row}
      {loop === "exposure" && mode === "camera" && ctx.live && <CameraAutoHint info={info} />}
      {loop === "exposure" && app && <ExposureAuto info={info} ctx={ctx} />}
      {loop === "white_balance" && app && <WhiteBalanceAuto ctx={ctx} />}
      {/* After a hand-back the line stays (with Try again) while the control is where it was handed to. */}
      {loop && !app && showsStatus(mode, status) && <LoopStatus loop={loop} label={label} status={status} live={ctx.live} />}
    </div>
  );
}

function Slider({
  info, label, locked, onReset, limit,
}: {
  info: ControlInfo;
  label: string;
  locked: boolean;
  onReset: () => void;
  /** A limit Better Cam auto keeps: a tick, and the range beyond it shaded. */
  limit: { fraction: number; title: string } | null;
}) {
  const { setControl } = useStore.getState();
  const span = info.max - info.min;
  const fill = span > 0 ? ((info.value - info.min) / span) * 100 : 0;
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === "Delete" && !locked) {
      e.preventDefault();
      onReset();
    }
  };
  const range = (
    <input
      type="range"
      className="pf-range"
      aria-label={label}
      title={locked ? undefined : info.auto ? "Moving this turns Auto off" : "Double-click or press Delete to reset"}
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
  return limit ? (
    <div className="ctl-range pf-range">
      {range}
      <LimitMark fraction={limit.fraction} title={limit.title} />
    </div>
  ) : (
    range
  );
}
