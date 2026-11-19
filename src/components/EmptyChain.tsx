import { useStore } from "../store";
import { FILTERS, type BuiltinKind } from "../filters/schema";
import { focusNavRow } from "../ui/navFocus";

/** The quick adds of an empty chain; the NVIDIA one only on a machine with an RTX GPU. */
const QUICK: BuiltinKind[] = ["color_filter", "auto_adjust", "crop_filter"];
const RTX_QUICK: BuiltinKind = "nv_background_blur_filter";

/**
 * What a chain with no filter says, in the navigator between the FILTERS heading and the Add filter row: a line, a few
 * filters to start with and where a file can go. It is the drop target of gap 0.
 */
export function EmptyChain({ compact = false }: { compact?: boolean }) {
  const gpu = useStore((s) => s.gpu);
  const addFilter = useStore((s) => s.addFilter);
  const kinds = gpu ? [...QUICK, RTX_QUICK] : QUICK;

  // The 52 px rail has no room for words: an empty slot that still takes a file (the "+" under it adds a filter).
  if (compact) return <li className="empty" aria-hidden />;

  return (
    <li className="empty">
      <p className="empty-line">No filters yet.</p>
      <div className="empty-add">
        {kinds.map((kind) => (
          <button key={kind} type="button" className="ghost" onClick={() => focusNavRow(addFilter(kind))}>
            {FILTERS[kind].label}
          </button>
        ))}
      </div>
      <p className="empty-drop">Or drop a .cube LUT or a picture here.</p>
    </li>
  );
}
