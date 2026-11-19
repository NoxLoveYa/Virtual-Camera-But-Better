/**
 * The queue behind "one apply at a time" (docs/LAYOUT-INSPECTOR.md 3.3). Pure: no store, no React.
 *
 * A job is one of two kinds:
 * - `serial` (a preset load, a reset, an Undo, the controls waiting for the camera): while another job runs it waits, and only
 *   the latest waiting one runs, the others end at once without running. A job with no label (the controls waiting for the
 *   camera) never replaces a waiting job that has one (a load the user asked for): it is dropped instead, since the load
 *   clears what it would have applied.
 * - `exclusive` (a save, an Undo of the chain alone): never dropped, never replaces another; runs in order.
 */
export interface QueueHooks {
  /** The label of the job that starts (`null`: it has none), and `null` when the queue is empty. The title bar shows it. */
  label: (label: string | null) => void;
  /** A job threw. */
  fail: (error: unknown) => void;
}

export interface Queue {
  serial: (label: string | null, job: () => Promise<void>) => Promise<void>;
  exclusive: (job: () => Promise<void>) => Promise<void>;
  /** A job runs or waits. */
  busy: () => boolean;
}

interface Entry {
  label: string | null;
  replaceable: boolean;
  job: () => Promise<void>;
  done: () => void;
}

export function createQueue(hooks: QueueHooks): Queue {
  let running = false;
  const waiting: Entry[] = [];

  const drain = async () => {
    running = true;
    let cur: Entry | undefined;
    while ((cur = waiting.shift())) {
      // Every job sets the label, an unlabelled one clears it: the spinner never shows the name of a job that is over.
      hooks.label(cur.label);
      try {
        await cur.job();
      } catch (e) {
        hooks.fail(e);
      }
      cur.done();
    }
    running = false;
    hooks.label(null);
  };

  const add = (label: string | null, replaceable: boolean, job: () => Promise<void>) =>
    new Promise<void>((done) => {
      if (replaceable && label === null && waiting.some((w) => w.replaceable && w.label !== null)) {
        done();
        return;
      }
      if (replaceable) {
        for (let i = waiting.length - 1; i >= 0; i--) {
          if (!waiting[i].replaceable) continue;
          waiting[i].done();
          waiting.splice(i, 1);
        }
      }
      waiting.push({ label, replaceable, job, done });
      if (!running) void drain();
    });

  return {
    serial: (label, job) => add(label, true, job),
    exclusive: (job) => add(null, false, job),
    busy: () => running || waiting.length > 0,
  };
}
