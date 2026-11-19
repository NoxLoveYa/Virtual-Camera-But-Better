/**
 * Where the keyboard focus goes when the chain changes under it: to the navigator row of the filter that was just added,
 * or to the row that took the selection after a removal. After the next frame, once the row exists.
 */
import { useStore } from "../store";
import { selectedFilterId } from "./layout";

const MAIN = ".nav-main";

/** Focuses the navigator row of filter `id` (its main button) and keeps it in view. Not while the NVIDIA dialog is open. */
export function focusNavRow(id: string) {
  if (useStore.getState().nvidiaPrompt) return;
  requestAnimationFrame(() => {
    const row = document.querySelector<HTMLElement>(`li.nav-step[data-filter-id="${CSS.escape(id)}"]`);
    row?.querySelector<HTMLElement>(MAIN)?.focus({ preventScroll: true });
    row?.scrollIntoView({ block: "nearest" });
  });
}

const CONTROLS = ["button", "input", "select", "textarea", "a[href]", '[tabindex]:not([tabindex="-1"])'].map((s) => `.inspector ${s}`).join(", ");

/** Focuses the first control of the inspector (the header's grip, or the section's action). `false` when it has none. */
export function focusInspector(): boolean {
  const first = Array.from(document.querySelectorAll<HTMLElement>(CONTROLS)).find(
    (el) => !(el as HTMLButtonElement).disabled && el.getClientRects().length > 0 && el.tabIndex >= 0,
  );
  first?.focus();
  return !!first;
}

/** Focuses whichever navigator row is selected now (after the selected filter was removed, that is the one that took its place). */
export function focusSelectedRow() {
  requestAnimationFrame(() => {
    const { selected } = useStore.getState().ui;
    const id = selectedFilterId(selected);
    const target =
      id !== null
        ? document.querySelector<HTMLElement>(`li.nav-step[data-filter-id="${CSS.escape(id)}"] ${MAIN}`)
        : document.querySelector<HTMLElement>(`[data-nav="${CSS.escape(selected)}"]`);
    target?.focus({ preventScroll: true });
    target?.scrollIntoView({ block: "nearest" });
  });
}
