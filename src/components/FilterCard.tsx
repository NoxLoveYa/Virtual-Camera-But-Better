import { memo, useCallback, type PointerEvent as ReactPointerEvent } from "react";
import { useStore } from "../store";
import { stripCodes } from "../camera/schema";
import { defOf, extIdOf, extOf, isExtKind, labelOf, type ExtensionInfo, type ParamValue } from "../filters/schema";
import { AddHere } from "./AddFilter";
import { Icon } from "./Icon";
import { ParamField } from "./ParamField";
import { moveWithFocus, type ChainHandlers, type DragView } from "../ui/useChainDrag";

/**
 * The props are the card's id, how it is being dragged (`null` for most cards) and the handlers (one object for the
 * life of the chain): all stable while another card's slider moves. The card reads its own filter, its place and the
 * rest from the store, so a change of another card's params does not render it.
 */
interface Props {
  id: string;
  dragMode: DragView["mode"] | null;
  reorder: ChainHandlers;
}

/** What a card of an extension says when the extension is not running: not installed, failed to load, or switched off. */
function extNoteOf(id: string, ext: ExtensionInfo | undefined) {
  if (!ext) return { state: "missing", failed: true, text: `Extension “${id}” is not installed. The card is bypassed and keeps its settings.` };
  if (ext.state === "error") return { state: "error", failed: true, text: `Extension “${id}” failed to load: ${ext.error ?? "unknown error"}` };
  if (ext.state === "disabled") return { state: "disabled", failed: false, text: `Extension “${ext.name}” is switched off in Settings → Extensions. The card is bypassed.` };
  return null;
}

// The actions never change: read them when used, so the card does not subscribe to them.
const act = () => useStore.getState();

export const FilterCard = memo(function FilterCard({ id, dragMode: mode, reorder }: Props) {
  // A card that was just removed is still mounted until its parent renders again: it has nothing to show then.
  const filter = useStore((s) => s.chain.find((f) => f.id === id));
  const index = useStore((s) => s.chain.findIndex((f) => f.id === id));
  const count = useStore((s) => s.chain.length);
  const extensions = useStore((s) => s.extensions);
  const extensionsRead = useStore((s) => s.extensionsRead);
  const folded = useStore((s) => s.collapsed[id] === true);
  const error = useStore((s) => s.filterErrors[id]);
  const status = useStore((s) => s.filterStatus[id]);
  const gpu = useStore((s) => s.gpu);
  const gpuRead = useStore((s) => s.gpuRead);
  const nvidia = useStore((s) => s.nvidia);
  // One function for the life of the card, so the fields that did not change do not render.
  const onChange = useCallback((key: string, value: ParamValue) => act().setParam(id, key, value), [id]);
  if (!filter) return null;
  // `def` is missing for an extension that is not installed or failed to load: the card keeps its settings and says why.
  const def = defOf(filter.kind, extensions);
  const label = labelOf(filter.kind, extensions);
  const ext = extOf(filter.kind, extensions);
  const extNote = !isExtKind(filter.kind) || !extensionsRead ? null : extNoteOf(extIdOf(filter.kind), ext);
  // What an NVIDIA filter needs before it can run, in the order of precedence: an RTX GPU, then the runtime. The app knows
  // both without the engine, so a card in a loaded preset says it before the first frame (the engine says the same
  // thing, and the picture is passed through meanwhile).
  const needs =
    !def?.rtx || !gpuRead
      ? null
      : !gpu
        ? { text: "Needs an NVIDIA RTX GPU (GeForce RTX 20 series or newer).", install: false }
        : nvidia && !nvidia.available
          ? { text: nvidia.detail, install: true }
          : null;

  const movable = count > 1;
  const gripDown = (e: ReactPointerEvent<HTMLElement>) => reorder.onPointerDown(e, id);

  return (
    <li
      className={`step${filter.enabled ? " on" : ""}${movable ? " movable" : ""}${mode === "pointer" ? " dragging" : ""}${mode === "keyboard" ? " lifted" : ""}`}
      data-filter-id={id}
      onKeyDown={(e) => reorder.onCardKeyDown(e, id)}
    >
      {/* The "+" of the gap above this card. It sits over the gap without taking room, and shows on hover and focus (always on touch). */}
      {index > 0 && (
        <div className="gap-add">
          <AddHere at={index} />
        </div>
      )}
      <span className="num" title={`Step ${index + 1} of ${count}`}>
        {String(index + 1).padStart(2, "0")}
      </span>
      <div className="card">
        <div className="card-head">
          <button
            type="button"
            className="grip"
            data-act="grip"
            title="Drag to reorder. From the keyboard: Space lifts the card, the arrow keys move it, Space drops it, Escape cancels."
            aria-label={`Move ${label}, step ${index + 1} of ${count}`}
            aria-pressed={mode === "keyboard"}
            aria-disabled={!movable}
            onPointerDown={gripDown}
            onKeyDown={(e) => reorder.onGripKeyDown(e, id)}
            onBlur={() => reorder.onGripBlur(id)}
          >
            <Icon name="grip" />
          </button>
          <h3 className={`card-title${folded ? " folded" : ""}`}>
            <button aria-expanded={!folded} title={folded ? "Expand" : "Collapse"} onClick={() => act().toggleCollapsed(id)}>
              <Icon name="chevron" />
              {label}
            </button>
          </h3>
          {ext && ext.state !== "error" && (
            <span className="ext-badge" title={`${ext.name} ${ext.version}${ext.author ? ` by ${ext.author}` : ""}`}>
              Extension {ext.version}
            </span>
          )}
          {(!filter.enabled || extNote) && <span className="tag">Bypassed</span>}
          <span className="fill" onPointerDown={gripDown} />
          <button className="ghost icon" data-act="up" disabled={index === 0} onClick={() => moveWithFocus(index, index - 1)} title="Apply earlier" aria-label="Move up">
            <Icon name="up" />
          </button>
          <button className="ghost icon" data-act="down" disabled={index === count - 1} onClick={() => moveWithFocus(index, index + 1)} title="Apply later" aria-label="Move down">
            <Icon name="down" />
          </button>
          <button
            role="switch"
            aria-checked={filter.enabled}
            aria-label={`${label} enabled`}
            className="switch"
            title={filter.enabled ? "Bypass" : "Enable"}
            onClick={() => act().toggleFilter(id)}
          />
          <button className="ghost icon" onClick={() => act().removeFilter(id)} title="Remove" aria-label="Remove">
            <Icon name="close" />
          </button>
        </div>
        {needs ? (
          <p className="filter-error" role="alert">
            {needs.text}
            {needs.install && (
              <button className="link" onClick={() => act().openNvidiaPrompt()}>
                Install it now
              </button>
            )}
          </p>
        ) : (
          <>
            {extNote && (
              <p className={extNote.failed ? "filter-error" : "filter-status ext-note"} role={extNote.failed ? "alert" : "status"} data-ext-note={extNote.state}>
                {extNote.text}
              </p>
            )}
            {error && (
              <p className="filter-error" role="alert" title={error}>
                {stripCodes(error)}
              </p>
            )}
            {status && !error && (
              <p className="filter-status" role="status">
                <span className="spinner" aria-hidden />
                {status}
              </p>
            )}
          </>
        )}
        {def && def.params.length > 0 && !folded && (
          <div className={`params${def.layout ? ` layout-${def.layout}` : ""}`}>
            {def.params.map((p) => (
              <ParamField key={p.key} def={p} params={filter.params} onChange={onChange} />
            ))}
          </div>
        )}
      </div>
    </li>
  );
});
