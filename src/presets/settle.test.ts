/**
 * Checks of the decision to write a preset's controls now or leave them for the camera's next `live`: an idle or lost camera
 * returns at once, a camera that is opening is waited for, a timeout gives up. `npm run test:preset` bundles and runs it.
 * It throws at the first failure and prints `ok` otherwise.
 */
import { REOPEN_GRACE_MS, SETTLE_MS, settleStep, settled, type EngineView } from "./settle";

const same = (got: unknown, want: unknown, what: string) => {
  const a = JSON.stringify(got);
  const b = JSON.stringify(want);
  if (a !== b) throw new Error(`${what}: got ${a}, wanted ${b}`);
};

const view = (state: EngineView["state"], reason: string | null = null): EngineView => ({ state, reason });

// --- one look --------------------------------------------------------------------------------------------------------
same(settleStep(view("live"), false, false, 0).result, true, "live is ready");
same(settleStep(view("stopped"), false, false, 0).result, true, "stopped: the camera is read by opening it");
same(settleStep(view("standby", "idle"), false, false, 0).result, false, "standby for idle (nobody uses the camera): not at all");
same(settleStep(view("standby", "idle"), true, false, 0).result, false, "idle after a capture change too");
same(settleStep(view("reconnecting", "camera is in use by another app"), false, false, 0).result, false, "reconnecting: the controls wait for live");
same(settleStep(view("standby", "opening"), false, false, 0).result, null, "opening: wait");
same(settleStep(view("standby", "reopening"), true, false, 0).result, null, "reopening: wait");
same(settleStep(view("standby", "opening"), false, false, 0).left, true, "an engine that is not live has left it");
same(settleStep(view("live"), true, false, 100).result, null, "after a capture change the old live does not count yet");
same(settleStep(view("live"), true, true, 100).result, true, "live again after it left");
same(settleStep(view("live"), true, false, REOPEN_GRACE_MS + 1).result, true, "or after the grace time");
same(settleStep(view("standby", "opening"), false, true, SETTLE_MS).result, false, "a camera that does not open in SETTLE_MS: give up");

// --- the loop, on a fake clock --------------------------------------------------------------------------------------
const run = async (engineAt: (ms: number) => EngineView, reopens: boolean) => {
  let t = 0;
  const calls: number[] = [];
  const result = await settled(
    {
      engine: () => engineAt(t),
      now: () => t,
      sleep: async (ms) => {
        calls.push(ms);
        t += ms;
      },
    },
    reopens,
  );
  return { result, elapsed: t, sleeps: calls.length };
};

same(await run(() => view("standby", "idle"), false), { result: false, elapsed: 0, sleeps: 0 }, "an idle camera: false at once, no wait");
same(await run(() => view("live"), false), { result: true, elapsed: 0, sleeps: 0 }, "a live camera: true at once");
same(await run((t) => (t < 500 ? view("standby", "opening") : view("live")), false), { result: true, elapsed: 500, sleeps: 5 }, "a camera that is opening: waits for live");
same(await run(() => view("standby", "opening"), false), { result: false, elapsed: SETTLE_MS, sleeps: SETTLE_MS / 100 }, "a camera that never opens: gives up after SETTLE_MS");
same(await run((t) => (t < 300 ? view("live") : t < 600 ? view("standby", "reopening") : view("live")), true), { result: true, elapsed: 600, sleeps: 6 }, "a reopen: the old live is not counted, the new one is");
same(await run((t) => (t < 300 ? view("live") : view("standby", "idle")), true), { result: false, elapsed: 300, sleeps: 3 }, "a camera that went idle while it reopened: false at once");

console.log("ok");
