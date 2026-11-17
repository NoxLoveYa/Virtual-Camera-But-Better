import { useState } from "react";
import { useStore } from "../store";
import { FILTERS, FILTER_HINTS, type FilterKind } from "../filters/schema";
import { Icon } from "./Icon";
import { Popover } from "./Popover";

export function AddFilter() {
  const addFilter = useStore((s) => s.addFilter);
  // The NVIDIA filters can still be added without an RTX GPU (a preset may carry them); the menu says what they need.
  const noRtx = useStore((s) => s.gpuRead && !s.gpu);
  const [open, setOpen] = useState(false);

  return (
    <Popover
      open={open}
      onClose={() => setOpen(false)}
      align="right"
      className="add-menu"
      anchor={
        <button aria-haspopup="menu" aria-expanded={open} onClick={() => setOpen(!open)}>
          <Icon name="plus" /> Add filter
        </button>
      }
    >
      {(Object.keys(FILTERS) as FilterKind[]).map((kind) => {
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
    </Popover>
  );
}
