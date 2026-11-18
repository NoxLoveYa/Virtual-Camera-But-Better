import { api } from "../api";
import { isDeviceDefault, problemText, stripCodes } from "../camera/schema";
import { useStore } from "../store";
import { Icon } from "./Icon";

/**
 * The one place for engine problems: a warning banner while the camera is waited for (what is wrong in plain words,
 * the engine's own words in a tooltip, and one button where it helps), and the error banner for a stop. The error
 * banner stays until its Dismiss button is pressed.
 */
export function EngineBanner() {
  const state = useStore((s) => s.engine.state);
  const reason = useStore((s) => s.engine.reason);
  const kind = useStore((s) => s.engine.kind);
  const apps = useStore((s) => s.engine.apps);
  const appsMaybe = useStore((s) => s.engine.appsMaybe);
  const error = useStore((s) => s.error);
  const errorKind = useStore((s) => s.errorKind);
  const onDefault = useStore((s) => isDeviceDefault(s.source));
  const { dismissError, startWithDeviceDefault } = useStore.getState();

  const waiting = state === "reconnecting";
  if (!error && !waiting) return null;

  return (
    <>
      {error && (
        <div className="banner error" role="alert">
          <span className="banner-text" title={error}>
            {stripCodes(error)}
          </span>
          {errorKind === "format" && !onDefault && (
            <button
              className="primary"
              title="Starts the camera with its own default format. Your saved format stays as it is; the next Start tries it again."
              onClick={() => {
                dismissError();
                void startWithDeviceDefault();
              }}
            >
              Use device default
            </button>
          )}
          <button className="ghost icon" aria-label="Dismiss" title="Dismiss" onClick={dismissError}>
            <Icon name="close" />
          </button>
        </div>
      )}
      {waiting && (
        <div className="banner warn" role="status" title={reason ?? undefined}>
          <span className="banner-text">{problemText(kind, reason, apps, appsMaybe).long}</span>
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
