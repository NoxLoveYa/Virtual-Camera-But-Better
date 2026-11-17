import { useStore } from "../store";

/** Shown while the virtual camera is not installed or is older than the one this app ships (plan 7.2). */
export function VcamBanner() {
  const state = useStore((s) => s.vcam?.state);
  const busy = useStore((s) => s.vcamBusy);
  const runVcam = useStore((s) => s.runVcam);

  if (state !== "missing" && state !== "outdated") return null;
  const missing = state === "missing";
  const action = missing ? "install" : "update";
  const working = busy === "install" || busy === "update";

  return (
    <div className="banner warn" role="status">
      <span>
        {missing
          ? "Better Cam's virtual camera isn't installed. Installing asks for administrator rights."
          : "The virtual camera needs an update."}
      </span>
      <button className="primary" disabled={busy !== null} onClick={() => void runVcam(action)}>
        {working ? (missing ? "Installing…" : "Updating…") : missing ? "Install virtual camera" : "Update virtual camera"}
      </button>
    </div>
  );
}
