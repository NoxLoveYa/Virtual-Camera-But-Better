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
        {error && (
          <p className="filter-error" role="alert">
            {error}
          </p>
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
