import { memo, useCallback, useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { useStore } from "../store";
import { stripCodes } from "../camera/schema";
import { defOf, extIdOf, extOf, isExtKind, labelOf, type ParamValue } from "../filters/schema";
import { extNoteOf, needsOf } from "../ui/navSummary";
import { Icon } from "./Icon";
import { InspectorHeader } from "./InspectorHeader";
import { ParamField } from "./ParamField";
import { Popover } from "./Popover";
import { focusSelectedRow } from "../ui/navFocus";
import { toolBlock, toolOf } from "../ui/previewTool";
import { NOT_HELD, holdStep, isHeld, type Hold, type HoldEvent } from "../ui/previewFrame";
import { moveWithFocus, type ChainHandlers, type DragView } from "../ui/useChainDrag";

/**
 * The inspector for one filter: the header (grip, name, step, On, ⋯, Before, the picture tool, Reset, status lines) and its
 * parameters as property rows. The props are the filter's id, how it is being dragged (`null` most of the time) and the
 * handlers (one object for the life of the chain). It reads its own filter, its place and the rest from the store, so a
 * change of another filter's params does not render it. Its root carries `data-inspects`, not `data-filter-id`: the
 * reorder code finds the navigator row by the latter.
 */
interface Props {
  id: string;
  dragMode: DragView["mode"] | null;
  reorder: ChainHandlers;
}

// The actions never change: read them when used, so the inspector does not subscribe to them.
const act = () => useStore.getState();

const pointsAt = (id: string) => {
  const p = act().previewPoint;
  return p !== null && typeof p === "object" && "filter" in p && p.filter === id;
};

/**
 * "Before": while it is held (pointer or Space) the preview shows the picture entering this filter; let go and it is the
 * output again, unless another gesture took the picture meanwhile. Any loss of the pointer, the focus or the window lets go.
 */
function BeforeButton({ id, blocked }: { id: string; blocked: string | null }) {
  const [hold, setHold] = useState<Hold>(NOT_HELD);
  const held = isHeld(hold);
  const shown = useRef(false);
  const off = blocked !== null;

  const step = useCallback((event: HoldEvent) => setHold((h) => holdStep(h, event)), []);

  useEffect(() => {
    if (held && !off) {
      shown.current = true;
      act().setPreviewPoint({ filter: id });
    } else if (shown.current) {
      shown.current = false;
      if (pointsAt(id)) act().setPreviewPoint(null);
    }
  }, [held, off, id]);
  // A button that went away under the pointer gets no pointerup.
  useEffect(() => {
    if (off) step("blur");
  }, [off, step]);
  useEffect(() => {
    const letGo = () => step("blur");
    window.addEventListener("blur", letGo);
    return () => {
      window.removeEventListener("blur", letGo);
      if (shown.current && pointsAt(id)) act().setPreviewPoint(null);
      shown.current = false;
    };
  }, [id, step]);

  return (
    <button
      type="button"
      className="insp-before"
      aria-pressed={held}
      disabled={off}
      title={blocked ?? "Hold to see the picture before this filter"}
      onPointerDown={(e) => {
        if (e.button !== 0) return;
        e.currentTarget.setPointerCapture(e.pointerId);
        step("pointerdown");
      }}
      onPointerUp={() => step("pointerup")}
      onPointerCancel={() => step("pointerup")}
      onLostPointerCapture={() => step("pointerup")}
      onKeyDown={(e) => {
        if (e.key !== " ") return;
        // The button's own Space click would fire on release: the hold is the whole gesture.
        e.preventDefault();
        if (!e.repeat) step("keydown");
      }}
      onKeyUp={(e) => {
        if (e.key !== " ") return;
        e.preventDefault();
        step("keyup");
      }}
      onBlur={() => step("blur")}
      onContextMenu={(e) => e.preventDefault()}
    >
      <Icon name="compare" />
      Before
    </button>
  );
}

/** The ⋯ of the header: Move up, Move down, Remove, as in the navigator's row menu. */
function FilterMenu({ id, index, count, label }: { id: string; index: number; count: number; label: string }) {
  const [open, setOpen] = useState(false);
  const anchor = useRef<HTMLButtonElement>(null);
  const move = (to: number) => () => {
    setOpen(false);
    moveWithFocus(index, to);
    anchor.current?.focus();
  };
  const remove = () => {
    setOpen(false);
    act().removeFilter(id);
    focusSelectedRow();
  };
  return (
    <Popover
      open={open}
      onClose={() => setOpen(false)}
      align="right"
      className="row-menu"
      flip
      anchor={
        <button ref={anchor} type="button" className="ghost icon" aria-haspopup="menu" aria-expanded={open} aria-label={`More for ${label}`} title="More" onClick={() => setOpen(!open)}>
          <Icon name="more" />
        </button>
      }
    >
      <button type="button" role="menuitem" className="pm-item" disabled={index === 0} onClick={move(index - 1)}>
        <Icon name="up" /> Move up <kbd>Alt+↑</kbd>
      </button>
      <button type="button" role="menuitem" className="pm-item" disabled={index === count - 1} onClick={move(index + 1)}>
        <Icon name="down" /> Move down <kbd>Alt+↓</kbd>
      </button>
      <button type="button" role="menuitem" className="pm-item" onClick={remove}>
        <Icon name="close" /> Remove
      </button>
    </Popover>
  );
}

export const FilterInspector = memo(function FilterInspector({ id, dragMode: mode, reorder }: Props) {
  // An inspector that was just removed is still mounted until its parent renders again: it has nothing to show then.
  const filter = useStore((s) => s.chain.find((f) => f.id === id));
  const index = useStore((s) => s.chain.findIndex((f) => f.id === id));
  const count = useStore((s) => s.chain.length);
  const extensions = useStore((s) => s.extensions);
  const extensionsRead = useStore((s) => s.extensionsRead);
  const error = useStore((s) => s.filterErrors[id]);
  const status = useStore((s) => s.filterStatus[id]);
  const gpu = useStore((s) => s.gpu);
  const gpuRead = useStore((s) => s.gpuRead);
  const nvidia = useStore((s) => s.nvidia);
  // The preview tool running for this filter (eyedropper or crop frame), and why none can start now (a tooltip), if so.
  const tool = useStore((s) => (s.previewTool?.filter === id ? s.previewTool.kind : null));
  const anyTool = useStore((s) => s.previewTool !== null);
  const toolBlocked = useStore((s) => toolBlock({ previewOn: s.previewOn, state: s.engine.state, paused: s.paused }));
  // One function for the life of the inspector, so the fields that did not change do not render.
  const onChange = useCallback((key: string, value: ParamValue) => act().setParam(id, key, value), [id]);
  if (!filter) return null;
  const toolKind = toolOf(filter.kind);
  // `def` is missing for an extension that is not installed or failed to load: the filter keeps its settings and says why.
  const def = defOf(filter.kind, extensions);
  const label = labelOf(filter.kind, extensions);
  const ext = extOf(filter.kind, extensions);
  const extNote = !isExtKind(filter.kind) || !extensionsRead ? null : extNoteOf(extIdOf(filter.kind), ext);
  // What an NVIDIA filter needs before it can run, in the order of precedence: an RTX GPU, then the runtime. The app knows
  // both without the engine, so a filter in a loaded preset says it before the first frame (the engine says the same
  // thing, and the picture is passed through meanwhile).
  const needs = needsOf(def?.rtx, gpuRead, gpu, nvidia);

  const movable = count > 1;
  const gripDown = (e: ReactPointerEvent<HTMLElement>) => reorder.onPointerDown(e, id);
  // The Before and tool buttons need the live picture; a running tool (of this filter or another) also blocks Before.
  const beforeBlocked = toolBlocked ?? (anyTool ? "Finish the pick or the crop first" : null);
  const atDefaults = !def || def.params.every((p) => (filter.params[p.key] ?? p.def) === p.def);
  const reset = () => {
    if (!def) return;
    const before = act().checkpoint();
    act().setParams(id, Object.fromEntries(def.params.map((p) => [p.key, p.def])));
    act().sayUndo(`Reset ${label}`, before);
  };

  return (
    <section
      className={`insp-panel${filter.enabled ? "" : " off"}${tool ? " tooling" : ""}`}
      data-inspects={id}
      aria-label={label}
      onKeyDown={(e) => reorder.onCardKeyDown(e, id)}
    >
      <InspectorHeader
        title={label}
        lead={
          <button
            type="button"
            className="grip"
            data-act="grip"
            title="Drag to reorder. From the keyboard: Space lifts the filter, the arrow keys move it, Space drops it, Escape cancels."
            aria-label={`Move ${label}, step ${index + 1} of ${count}`}
            aria-pressed={mode === "keyboard"}
            aria-disabled={!movable}
            onPointerDown={gripDown}
            onKeyDown={(e) => reorder.onGripKeyDown(e, id)}
            onBlur={() => reorder.onGripBlur(id)}
          >
            <Icon name="grip" />
          </button>
        }
        badges={
          <>
            {ext && ext.state !== "error" && (
              <span className="ext-badge" title={`${ext.name} ${ext.version}${ext.author ? ` by ${ext.author}` : ""}`}>
                Extension {ext.version}
              </span>
            )}
            {(!filter.enabled || extNote) && <span className="tag">Bypassed</span>}
          </>
        }
        sub={`Step ${index + 1} of ${count}`}
        end={
          <>
            <label className="insp-on">
              <span>On</span>
              <button
                type="button"
                role="switch"
                aria-checked={filter.enabled}
                aria-label={`${label} enabled`}
                className="switch"
                title={filter.enabled ? "Bypass" : "Enable"}
                onClick={() => act().toggleFilter(id)}
              />
            </label>
            <FilterMenu id={id} index={index} count={count} label={label} />
          </>
        }
        actions={{
          start: (
            <>
              <BeforeButton id={id} blocked={beforeBlocked} />
              {toolKind && !needs && (
                <button
                  type="button"
                  className="insp-tool"
                  aria-pressed={tool !== null}
                  disabled={tool === null && toolBlocked !== null}
                  title={
                    tool !== null
                      ? toolKind === "pick"
                        ? "Stop picking"
                        : "Finish editing on the picture"
                      : (toolBlocked ??
                        (toolKind === "pick"
                          ? "Click the backdrop in the preview to use its colour as the key colour"
                          : "Show the picture before this filter with a frame to drag: handles move the edges, inside pans, the wheel zooms"))
                  }
                  onClick={() => (tool !== null ? act().endPreviewTool() : act().startPreviewTool(toolKind, id))}
                >
                  <Icon name={toolKind === "pick" ? "eyedropper" : "crop"} />
                  {toolKind === "pick" ? (tool !== null ? "Cancel pick" : "Pick from preview") : tool !== null ? "Done" : "Edit on picture"}
                </button>
              )}
            </>
          ),
          end: (
            <button type="button" className="insp-reset" disabled={atDefaults} title={atDefaults ? "Every setting is at its default" : "Put every setting back to its default"} onClick={reset}>
              <Icon name="reset" />
              Reset
            </button>
          ),
        }}
      >
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
        {tool !== null && !needs && (
          <p className="insp-hint" role="status">
            {toolKind === "pick" ? "Click the backdrop in the preview. Esc cancels." : "Drag the frame in the preview. Esc finishes."}
          </p>
        )}
      </InspectorHeader>
      <div className="insp-scroll">
        {def && def.params.length > 0 && (
          <div className={`props${def.layout ? ` layout-${def.layout}` : ""}`}>
            {def.params.map((p) => (
              <ParamField key={p.key} def={p} params={filter.params} onChange={onChange} />
            ))}
          </div>
        )}
      </div>
    </section>
  );
});
