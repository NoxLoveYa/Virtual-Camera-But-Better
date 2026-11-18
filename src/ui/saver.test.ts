/**
 * Checks of the throttled saver: a burst of saves is one write per 300 ms, the write is the last one, and a flush
 * writes at once. A fake clock drives it. `npm run test:ui` bundles and runs it. It throws at the first failure.
 */
import { answerFlushRequest, createSaver, type Timers } from "./saver";

function eq<T>(actual: T, expected: T, what: string) {
  if (actual !== expected) throw new Error(`${what}: expected ${String(expected)}, got ${String(actual)}`);
}

/** A clock that only moves when the test says so. */
function fakeClock() {
  let now = 0;
  let next = 1;
  const queue = new Map<number, { at: number; fn: () => void }>();
  const timers: Timers = {
    set: (fn, ms) => {
      const id = next++;
      queue.set(id, { at: now + ms, fn });
      return id;
    },
    clear: (h) => void queue.delete(h as number),
  };
  const advance = (ms: number) => {
    const end = now + ms;
    for (;;) {
      const due = [...queue.entries()].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
      if (!due) break;
      queue.delete(due[0]);
      now = due[1].at;
      due[1].fn();
    }
    now = end;
  };
  return { timers, advance, timersLeft: () => queue.size, now: () => now };
}

// A burst of 100 changes in 2 s (every 20 ms) is one write per 300 ms, and the last write holds the last value.
{
  const clock = fakeClock();
  let state = 0;
  const writes: Array<{ at: number; value: number }> = [];
  const saver = createSaver(() => writes.push({ at: clock.now(), value: state }), 300, clock.timers);
  for (let i = 1; i <= 100; i++) {
    state = i;
    saver.schedule();
    clock.advance(20);
  }
  eq(writes.length, 6, "writes during a 2 s burst (one per 300 ms)");
  clock.advance(300);
  eq(writes.length, 7, "the trailing write after the burst");
  eq(writes[writes.length - 1].value, 100, "the last write holds the newest value");
  for (let i = 1; i < writes.length; i++) eq(writes[i].at - writes[i - 1].at >= 300, true, `writes ${i - 1} and ${i} are at least 300 ms apart`);
  eq(saver.pending, false, "nothing waits after the trailing write");
  eq(clock.timersLeft(), 0, "no timer is left");
}

// A single change is written once, 300 ms later, not before.
{
  const clock = fakeClock();
  let writes = 0;
  const saver = createSaver(() => writes++, 300, clock.timers);
  saver.schedule();
  eq(writes, 0, "no write at once");
  clock.advance(299);
  eq(writes, 0, "no write before 300 ms");
  eq(saver.pending, true, "a save waits");
  clock.advance(1);
  eq(writes, 1, "one write at 300 ms");
  clock.advance(5000);
  eq(writes, 1, "and no more when nothing changed");
}

// The write reads the state when it runs, not when the change was made.
{
  const clock = fakeClock();
  let state = "a";
  let stored = "";
  const saver = createSaver(() => (stored = state), 300, clock.timers);
  saver.schedule();
  state = "b";
  saver.schedule();
  state = "c";
  clock.advance(300);
  eq(stored, "c", "the stored value is the state at the write");
}

// A flush writes at once, stops the timer (no second write) and does nothing when nothing waits.
{
  const clock = fakeClock();
  let state = 0;
  let stored = -1;
  let writes = 0;
  const saver = createSaver(
    () => {
      writes++;
      stored = state;
    },
    300,
    clock.timers,
  );
  saver.flush();
  eq(writes, 0, "flush with nothing waiting writes nothing");
  state = 7;
  saver.schedule();
  clock.advance(100);
  saver.flush();
  eq(writes, 1, "flush writes at once");
  eq(stored, 7, "flush writes the newest value");
  eq(saver.pending, false, "nothing waits after a flush");
  eq(clock.timersLeft(), 0, "the timer is stopped");
  clock.advance(1000);
  eq(writes, 1, "the stopped timer does not write again");
  saver.flush();
  eq(writes, 1, "a second flush writes nothing");
  // A change after a flush starts a new 300 ms wait.
  state = 8;
  saver.schedule();
  clock.advance(299);
  eq(writes, 1, "the next change waits again");
  clock.advance(1);
  eq(stored, 8, "and is written");
}

// `now` writes whether or not a save waits, and takes a waiting one with it.
{
  const clock = fakeClock();
  let writes = 0;
  const saver = createSaver(() => writes++, 300, clock.timers);
  saver.now();
  eq(writes, 1, "now writes with nothing waiting");
  saver.schedule();
  saver.now();
  eq(writes, 2, "now writes once for a waiting save");
  clock.advance(1000);
  eq(writes, 2, "the waiting save is gone");
}

// A write that throws does not leave the saver stuck: the next change is saved again.
{
  const clock = fakeClock();
  let fail = true;
  let writes = 0;
  const saver = createSaver(
    () => {
      writes++;
      if (fail) throw new Error("quota");
    },
    300,
    clock.timers,
  );
  saver.schedule();
  let thrown = false;
  try {
    clock.advance(300);
  } catch {
    thrown = true;
  }
  eq(thrown, true, "the error reaches the timer");
  eq(saver.pending, false, "a failed write does not wedge the saver");
  fail = false;
  saver.schedule();
  clock.advance(300);
  eq(writes, 2, "the next change is written");
}

// The app's exit request: the waiting save is written first, then the acknowledgement goes out, also when the write fails.
{
  const clock = fakeClock();
  const log: string[] = [];
  const saver = createSaver(() => log.push("write"), 300, clock.timers);
  saver.schedule();
  answerFlushRequest(saver.flush, () => log.push("ack"));
  eq(log.join(","), "write,ack", "the save is written, then acknowledged");
  answerFlushRequest(saver.flush, () => log.push("ack"));
  eq(log.join(","), "write,ack,ack", "nothing waiting: acknowledged, nothing written");

  let thrown = false;
  const failing = createSaver(() => {
    log.push("write!");
    throw new Error("quota");
  }, 300, clock.timers);
  failing.schedule();
  try {
    answerFlushRequest(failing.flush, () => log.push("ack!"));
  } catch {
    thrown = true;
  }
  eq(thrown, true, "the write error is not swallowed");
  eq(log.slice(3).join(","), "write!,ack!", "a failed write is still acknowledged");
}

console.log("saver: ok");
