import type { VcamStatus } from "../api";

/**
 * Whether the virtual camera's status is read again on a timer: only while a problem is shown whose Repair button is
 * hidden (`repairable` false: an administrator has to act, and the button must come back without a click). A problem
 * Repair can fix needs no poll: the button is there.
 */
export function wantsVcamPoll(vcam: Pick<VcamStatus, "problem" | "repairable"> | null | undefined): boolean {
  return !!vcam?.problem && vcam.repairable === false;
}

/**
 * Bookkeeping for overlapping status reads (a focus read and a timer tick): a tick is skipped while a read is
 * pending, and an answer that is older than one already shown is dropped.
 */
export function createReadGate() {
  let issued = 0;
  let applied = 0;
  let inFlight = 0;
  return {
    /** A read starts; the number identifies it for `end`. */
    begin(): number {
      inFlight++;
      return ++issued;
    },
    /** The read `seq` is over (`ok`: it has an answer). `true` when that answer is to be shown. */
    end(seq: number, ok: boolean): boolean {
      inFlight--;
      if (!ok || seq < applied) return false;
      applied = seq;
      return true;
    },
    /** A read is pending: a tick waits for the next one. */
    busy(): boolean {
      return inFlight > 0;
    },
  };
}
