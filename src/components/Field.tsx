import type { ReactNode } from "react";

/** A property row of the inspector: the label at the left, the control after it. The wrapping <label> names the control. */
export function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <label className="prop">
      <span className="pf-label">{label}</span>
      {children}
    </label>
  );
}
