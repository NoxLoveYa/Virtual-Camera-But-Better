import { useState } from "react";
import { useStore } from "../store";
import { FILTERS, FILTER_HINTS, type BuiltinKind } from "../filters/schema";
import { Icon } from "./Icon";
import { Popover } from "./Popover";

export function AddFilter() {
  const addFilter = useStore((s) => s.addFilter);
  // The NVIDIA filters can still be added without an RTX GPU (a preset may carry them); the menu says what they need.
  const noRtx = useStore((s) => s.gpuRead && !s.gpu);
  const extensions = useStore((s) => s.extensions);
  const [open, setOpen] = useState(false);
  // Only an extension that is ready can be added; overlays and widgets have their own place (a later version).
  const ready = extensions.filter((e) => e.kind === "filter" && e.state === "ready");

  return (
    <Popover
      open={open}
      onClose={() => setOpen(false)}
      align="right"
      className="add-menu"
      anchor={
        <button data-add-filter aria-haspopup="menu" aria-expanded={open} onClick={() => setOpen(!open)}>
          <Icon name="plus" /> Add filter
        </button>
      }
    >
      {(Object.keys(FILTERS) as BuiltinKind[]).map((kind) => {
        const rtxOnly = noRtx && FILTERS[kind].rtx === true;
        return (
          <button
            key={kind}
            className="pm-item two"
            onClick={() => {
              addFilter(kind);
              setOpen(false);
            }}
          >
            <b>
              {FILTERS[kind].label}
              {rtxOnly && <span className="rtx-badge">RTX only</span>}
            </b>
            <small>{rtxOnly ? FILTER_HINTS[kind].replace(/ \(RTX only\)$/, "") : FILTER_HINTS[kind]}</small>
          </button>
        );
      })}
      {ready.length > 0 && (
        <div className="pm-group" role="presentation">
          Extension filters
        </div>
      )}
      {ready.map((ext) => (
        <button
          key={ext.id}
          className="pm-item two"
          data-ext={ext.id}
          onClick={() => {
            addFilter(`ext:${ext.id}`);
            setOpen(false);
          }}
        >
          <b>
            {ext.name}
            <span className="ext-badge">Extension</span>
          </b>
          {ext.hint && <small>{ext.hint}</small>}
        </button>
      ))}
    </Popover>
  );
}
