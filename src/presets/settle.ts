/**
 * When a preset load may write the camera's controls (docs/LAYOUT-INSPECTOR.md 3.3, step 6). Pure: the engine's state comes in,
 * a verdict goes out, so a script can run it with a fake clock.
 */

/** How long a load waits for the camera to be open before it leaves the controls for the next `live`. */
export const SETTLE_MS = 10_000;
/** After a capture change a `live` from before it counts once this long has passed, whether or not the engine was seen leaving it. */
export const REOPEN_GRACE_MS = 1_500;
const POLL_MS = 100;

export interface EngineView {
  state: "stopped" | "standby" | "live" | "reconnecting";
  reason: string | null;
}

/**
 * One look at the engine. `result`: `true` the camera is ready (open, or closed for good: the controls are read by opening it
 * for a moment), `false` it will not open soon (nobody uses it: `standby` for `idle`; it is lost or in use: `reconnecting`;
 * or SETTLE_MS passed), `null` look again. `left`: the engine was seen to leave `live` after the capture change (`reopens`).
 */
export function settleStep(engine: EngineView, reopens: boolean, left: boolean, elapsed: number): { result: boolean | null; left: boolean } {
  if (engine.state === "stopped") return { result: true, left };
  // A closed camera opens when an app asks for it: waiting here would only hold the load for SETTLE_MS. The controls wait for `live`.
  if (engine.state === "reconnecting" || (engine.state === "standby" && engine.reason === "idle")) return { result: false, left };
  if (engine.state === "live" && (!reopens || left || elapsed > REOPEN_GRACE_MS)) return { result: true, left };
  return { result: elapsed >= SETTLE_MS ? false : null, left: left || engine.state !== "live" };
}

export interface SettleEnv {
  engine: () => EngineView;
  now: () => number;
  sleep: (ms: number) => Promise<void>;
}

/** Waits for `settleStep` to give a verdict. */
export async function settled(env: SettleEnv, reopens: boolean): Promise<boolean> {
  const t0 = env.now();
  let left = false;
  for (;;) {
    const step = settleStep(env.engine(), reopens, left, env.now() - t0);
    left = step.left;
    if (step.result !== null) return step.result;
    await env.sleep(POLL_MS);
  }
}
