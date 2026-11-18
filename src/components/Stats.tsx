import { useStats, useStore } from "../store";
import { formatLabel, measuredFps } from "../camera/schema";

const CHOOSE = "choose “Better Cam” as the camera in your video app";

/** One line in the footer: who uses Better Cam first, then what comes in and goes out, how long it takes. */
export function Stats() {
  const state = useStore((s) => s.engine.state);
  const reason = useStore((s) => s.engine.reason);
  const paused = useStore((s) => s.paused);
  const stats = useStats((s) => s.stats);

  let text: string;
  let title: string | undefined;
  if (state === "stopped") {
    text = "Stopped";
  } else if (state === "reconnecting") {
    text = "Waiting for the camera";
  } else if (state === "standby" && reason === "idle") {
    text = `Not in use · the camera is off until an app uses Better Cam · ${CHOOSE}`;
  } else if (!stats || stats.capture.width === 0 || stats.output.width === 0) {
    text = state === "live" ? "Starting…" : "Opening the camera…";
  } else {
    const { capture: c, output: o, consumers } = stats;
    const decode = c.decode === "hw" ? " (HW)" : c.decode === "sw" ? " (software decode)" : "";
    const size = `${c.width}×${c.height}`;
    const out = `${o.width}×${o.height}`;
    const tail = [`${measuredFps(o.fps)} fps`, `GPU ${stats.gpu_ms.toFixed(1)} ms`, `${Math.round(stats.latency_p95_ms)} ms latency`];
    // With nobody using the camera the line says what to do about it, and leaves the format to the tooltip.
    text =
      consumers === 0
        ? [`Not in use · ${CHOOSE}`, `${size} → ${out}`, ...tail].join(" · ")
        : [`In use by ${consumers} ${consumers === 1 ? "app" : "apps"}`, `${size} ${formatLabel(c.format)}${decode} → ${out}`, ...tail].join(" · ");
    if (paused) text = `Paused · ${text}`;
    title = [
      `Camera ${size} ${formatLabel(c.format)} ${measuredFps(c.fps)} fps${decode}`,
      `Output ${out} ${measuredFps(o.fps)} fps`,
      `Dropped ${stats.dropped}`,
      `repeated ${stats.repeated}`,
      "latency is the 95th percentile",
    ].join(" · ");
    if (c.decode === "sw") title += " · software decoding (hardware decoding is off or failed for this format)";
  }

  return (
    <div className="stats" title={title} aria-live="off">
      {text}
    </div>
  );
}
