import { useStore } from "../store";
import { FILTERS, type FilterCfg } from "../filters/schema";
import { Icon } from "./Icon";
import { ParamField } from "./ParamField";

interface Props {
  filter: FilterCfg;
  index: number;
  count: number;
  onDragStart: () => void;
  onDrop: () => void;
}

export function FilterCard({ filter, index, count, onDragStart, onDrop }: Props) {
  const { toggleFilter, removeFilter, moveFilter, setParam, collapsed, toggleCollapsed } = useStore();
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

  return (
    <li className={`step${filter.enabled ? " on" : ""}`} onDragOver={(e) => e.preventDefault()} onDrop={onDrop}>
      <span className="num" title={`Step ${index + 1} of ${count}`}>
        {String(index + 1).padStart(2, "0")}
      </span>
      <div className="card">
        <div className="card-head">
          <span className="grip" title="Drag to reorder" draggable onDragStart={onDragStart}>
            <Icon name="grip" />
          </span>
          <h3 className={`card-title${folded ? " folded" : ""}`}>
            <button aria-expanded={!folded} title={folded ? "Expand" : "Collapse"} onClick={() => toggleCollapsed(filter.id)}>
              <Icon name="chevron" />
              {def.label}
            </button>
          </h3>
          {!filter.enabled && <span className="tag">Bypassed</span>}
          <span className="fill" />
          <button className="ghost icon" disabled={index === 0} onClick={() => moveFilter(index, index - 1)} title="Apply earlier" aria-label="Move up">
            <Icon name="up" />
          </button>
          <button className="ghost icon" disabled={index === count - 1} onClick={() => moveFilter(index, index + 1)} title="Apply later" aria-label="Move down">
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
              <p className="filter-error" role="alert">
                {error}
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
