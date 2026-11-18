/**
 * Checks of the virtual camera status poll: when it runs, that a tick waits for a pending read, and that an older
 * answer never replaces a newer one. `npm run test:ui` bundles and runs it. It throws at the first failure.
 */
import { createReadGate, wantsVcamPoll } from "./vcamPoll";

function eq<T>(actual: T, expected: T, what: string) {
  if (actual !== expected) throw new Error(`${what}: expected ${String(expected)}, got ${String(actual)}`);
}

// the poll runs only for a problem that Repair cannot fix (its button is hidden until an administrator has acted)
eq(wantsVcamPoll(null), false, "no status yet");
eq(wantsVcamPoll(undefined), false, "no status yet (undefined)");
eq(wantsVcamPoll({ problem: null, repairable: true }), false, "no problem");
eq(wantsVcamPoll({ problem: null, repairable: false }), false, "no problem, not repairable");
eq(wantsVcamPoll({ problem: "encrypted", repairable: true }), false, "a problem Repair fixes");
eq(wantsVcamPoll({ problem: "encrypted", repairable: false }), true, "a problem Repair cannot fix");

// a tick waits while a read is pending, and goes on when it has ended (with or without an answer)
{
  const gate = createReadGate();
  eq(gate.busy(), false, "idle at first");
  const a = gate.begin();
  eq(gate.busy(), true, "busy while a read is pending");
  eq(gate.end(a, true), true, "the answer is shown");
  eq(gate.busy(), false, "idle after it");
  const b = gate.begin();
  eq(gate.end(b, false), false, "a failed read shows nothing");
  eq(gate.busy(), false, "idle after a failed read");
}

// a focus read and a tick that overlap: the older answer arriving last is dropped, the newer one stays
{
  const gate = createReadGate();
  const tick = gate.begin();
  const focus = gate.begin();
  eq(gate.busy(), true, "two pending");
  eq(gate.end(focus, true), true, "the newer answer first is shown");
  eq(gate.busy(), true, "one still pending");
  eq(gate.end(tick, true), false, "the older answer after it is dropped");
  eq(gate.busy(), false, "idle after both");
  // in order: both are shown
  const x = gate.begin();
  const y = gate.begin();
  eq(gate.end(x, true), true, "the older first is shown");
  eq(gate.end(y, true), true, "the newer after it is shown");
  // a failed newer read does not make the older answer stale
  const p = gate.begin();
  const q = gate.begin();
  eq(gate.end(q, false), false, "the failed one shows nothing");
  eq(gate.end(p, true), true, "the older answer is still shown");
}

console.log("vcamPoll: ok");
