import { useEffect, useState, type CSSProperties, type ReactNode } from "react";
import { useStats, useStore } from "../store";
import {
  DEFAULT_AUTO,
  SLOW_CAMERA_AFTER,
  SPEED_OPTIONS,
  TARGET_MAX,
  TARGET_MIN,
  autoAction,
  autoAdjustWhiteBalance,
  autoText,
  autoTone,
  cameraIsSlow,
  exposureText,
  findCamera,
  longestKey,
  longestOptions,
  maxGainValue,
  parseLongest,
  requestedFps,
  slowCameraText,
  type AutoCfg,
  type AutoLoop,
  type AutoState,
  type ControlInfo,
  type ExposureStatus,
  type Speed,
  type WhiteBalanceStatus,
} from "../camera/schema";
import { Select } from "./Select";

/** What the Better Cam auto blocks of the card read besides the row's own control. */
export interface AutoCtx {
  /** Every control the camera reported (the gain's range, the low light switch). */
  infos: ControlInfo[];
  cfg: AutoCfg;
  exposure: ExposureStatus | null;
  whiteBalance: WhiteBalanceStatus | null;
  /** The camera is open: without it the loops have nothing to say yet. */
  live: boolean;
}

const WHOLE_STOPS = "No gain control: Better Cam auto works in whole stops.";
const LOW_LIGHT = "This camera can also hold the frame rate itself: Camera auto with Low light compensation off.";
const AFTER_SENSOR = "Brightness, contrast and gamma change the picture after the sensor. Set them once and leave them.";

const IDLE: readonly AutoState[] = ["paused", "closed", "no_picture"];

/**
 * One line under a row: a dot coloured by the state (neutral while it works, warning at a limit, error when it
 * stopped), the text, and the button the state needs. A polite live region, so a screen reader hears a change.
 */
export function StatusLine({ state, text, loop, label }: { state: AutoState; text: string | null; loop: AutoLoop; label: string }) {
  const resumeAuto = useStore((s) => s.resumeAuto);
  const action = autoAction(state);
  return (
    <div className="ctl-status" role="status" aria-live="polite" data-tone={autoTone(state)} data-idle={IDLE.includes(state)}>
      {text && (
        <>
          <span className="ctl-dot" aria-hidden="true" />
          <span className="ctl-status-text">{text}</span>
          {action && (
            <button type="button" className="link" aria-label={`${action === "resume" ? "Resume" : "Try again"}: ${label}`} onClick={() => void resumeAuto(loop)}>
              {action === "resume" ? "Resume" : "Try again"}
            </button>
          )}
        </>
      )}
    </div>
  );
}

/** What a loop says, as the line under its row: the backend's text; before there is any, what the camera's state implies. */
export function LoopStatus({ loop, label, status, live }: { loop: AutoLoop; label: string; status: ExposureStatus | WhiteBalanceStatus | null; live: boolean }) {
  if (status) return <StatusLine state={status.state} text={autoText(status)} loop={loop} label={label} />;
  return <StatusLine state={live ? "adjusting" : "closed"} text={live ? "Adjusting…" : "Starts when the camera opens."} loop={loop} label={label} />;
}

const pct = (v: number, min: number, max: number) => (max > min ? ((v - min) / (max - min)) * 100 : 0);

function Field({ label, value, children }: { label: string; value?: string; children: ReactNode }) {
  return (
    <label className="auto-field">
      <span className="auto-label">{label}</span>
      <span className="auto-input">{children}</span>
      {value !== undefined && <b>{value}</b>}
    </label>
  );
}

/** A slider of a sub-block: the app's own range input, reset by a double click. */
function Range({
  label, min, max, step, value, def, onChange, ends,
}: {
  label: string;
  min: number;
  max: number;
  step: number;
  value: number;
  def: number;
  onChange: (v: number) => void;
  ends?: [string, string];
}) {
  return (
    <>
      {ends && <span className="auto-end">{ends[0]}</span>}
      <input
        type="range"
        aria-label={label}
        title="Double-click to reset"
        style={{ "--p": `${pct(value, min, max)}%` } as CSSProperties}
        min={min}
        max={max}
        step={step}
        value={value}
        onChange={(e) => onChange(Number(e.target.value))}
        onDoubleClick={() => onChange(def)}
      />
      {ends && <span className="auto-end">{ends[1]}</span>}
    </>
  );
}

const signed = (v: number) => (v === 0 ? "0" : `${v > 0 ? "+" : ""}${v.toFixed(2)}`);

/** "Response": how fast the loop moves. */
function ResponseField({ value, onChange }: { value: Speed; onChange: (v: Speed) => void }) {
  return (
    <Field label="Response">
      <Select value={value} options={SPEED_OPTIONS} onChange={(v) => onChange(v as Speed)} />
    </Field>
  );
}

/** The settings, the hints and the status of Better Cam auto exposure, under the exposure row (docs/AUTO-CONTROLS.md 4.1, 4.3). */
export function ExposureAuto({ info, ctx }: { info: ControlInfo; ctx: AutoCtx }) {
  const setAutoConfig = useStore((s) => s.setAutoConfig);
  const addFilter = useStore((s) => s.addFilter);
  // The filter's own target is the final say on brightness (3 of the document).
  const adjust = useStore((s) => s.chain.find((f) => f.kind === "auto_adjust" && f.enabled && f.params.exposure === true));
  const cfg = ctx.cfg.exposure;
  const gain = ctx.infos.find((i) => i.key === "gain");
  const stops = info.app === "stops";
  const lowLight = ctx.infos.some((i) => i.key === "low_light_compensation");
  const adjustTarget = adjust && typeof adjust.params.target === "number" ? adjust.params.target : DEFAULT_AUTO.exposure.target;
  const fps = ctx.exposure && ctx.exposure.target_fps > 0 ? ctx.exposure.target_fps : null;

  const options = longestOptions(info, fps);
  const longest = longestKey(cfg.longest);
  if (!options.some((o) => o.value === longest)) options.push({ value: longest, label: exposureText(Number(longest)) });

  const mostGain = gain && gain.can_manual && !stops ? maxGainValue(cfg.max_gain, gain) : null;

  return (
    <div className="ctl-sub" data-for="exposure">
      <Field label="Target brightness" value={cfg.target.toFixed(2)}>
        <Range
          label="Target brightness"
          min={TARGET_MIN}
          max={TARGET_MAX}
          step={0.01}
          value={cfg.target}
          def={DEFAULT_AUTO.exposure.target}
          onChange={(target) => setAutoConfig({ exposure: { target } })}
        />
      </Field>
      <Field label="Longest exposure">
        <Select value={longest} options={options} onChange={(v) => setAutoConfig({ exposure: { longest: parseLongest(v) } })} />
      </Field>
      {gain && mostGain !== null && (
        <Field label="Most gain" value={mostGain <= gain.min ? "Off" : String(mostGain)}>
          <Range
            label="Most gain"
            min={gain.min}
            max={gain.max}
            step={Math.max(1, gain.step)}
            value={mostGain}
            def={maxGainValue(null, gain)}
            onChange={(max_gain) => setAutoConfig({ exposure: { max_gain } })}
          />
        </Field>
      )}
      <ResponseField value={cfg.speed} onChange={(speed) => setAutoConfig({ exposure: { speed } })} />
      <LoopStatus loop="exposure" label="Exposure" status={ctx.exposure} live={ctx.live} />
      {ctx.exposure?.note && <p className="ctl-hint">{ctx.exposure.note}</p>}
      {stops && <p className="ctl-hint">{WHOLE_STOPS}</p>}
      {adjust ? (
        <p className="ctl-hint">Auto Adjust finishes the brightness at its own target ({adjustTarget.toFixed(2)}).</p>
      ) : (
        stops && (
          <p className="ctl-hint">
            Add Auto Adjust for the fine trim.
            <button type="button" className="link" onClick={() => addFilter("auto_adjust")}>
              Add Auto Adjust
            </button>
          </p>
        )
      )}
      {lowLight && <p className="ctl-hint">{LOW_LIGHT}</p>}
      <p className="ctl-hint">{AFTER_SENSOR}</p>
    </div>
  );
}

/**
 * The camera delivers far less than the rate it is asked for (the open media type's, so also with Device default), for
 * at least `SLOW_CAMERA_AFTER` seconds: the two rates, else `null`. The card's hint and the footer read it.
 */
export function useSlowCamera(): { measured: number; requested: number } | null {
  const stats = useStats((s) => s.stats);
  const source = useStore((s) => s.source);
  const output = useStore((s) => s.output);
  const cameras = useStore((s) => s.cameras);
  const cam = cameras ? findCamera(cameras, source.video_device_id) : null;
  const requested = stats ? requestedFps(source, output, cam, stats.capture) : null;
  const measured = stats?.capture.fps ?? 0;
  const slow = cameraIsSlow(measured, requested);
  const [shown, setShown] = useState(false);
  useEffect(() => {
    if (!slow) {
      setShown(false);
      return;
    }
    const t = setTimeout(() => setShown(true), SLOW_CAMERA_AFTER * 1000);
    return () => clearTimeout(t);
  }, [slow]);
  return shown && slow && requested !== null ? { measured, requested } : null;
}

/**
 * Under the exposure row while it is in Camera auto: the camera's own auto lengthens the exposure in dim light and the
 * frame rate falls with it, and Better Cam auto cannot change that from outside. After 3 s with the measured capture
 * rate under 0.9 × the rate asked for, say so and offer Better Cam auto (only where it is offered); it goes away when
 * the rate recovers.
 */
export function CameraAutoHint({ info }: { info: ControlInfo }) {
  const setControlMode = useStore((s) => s.setControlMode);
  const slow = useSlowCamera();
  if (!slow || info.app == null) return null;
  return (
    <p className="ctl-hint" role="status" aria-live="polite" data-hint="slow-camera">
      {slowCameraText(slow.measured, slow.requested)}
      <button type="button" className="link" onClick={() => void setControlMode("exposure", "app")}>
        Use Better Cam auto
      </button>
    </p>
  );
}

/** The settings, the hint and the status of Better Cam auto white balance, under the white balance row. */
export function WhiteBalanceAuto({ ctx }: { ctx: AutoCtx }) {
  const setAutoConfig = useStore((s) => s.setAutoConfig);
  const addFilter = useStore((s) => s.addFilter);
  const cfg = ctx.cfg.white_balance;
  // Auto Adjust finishes what is left past the end of the camera's range, and its warmth decides the final colour.
  const adjust = useStore((s) => autoAdjustWhiteBalance(s.chain));
  const adjustWarmth = adjust ? Number(adjust.params.temperature) || 0 : 0;

  return (
    <div className="ctl-sub" data-for="white_balance">
      <Field label="Warmth" value={signed(cfg.warmth)}>
        <Range
          label="Warmth"
          min={-1}
          max={1}
          step={0.01}
          value={cfg.warmth}
          def={0}
          ends={["cooler", "warmer"]}
          onChange={(warmth) => setAutoConfig({ white_balance: { warmth } })}
        />
      </Field>
      <ResponseField value={cfg.speed} onChange={(speed) => setAutoConfig({ white_balance: { speed } })} />
      <LoopStatus loop="white_balance" label="White balance" status={ctx.whiteBalance} live={ctx.live} />
      {ctx.whiteBalance?.state === "at_limit" &&
        (adjust ? (
          <p className="ctl-hint" data-hint="finish-colour">Auto Adjust finishes the colour.</p>
        ) : (
          <p className="ctl-hint" data-hint="finish-colour">
            Add Auto Adjust to finish the colour.
            <button type="button" className="link" onClick={() => addFilter("auto_adjust")}>
              Add Auto Adjust
            </button>
          </p>
        ))}
      {adjust && <p className="ctl-hint">Auto Adjust's warmth decides the final colour ({signed(adjustWarmth)}).</p>}
    </div>
  );
}

/** A limit on a slider: a tick with a tooltip and the range beyond it shaded (`fraction` 0..1 of the range). */
export function LimitMark({ fraction, title }: { fraction: number; title: string }) {
  return (
    <>
      <span className="ctl-shade" style={{ "--m": fraction } as CSSProperties} aria-hidden="true" />
      <span className="ctl-limit" style={{ "--m": fraction } as CSSProperties} title={title} />
    </>
  );
}
