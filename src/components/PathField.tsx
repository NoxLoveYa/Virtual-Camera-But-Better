import { useEffect, useRef, useState } from "react";
import { api } from "../api";
import { registerField } from "../ui/fileDrop";

interface Props {
  /** The full path, or "" for none. */
  value: string;
  /** Which file dialog: pictures, or colour lookup tables. */
  kind: "image" | "lut";
  onChange: (value: string) => void;
  /** Names the controls for screen readers. */
  label: string;
}

/** Splits a Windows or POSIX path into the folder part (with its trailing separator), the file name without its extension and the extension (with the dot). */
export function splitPath(path: string): { dir: string; stem: string; ext: string } {
  const i = Math.max(path.lastIndexOf("\\"), path.lastIndexOf("/"));
  const name = path.slice(i + 1);
  const dot = name.lastIndexOf(".");
  return dot > 0 ? { dir: path.slice(0, i + 1), stem: name.slice(0, dot), ext: name.slice(dot) } : { dir: path.slice(0, i + 1), stem: name, ext: "" };
}

/**
 * A file the filter reads: the path (read-only; when it does not fit the folder shortens first, then the file name
 * loses the end of its stem but keeps the extension; the full path is the tooltip), a "Browse…" button for the native
 * file dialog and, once a file is set, a clear button. Both buttons are plain buttons: Tab, Enter and Space work, and
 * focus returns to Browse after the dialog closes and after Clear. A file dropped from Explorer onto the field sets it
 * (the window's drop listener finds the field by `data-drop="path"`, see `ui/useFileDrop.ts`); while a fitting file is
 * over it the path text is replaced by "Drop to use this file" in an accent border.
 */
export function PathField({ value, kind, onChange, label }: Props) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [over, setOver] = useState(false);
  const browseRef = useRef<HTMLButtonElement>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const change = useRef(onChange);
  change.current = onChange;
  const { dir, stem, ext } = splitPath(value);

  useEffect(
    () =>
      registerField(rootRef.current!, {
        kind,
        drop: (path) => {
          setFailure(null);
          change.current(path);
        },
        setOver,
      }),
    [kind],
  );

  const browse = async () => {
    setBusy(true);
    setFailure(null);
    try {
      const picked = await api.pickFile(kind);
      if (picked !== null && picked !== value) onChange(picked);
    } catch (e) {
      setFailure(String(e));
    } finally {
      setBusy(false);
      // The dialog took the focus away; a button that was disabled meanwhile cannot hold it, so wait one render.
      requestAnimationFrame(() => browseRef.current?.focus());
    }
  };

  return (
    <div ref={rootRef} className={`pathfield${over ? " drop-over" : ""}`} role="group" aria-label={label} data-drop="path" data-kind={kind}>
      <div className="pf-line">
        <div className={`pf-text${value ? "" : " empty"}`} title={value || undefined}>
          {over ? (
            <span className="pf-drop">Drop to use this file</span>
          ) : value ? (
            <>
              <span className="pf-dir">{dir}</span>
              <span className="pf-stem">{stem}</span>
              <span className="pf-ext">{ext}</span>
            </>
          ) : (
            "No file"
          )}
        </div>
        <button ref={browseRef} type="button" className="pf-browse" disabled={busy} aria-label={`Browse for ${label}`} onClick={() => void browse()}>
          Browse…
        </button>
        {value && (
          <button
            type="button"
            className="icon ghost pf-clear"
            aria-label={`Clear ${label}`}
            title="Clear"
            onClick={() => {
              onChange("");
              browseRef.current?.focus();
            }}
          >
            <svg width="12" height="12" viewBox="0 0 12 12" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true">
              <path d="M2 2l8 8M10 2l-8 8" />
            </svg>
          </button>
        )}
      </div>
      {failure && (
        <small className="pf-failure" role="alert">
          {failure}
        </small>
      )}
    </div>
  );
}
