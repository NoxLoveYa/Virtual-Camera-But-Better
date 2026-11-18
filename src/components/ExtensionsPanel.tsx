import { useEffect, useRef } from "react";
import { createPortal } from "react-dom";
import { useStore } from "../store";
import type { ExtensionInfo } from "../filters/schema";

const KIND = { filter: "Filter", overlay: "Overlay", widget: "Widget" } as const;
const STATE = { ready: "Ready", disabled: "Switched off", error: "Error" } as const;

function Row({ ext }: { ext: ExtensionInfo }) {
  const setEnabled = useStore((s) => s.setExtensionEnabled);
  return (
    <li className={`ext-item ${ext.state}`} data-ext-id={ext.id} data-state={ext.state}>
      <div className="ext-head">
        <b className="ext-name">{ext.name}</b>
        {ext.version && <span className="ext-meta">{ext.version}</span>}
        {ext.kind && <span className="ext-meta">{KIND[ext.kind]}</span>}
        <span className={`ext-state ${ext.state}`}>{STATE[ext.state]}</span>
        <span className="fill" />
        {ext.state !== "error" && (
          <button
            role="switch"
            aria-checked={ext.enabled}
            aria-label={`${ext.name} enabled`}
            className="switch"
            title={ext.enabled ? "Switch off. Its cards stay in the chain and are bypassed." : "Switch on"}
            onClick={() => void setEnabled(ext.id, !ext.enabled)}
          />
        )}
      </div>
      {ext.hint && <small className="ext-hint">{ext.hint}</small>}
      {ext.state !== "error" && ext.kind && ext.kind !== "filter" && (
        <small className="ext-hint">Overlays and widgets can't be added to the camera yet.</small>
      )}
      {ext.error && (
        <p className="filter-error" role="alert">
          {ext.error}
        </p>
      )}
      <small className="ext-folder" title={ext.folder}>
        {ext.folder}
      </small>
    </li>
  );
}

/** Settings → Extensions: what the app found in the extensions folder, with its state and errors. */
export function ExtensionsPanel() {
  const open = useStore((s) => s.extensionsOpen);
  const extensions = useStore((s) => s.extensions);
  const { setExtensionsOpen, reloadExtensions, openExtensionsFolder } = useStore.getState();
  const close = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!open) return;
    close.current?.focus({ preventScroll: true });
    const esc = (e: KeyboardEvent) => e.key === "Escape" && setExtensionsOpen(false);
    document.addEventListener("keydown", esc);
    return () => document.removeEventListener("keydown", esc);
  }, [open, setExtensionsOpen]);

  if (!open) return null;
  const errors = extensions.filter((e) => e.state === "error").length;

  return createPortal(
    <div className="backdrop" onMouseDown={() => setExtensionsOpen(false)}>
      <div className="modal ext-modal" role="dialog" aria-modal="true" aria-labelledby="ext-title" onMouseDown={(e) => e.stopPropagation()}>
        <h2 id="ext-title">Extensions</h2>
        <p>
          Extensions are folders in the extensions folder. Copy one in and it shows up here within a second; Better Cam watches the
          folder. They run code on your PC: install only extensions from people you trust.
        </p>
        {extensions.length === 0 ? (
          <p className="ext-empty" data-ext-empty>
            No extensions found. Open the folder and copy an extension folder into it.
          </p>
        ) : (
          <>
            {errors > 0 && <p className="ext-summary">{errors === 1 ? "1 extension has a problem." : `${errors} extensions have a problem.`}</p>}
            <ul className="ext-list">
              {extensions.map((e) => (
                <Row key={e.id} ext={e} />
              ))}
            </ul>
          </>
        )}
        <div className="modal-actions">
          <button onClick={() => void openExtensionsFolder()}>Open folder</button>
          <button onClick={() => void reloadExtensions()}>Reload</button>
          <button ref={close} className="primary" onClick={() => setExtensionsOpen(false)}>
            Close
          </button>
        </div>
      </div>
    </div>,
    document.body,
  );
}
