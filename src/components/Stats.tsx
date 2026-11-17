import { useStats, useStore } from "../store";
import { formatLabel, measuredFps } from "../camera/schema";

/** One line under the columns: what comes in, what goes out, how long it takes, who is watching. */
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
    text = "Camera off: no app is using Better Cam";
  } else if (!stats || stats.capture.width === 0 || stats.output.width === 0) {
    text = state === "live" ? "Starting…" : "Opening the camera…";
  } else {
    const { capture: c, output: o, consumers } = stats;
    const decode = c.decode === "hw" ? " (HW)" : c.decode === "sw" ? " (SW)" : "";
    const used = consumers === 0 ? "No app is using Better Cam" : `In use by ${consumers} ${consumers === 1 ? "app" : "apps"}`;
    text = [
      `${c.width}×${c.height} ${formatLabel(c.format)} ${measuredFps(c.fps)} fps${decode} → ${o.width}×${o.height} ${measuredFps(o.fps)} fps`,
      `GPU ${stats.gpu_ms.toFixed(1)} ms`,
      `${Math.round(stats.latency_p95_ms)} ms latency`,
      used,
    ].join(" · ");
    if (paused) text = `Paused · ${text}`;
    title = `Dropped ${stats.dropped} · repeated ${stats.repeated} · latency is the 95th percentile`;
  }

  return (
    <div className="stats" title={title} aria-live="off">
      {text}
    </div>
  );
}
