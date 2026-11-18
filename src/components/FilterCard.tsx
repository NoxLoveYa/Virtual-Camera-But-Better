import type { PointerEvent as ReactPointerEvent } from "react";
import { useStore } from "../store";
import { stripCodes } from "../camera/schema";
import { FILTERS, type FilterCfg } from "../filters/schema";
import { Icon } from "./Icon";
import { ParamField } from "./ParamField";
import { moveWithFocus, type Reorder } from "../ui/useChainDrag";

interface Props {
  filter: FilterCfg;
  index: number;
  count: number;
  reorder: Reorder;
}

export function FilterCard({ filter, index, count, reorder }: Props) {
  const { toggleFilter, removeFilter, setParam, collapsed, toggleCollapsed } = useStore();
  const def = FILTERS[filter.kind];
  const folded = !!collapsed[filter.id];
  const error = useStore((s) => s.filterErrors[filter.id]);
  const status = useStore((s) => s.filterStatus[filter.id]);
  const gpu = useStore((s) => s.gpu);
  const gpuRead = useStore((s) => s.gpuRead);
  const nvidia = useStore((s) => s.nvidia);
  const openNvidiaPrompt = useStore((s) => s.openNvidiaPrompt);
  // What an NVIDIA filter needs before it can run, in the order of precedence: an RTX GPU, then the runtime. The app knows
  // both without the engine, so a card in a loaded preset says it before the first frame (the engine says the same
  // thing, and the picture is passed through meanwhile).
  const needs =
    !def.rtx || !gpuRead
      ? null
      : !gpu
        ? { text: "Needs an NVIDIA RTX GPU (GeForce RTX 20 series or newer).", install: false }
        : nvidia && !nvidia.available
          ? { text: nvidia.detail, install: true }
          : null;

  const movable = count > 1;
  const mode = reorder.drag?.id === filter.id ? reorder.drag.mode : null;
  const gripDown = (e: ReactPointerEvent<HTMLElement>) => reorder.onPointerDown(e, filter.id);

  return (
    <li
      className={`step${filter.enabled ? " on" : ""}${movable ? " movable" : ""}${mode === "pointer" ? " dragging" : ""}${mode === "keyboard" ? " lifted" : ""}`}
      data-filter-id={filter.id}
      onKeyDown={(e) => reorder.onCardKeyDown(e, filter.id)}
    >
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
            aria-label={`Move ${def.label}, step ${index + 1} of ${count}`}
            aria-pressed={mode === "keyboard"}
            aria-disabled={!movable}
            onPointerDown={gripDown}
            onKeyDown={(e) => reorder.onGripKeyDown(e, filter.id)}
            onBlur={() => reorder.onGripBlur(filter.id)}
          >
            <Icon name="grip" />
          </button>
          <h3 className={`card-title${folded ? " folded" : ""}`}>
            <button aria-expanded={!folded} title={folded ? "Expand" : "Collapse"} onClick={() => toggleCollapsed(filter.id)}>
              <Icon name="chevron" />
              {def.label}
            </button>
          </h3>
          {!filter.enabled && <span className="tag">Bypassed</span>}
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
            aria-label={`${def.label} enabled`}
            className="switch"
            title={filter.enabled ? "Bypass" : "Enable"}
            onClick={() => toggleFilter(filter.id)}
          />
          <button className="ghost icon" onClick={() => removeFilter(filter.id)} title="Remove" aria-label="Remove">
            <Icon name="close" />
          </button>
        </div>
        {needs ? (
          <p className="filter-error" role="alert">
            {needs.text}
            {needs.install && (
              <button className="link" onClick={openNvidiaPrompt}>
                Install it now
              </button>
            )}
          </p>
        ) : (
          <>
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
        {def.params.length > 0 && !folded && (
          <div className={`params${def.layout ? ` layout-${def.layout}` : ""}`}>
            {def.params.map((p) => (
              <ParamField key={p.key} def={p} params={filter.params} onChange={(key, value) => setParam(filter.id, key, value)} />
            ))}
          </div>
        )}
      </div>
    </li>
  );
}
