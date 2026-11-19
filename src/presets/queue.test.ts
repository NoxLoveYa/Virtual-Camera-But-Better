/**
 * Checks of the apply queue: one job at a time, the latest waiting load wins, the controls that wait for the camera never
 * push out a load, a save is never dropped, the title bar's label follows the job. `npm run test:preset` bundles and runs it.
 * It throws at the first failure and prints `ok` otherwise.
 */
import { createQueue } from "./queue";

const same = (got: unknown, want: unknown, what: string) => {
  const a = JSON.stringify(got);
  const b = JSON.stringify(want);
  if (a !== b) throw new Error(`${what}: got ${a}, wanted ${b}`);
};

/** A job that runs when `finish` is called; `ran` lists the jobs that started, in order. */
function rig() {
  const ran: string[] = [];
  const labels: (string | null)[] = [];
  const errors: string[] = [];
  const queue = createQueue({ label: (l) => labels.push(l), fail: (e) => errors.push(String(e)) });
  const gates = new Map<string, () => void>();
  const job = (name: string, throwing = false) => () => {
    ran.push(name);
    return new Promise<void>((resolve, reject) => {
      gates.set(name, () => (throwing ? reject(new Error(name)) : resolve()));
    });
  };
  const finish = async (name: string) => {
    gates.get(name)!();
    // Let the queue move on (promise callbacks only: no timers in this file).
    for (let i = 0; i < 10; i++) await Promise.resolve();
  };
  return { queue, ran, labels, errors, job, finish };
}

const ended = (p: Promise<void>) => {
  const state = { done: false };
  void p.then(() => (state.done = true));
  return state;
};
const tick = async () => {
  for (let i = 0; i < 10; i++) await Promise.resolve();
};

// An idle queue runs at once and is busy until the job ends.
{
  const r = rig();
  const a = ended(r.queue.serial("A", r.job("A")));
  same(r.ran, ["A"], "an idle queue runs the job at once");
  same(r.queue.busy(), true, "busy while it runs");
  await r.finish("A");
  same([a.done, r.queue.busy()], [true, false], "the job ended, the queue is idle");
  same(r.labels, ["A", null], "the label is set, then cleared at the end");
}

// Latest wins for labelled jobs: the middle one ends without running.
{
  const r = rig();
  void r.queue.serial("A", r.job("A"));
  const b = ended(r.queue.serial("B", r.job("B")));
  const c = ended(r.queue.serial("C", r.job("C")));
  await tick();
  same([b.done, c.done], [true, false], "a replaced waiting load ends at once");
  await r.finish("A");
  same(r.ran, ["A", "C"], "the latest waiting load runs, the replaced one never");
  await r.finish("C");
  same(r.labels, ["A", "C", null], "the label follows the load that runs");
}

// An unlabelled job (the controls waiting for the camera) never replaces a waiting labelled one.
{
  const r = rig();
  void r.queue.serial("A", r.job("A"));
  const load = ended(r.queue.serial("B", r.job("B")));
  const pending = ended(r.queue.serial(null, r.job("pending")));
  await tick();
  same([load.done, pending.done], [false, true], "the unlabelled job is dropped, the load waits");
  await r.finish("A");
  same(r.ran, ["A", "B"], "the labelled load runs after the current one");
  await r.finish("B");
  same(r.ran, ["A", "B"], "the dropped job never runs");
}

// A labelled job replaces a waiting unlabelled one; an unlabelled one replaces an unlabelled one.
{
  const r = rig();
  void r.queue.serial("A", r.job("A"));
  const p1 = ended(r.queue.serial(null, r.job("p1")));
  const p2 = ended(r.queue.serial(null, r.job("p2")));
  await tick();
  same([p1.done, p2.done], [true, false], "the latest unlabelled job wins over an older one");
  const load = ended(r.queue.serial("B", r.job("B")));
  await tick();
  same([p2.done, load.done], [true, false], "a load replaces waiting controls");
  await r.finish("A");
  await r.finish("B");
  same(r.ran, ["A", "B"], "only the load ran");
}

// The title bar's label: an unlabelled job clears it, so the spinner never keeps the name of a load that is over.
{
  const r = rig();
  void r.queue.serial("A", r.job("A"));
  void r.queue.serial(null, r.job("pending"));
  await tick();
  await r.finish("A");
  same(r.labels, ["A", null], "the unlabelled job sets no label");
  await r.finish("pending");
  same(r.labels, ["A", null, null], "and the end clears it");
}

// An exclusive job (a save) is never dropped and never replaces one, and runs in order.
{
  const r = rig();
  void r.queue.serial("A", r.job("A"));
  const save = ended(r.queue.exclusive(r.job("save")));
  const load = ended(r.queue.serial("B", r.job("B")));
  const pending = ended(r.queue.serial(null, r.job("pending")));
  const load2 = ended(r.queue.serial("C", r.job("C")));
  await tick();
  same([save.done, load.done, pending.done, load2.done], [false, true, true, false], "a save waits; later loads replace each other, not the save");
  await r.finish("A");
  same(r.ran, ["A", "save"], "the save runs after the current job and before the later load");
  await r.finish("save");
  await r.finish("C");
  same(r.ran, ["A", "save", "C"], "then the latest load");
  same(r.queue.busy(), false, "idle at the end");
}

// A job that throws is reported and the queue goes on.
{
  const r = rig();
  const a = ended(r.queue.serial("A", r.job("A", true)));
  void r.queue.serial("B", r.job("B"));
  await tick();
  await r.finish("A");
  same([a.done, r.errors, r.ran], [true, ["Error: A"], ["A", "B"]], "a failure is reported and the next job runs");
  await r.finish("B");
}

console.log("ok");
