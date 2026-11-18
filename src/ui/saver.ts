/**
 * Coalesces saves: many `schedule()` calls make one `write()` at most every `wait` ms (trailing, so the write that comes
 * reads the newest state), and `flush()` writes at once when a save is waiting (the page is going away, or an action
 * needs the stored value to be current). A burst therefore leaves the same stored value as saving on every change did;
 * only the number of writes and their timing differ.
 *
 * The timer functions are parameters so a test can drive the clock.
 */
export interface Saver {
  /** Asks for a save: the first call after a write starts the timer, later ones until it fires add nothing. */
  schedule: () => void;
  /** Writes now if a save is waiting (and stops its timer); does nothing otherwise. */
  flush: () => void;
  /** Writes now, whether one is waiting or not. */
  now: () => void;
  /** A save is waiting. */
  readonly pending: boolean;
}

export interface Timers {
  set: (fn: () => void, ms: number) => unknown;
  clear: (handle: unknown) => void;
}

const REAL: Timers = { set: (fn, ms) => setTimeout(fn, ms), clear: (h) => clearTimeout(h as ReturnType<typeof setTimeout>) };

/**
 * The answer to the app's "save now, I am about to exit or hide" request: flushes, then acknowledges. The
 * acknowledgement goes out even when the flush throws (storage full), so the app does not wait out its timeout for a
 * save that can no longer happen.
 */
export function answerFlushRequest(flush: () => void, acknowledge: () => void): void {
  try {
    flush();
  } finally {
    acknowledge();
  }
}

export function createSaver(write: () => void, wait = 300, timers: Timers = REAL): Saver {
  let timer: unknown;
  let waiting = false;

  const run = () => {
    if (waiting) timers.clear(timer);
    timer = undefined;
    // Cleared before the write: a write that throws (storage full) must not leave the saver stuck "waiting".
    waiting = false;
    write();
  };

  return {
    schedule: () => {
      if (waiting) return;
      waiting = true;
      timer = timers.set(run, wait);
    },
    flush: () => {
      if (waiting) run();
    },
    now: run,
    get pending() {
      return waiting;
    },
  };
}
