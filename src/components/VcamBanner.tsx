import { useStore } from "../store";

/** Why the install/update/repair button is off while `%ProgramData%` itself is encrypted (the problem's sentence says what to do). */
const BLOCKED_REASON = "An administrator must turn off encryption first (see below).";

/**
 * Shown while the virtual camera is not installed or is older than the one this app ships (plan 7.2), and, apart,
 * while its data folder is encrypted (Windows EFS): the camera is black then whatever the registration says. Repair
 * puts that right, so the problem's banner has its own Repair button, except
 * - while the install/update banner is shown (its button repairs it too), and
 * - when Repair cannot help (`repairable` false: `%ProgramData%` itself is encrypted, the sentence says what an
 *   administrator does; the button comes back by itself once the next status says "Run Repair").
 */
export function VcamBanner() {
  const state = useStore((s) => s.vcam?.state);
  const problem = useStore((s) => s.vcam?.problem ?? null);
  const repairable = useStore((s) => s.vcam?.repairable ?? true);
  const busy = useStore((s) => s.vcamBusy);
  const runVcam = useStore((s) => s.runVcam);

  const stale = state === "missing" || state === "outdated";
  if (!stale && !problem) return null;
  const missing = state === "missing";
  const action = missing ? "install" : "update";
  const working = busy === "install" || busy === "update";
  const label = missing ? "Install virtual camera" : "Update virtual camera";
  // The install/update does what Repair does, so it is refused the same way when Repair cannot help.
  const blocked = !repairable;

  return (
    <>
      {stale && (
        <div className="banner warn" role="status">
          <span>
            {missing
              ? "Better Cam's virtual camera isn't installed. Installing asks for administrator rights and sets the camera up for this Windows account."
              : "The virtual camera needs an update. It will then work for this Windows account."}
            {blocked && (
              <>
                <br />
                {BLOCKED_REASON}
              </>
            )}
          </span>
          <button
            className="primary"
            disabled={busy !== null || blocked}
            title={blocked ? BLOCKED_REASON : undefined}
            onClick={() => void runVcam(action)}
          >
            {working ? (missing ? "Installing…" : "Updating…") : label}
          </button>
        </div>
      )}
      {problem && (
        <div className="banner warn" role="status">
          {/* the sentence says "Run Repair"; while the install/update banner shows, its button is the one that does it */}
          <span>{stale && repairable ? problem.replace(/\bRun Repair\b/, `Use “${label}” above`) : problem}</span>
          {repairable && !stale && (
            <button className="primary" disabled={busy !== null} onClick={() => void runVcam("repair")}>
              {busy === "repair" ? "Repairing…" : "Repair"}
            </button>
          )}
        </div>
      )}
    </>
  );
}
