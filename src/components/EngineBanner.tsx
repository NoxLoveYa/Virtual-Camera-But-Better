import { api } from "../api";
import { useStore } from "../store";

const sentence = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

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
      return "Windows is not letting desktop apps use the camera. Turn on “Let desktop apps access your camera” in Windows camera privacy settings; Better Cam goes live by itself afterwards.";
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
  const onDefault = useStore((s) => s.source.res_type === "preferred" && s.source.video_format === "any");
  const { dismissError, patchSource } = useStore.getState();

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
              onClick={(e) => {
                e.stopPropagation();
                patchSource({ res_type: "preferred", video_format: "any" });
                dismissError();
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
