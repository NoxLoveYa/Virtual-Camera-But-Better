import { api } from "../api";
import { isDeviceDefault } from "../camera/schema";
import { useStore } from "../store";

const sentence = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

/**
 * Which switch the engine's words for a privacy wait name. The engine says "privacy" only for an open that Windows
 * refused (access denied) while a switch is off; the switches only decide this wording.
 */
function privacyText(reason: string | null): string {
  const r = reason ?? "";
  const afterwards = "Better Cam goes live by itself afterwards.";
  if (r.includes("policy")) {
    return `Windows is not letting apps use the camera: a Windows policy turns camera access off, and no setting in Windows Settings changes it. ${afterwards}`;
  }
  if (r.includes("desktop apps")) {
    return `Windows is not letting desktop apps use the camera. Turn on “Let desktop apps access your camera” in Windows camera privacy settings. ${afterwards}`;
  }
  return `Windows is not letting apps use the camera. Turn on “Camera access” in Windows camera privacy settings. ${afterwards}`;
}

/** What the banner says while the camera is being waited for, by the engine's `kind` (plan 4.9). `reason` is the detail. */
function waitingText(kind: string | null, reason: string | null): string {
  switch (kind) {
    case "in_use":
      return "Camera is in use by another app. Close it or pick another camera. Better Cam goes live as soon as it is free.";
    case "disconnected":
      return "The camera was disconnected. Plug it in again; Better Cam goes live as soon as it is back.";
    case "not_found":
      return "Camera not found. Connect it or pick another camera; Better Cam goes live as soon as it is there.";
    case "privacy":
      return privacyText(reason);
    default:
      return `${reason ? sentence(reason) : "The camera is not answering"}. Better Cam keeps trying for a little while.`;
  }
}

/**
 * The one place for engine problems: a warning banner while the camera is waited for (what is wrong in plain words,
 * the engine's own words in a tooltip, and one button where it helps), and the error banner for a stop. Click an error
 * to dismiss it.
 */
export function EngineBanner() {
  const state = useStore((s) => s.engine.state);
  const reason = useStore((s) => s.engine.reason);
  const kind = useStore((s) => s.engine.kind);
  const error = useStore((s) => s.error);
  const errorKind = useStore((s) => s.errorKind);
  const onDefault = useStore((s) => isDeviceDefault(s.source));
  const { dismissError, startWithDeviceDefault } = useStore.getState();

  const waiting = state === "reconnecting";
  if (!error && !waiting) return null;

  return (
    <>
      {error && (
        <div className={`banner error${errorKind === "format" && !onDefault ? " with-action" : ""}`} onClick={dismissError}>
          <span>{error}</span>
          {errorKind === "format" && !onDefault && (
            <button
              className="primary"
              title="Starts the camera with its own default format. Your saved format stays as it is; the next Start tries it again."
              onClick={(e) => {
                e.stopPropagation();
                dismissError();
                void startWithDeviceDefault();
              }}
            >
              Use device default
            </button>
          )}
        </div>
      )}
      {waiting && (
        <div className="banner warn" role="status" title={reason ?? undefined}>
          <span>{waitingText(kind, reason)}</span>
          {kind === "privacy" && (
            <button className="primary" onClick={() => void api.openCameraSettings().catch(() => {})}>
              Open camera settings
            </button>
          )}
        </div>
      )}
    </>
  );
}
