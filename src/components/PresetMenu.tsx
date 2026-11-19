import { useCallback, useEffect, useState, type FormEvent } from "react";
import { useStore } from "../store";
import { PRESET_NAME } from "../filters/chain";
import { isDefaultPreset, presetSignature, presetStateOf } from "../presets/preset";
import { Icon } from "./Icon";
import { Popover } from "./Popover";

type StoreState = ReturnType<typeof useStore.getState>;

// The selector runs on every store change, not only on one that matters here: work out the fingerprint once per set of inputs.
let cached: { deps: unknown[]; sig: string; isDefault: boolean } | null = null;
const fingerprint = (s: StoreState) => {
  const deps = [s.chain, s.source, s.output, s.controls, s.cameras];
  if (!cached || deps.some((d, i) => d !== cached!.deps[i])) {
    const state = presetStateOf(s.chain, s.source, s.output, s.controls, s.cameras);
    cached = { deps, sig: presetSignature(state), isDefault: isDefaultPreset(state) };
  }
  return cached;
};

const Pencil = () => (
  <svg width={14} height={14} viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth={1.5} strokeLinecap="round" strokeLinejoin="round" aria-hidden>
    <path d="M10.5 3.5l2 2M3 13l.6-2.6 6.9-6.9 2 2-6.9 6.9z" />
  </svg>
);

export function PresetMenu() {
  const presets = useStore((s) => s.presets);
  const activePreset = useStore((s) => s.activePreset);
  const loading = useStore((s) => s.presetLoading);
  // Two booleans, not the fingerprint: it changes with every step of a slider drag, they do not.
  const isDefault = useStore((s) => fingerprint(s).isDefault);
  const dirty = useStore((s) => (s.activePreset ? fingerprint(s).sig !== s.presetBaseline : !fingerprint(s).isDefault));
  const { loadPreset, savePreset, saveActivePreset, deletePreset, renamePreset, resetAll } = useStore.getState();

  const [open, setOpen] = useState(false);
  const [saving, setSaving] = useState(false);
  const [name, setName] = useState("");
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [newName, setNewName] = useState("");

  const close = useCallback(() => {
    setOpen(false);
    setSaving(false);
    setName("");
    setConfirmDelete(null);
    setRenaming(null);
  }, []);

  const label = loading ?? activePreset ?? (isDefault ? "Default" : "Unsaved");
  const modified = loading === null && activePreset !== null && dirty;

  // Save overwrites the active preset; with none active it asks for a name instead.
  const save = useCallback(() => {
    if (!activePreset) {
      setOpen(true);
      setSaving(true);
    } else if (dirty) {
      void saveActivePreset();
    }
  }, [activePreset, dirty, saveActivePreset]);

  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") {
        e.preventDefault();
        save();
      }
    };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, [save]);

  const trimmed = name.trim();
  const existing = presets.find((p) => p.toLowerCase() === trimmed.toLowerCase());
  const invalid = trimmed !== "" && !PRESET_NAME.test(trimmed);
  const hint = invalid
    ? "Use letters, numbers, spaces, - and _ only."
    : existing
      ? `“${existing}” already exists. Saving replaces it.`
      : "";

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (!trimmed || invalid) return;
    void savePreset(existing ?? trimmed).then(close);
  };

  // Rename: another preset with the name is refused; a change of letter case only (the same preset) is fine.
  const renamed = newName.trim();
  const taken = renaming === null ? undefined : presets.find((p) => p.toLowerCase() === renamed.toLowerCase() && p.toLowerCase() !== renaming.toLowerCase());
  const renameInvalid = renamed !== "" && !PRESET_NAME.test(renamed);
  const renameHint = renameInvalid ? "Use letters, numbers, spaces, - and _ only." : taken ? `“${taken}” already exists.` : "";

  const submitRename = (e: FormEvent) => {
    e.preventDefault();
    if (renaming === null || !renamed || renameInvalid || taken) return;
    const from = renaming;
    setRenaming(null);
    if (renamed !== from) void renamePreset(from, renamed);
  };

  return (
    <>
      <Popover
        open={open}
        onClose={close}
        className="presets-menu"
        anchor={
          <button
            className="preset-trigger"
            aria-haspopup="menu"
            aria-expanded={open}
            aria-label={modified ? `${label}, modified` : undefined}
            title={modified ? "Modified. Ctrl+S saves." : undefined}
            onClick={() => (open ? close() : setOpen(true))}
          >
            <span className="label">Preset</span>
            {loading !== null && <span className="spinner" />}
            <span className="pt-label">
              <span className="pt-name">{label}</span>
              {modified && (
                <span className="pt-mod" aria-hidden>
                  *
                </span>
              )}
            </span>
            <Icon name="chevron" />
          </button>
        }
      >
        <div className="pm-list">
          {presets.length === 0 && <p className="pm-empty">No saved presets yet.</p>}
          {presets.map((p) =>
            confirmDelete === p ? (
              <div className="pm-row confirm" key={p}>
                <span>Delete “{p}”?</span>
                <button
                  className="danger"
                  onClick={() => {
                    setConfirmDelete(null);
                    void deletePreset(p);
                  }}
                >
                  Delete
                </button>
                <button className="ghost" onClick={() => setConfirmDelete(null)}>
                  Cancel
                </button>
              </div>
            ) : renaming === p ? (
              <form
                className="pm-rename"
                key={p}
                onSubmit={submitRename}
                onKeyDown={(e) => {
                  if (e.key !== "Escape") return;
                  // Cancels the rename only; the menu stays.
                  e.stopPropagation();
                  setRenaming(null);
                }}
              >
                <input autoFocus maxLength={40} value={newName} onChange={(e) => setNewName(e.target.value)} aria-label={`New name for ${p}`} />
                <button className="primary" type="submit" disabled={!renamed || renameInvalid || !!taken}>
                  Rename
                </button>
                {renameHint && <small className={renameInvalid ? "bad" : ""}>{renameHint}</small>}
              </form>
            ) : (
              <div className={`pm-row${p === activePreset ? " active" : ""}`} key={p} role="menuitem">
                <button
                  className="pm-load"
                  onClick={() => {
                    void loadPreset(p);
                    close();
                  }}
                >
                  <span className="pm-check">{p === activePreset && <Icon name="check" />}</span>
                  <span className="pm-name">{p}</span>
                </button>
                <button
                  className="ghost icon"
                  aria-label={`Rename ${p}`}
                  title="Rename"
                  onClick={() => {
                    setRenaming(p);
                    setNewName(p);
                    setConfirmDelete(null);
                  }}
                >
                  <Pencil />
                </button>
                <button className="ghost icon" aria-label={`Delete ${p}`} title="Delete" onClick={() => setConfirmDelete(p)}>
                  <Icon name="trash" />
                </button>
              </div>
            ),
          )}
        </div>
        <p className="pm-note">Presets save the filters, the camera and its controls, Better Cam auto and the output.</p>
        <div className="pm-sep" />
        {saving ? (
          <form className="pm-save" onSubmit={submit}>
            <input
              autoFocus
              placeholder="Name this preset"
              maxLength={40}
              value={name}
              onChange={(e) => setName(e.target.value)}
              aria-label="Preset name"
            />
            <button className="primary" type="submit" disabled={!trimmed || invalid}>
              {existing ? "Replace" : "Save"}
            </button>
            {hint && <small className={invalid ? "bad" : ""}>{hint}</small>}
          </form>
        ) : (
          <button className="pm-item" onClick={() => setSaving(true)}>
            <Icon name="plus" /> Save as new preset…
          </button>
        )}
        <button
          className="pm-item"
          onClick={() => {
            void resetAll();
            close();
          }}
        >
          <Icon name="reset" /> Reset to defaults
        </button>
      </Popover>
      <button
        className={dirty ? "toggled" : ""}
        disabled={!!activePreset && !dirty}
        onClick={save}
        title={activePreset ? `Save changes to “${activePreset}” (Ctrl+S)` : "Save as a preset (Ctrl+S)"}
      >
        {activePreset ? "Save" : "Save as…"}
      </button>
    </>
  );
}
