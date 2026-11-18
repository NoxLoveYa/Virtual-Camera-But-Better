import { useStore } from "../store";

/**
 * Shown while the virtual camera is not installed or is older than the one this app ships (plan 7.2), and, apart,
 * while its data folder is encrypted (Windows EFS): the camera is black then whatever the registration says, and Repair
 * puts it right (or the sentence says what an administrator does about `%ProgramData%`).
 */
export function VcamBanner() {
  const state = useStore((s) => s.vcam?.state);
  const problem = useStore((s) => s.vcam?.problem ?? null);
  const busy = useStore((s) => s.vcamBusy);
  const runVcam = useStore((s) => s.runVcam);

  const stale = state === "missing" || state === "outdated";
  if (!stale && !problem) return null;
  const missing = state === "missing";
  const action = missing ? "install" : "update";
  const working = busy === "install" || busy === "update";

  return (
    <>
      {stale && (
        <div className="banner warn" role="status">
          <span>
            {missing
              ? "Better Cam's virtual camera isn't installed. Installing asks for administrator rights and sets the camera up for this Windows account."
              : "The virtual camera needs an update. It will then work for this Windows account."}
          </span>
          <button className="primary" disabled={busy !== null} onClick={() => void runVcam(action)}>
            {working ? (missing ? "Installing…" : "Updating…") : missing ? "Install virtual camera" : "Update virtual camera"}
          </button>
        </div>
      )}
      {problem && (
        <div className="banner warn" role="status">
          <span>{problem}</span>
          <button className="primary" disabled={busy !== null} onClick={() => void runVcam("repair")}>
            {busy === "repair" ? "Repairing…" : "Repair"}
          </button>
        </div>
      )}
    </>
  );
}
