import { useStore } from "../store";
import { FILTERS, type BuiltinKind } from "../filters/schema";

/** The quick adds of an empty chain; the NVIDIA one only on a machine with an RTX GPU. */
const QUICK: BuiltinKind[] = ["color_filter", "auto_adjust", "crop_filter"];
const RTX_QUICK: BuiltinKind = "nv_background_blur_filter";

/** Opens the toolbar's Add filter menu (its anchor carries `data-add-filter`). */
const openAddMenu = () => {
  const button = document.querySelector<HTMLButtonElement>("[data-add-filter]");
  button?.focus();
  button?.click();
};

/** What a chain with no filter says: what filters are, a few to start with, and where the rest are. */
export function EmptyChain() {
  const gpu = useStore((s) => s.gpu);
  const addFilter = useStore((s) => s.addFilter);
  const kinds = gpu ? [...QUICK, RTX_QUICK] : QUICK;

  return (
    <li className="empty">
      <div className="empty-body">
        <p>No filters yet. Filters change the picture before other apps see it.</p>
        <div className="empty-add">
          {kinds.map((kind) => (
            <button key={kind} onClick={() => addFilter(kind)}>
              {FILTERS[kind].label}
            </button>
          ))}
          <button aria-haspopup="menu" onClick={openAddMenu}>
            More…
          </button>
        </div>
        <p className="empty-drop">Or drop a .cube LUT or a picture here.</p>
      </div>
    </li>
  );
}
