import type { ReactNode } from "react";

/** A small caption above a control, as in Better Mic's device panel. The wrapping <label> names the control. */
export function Field({ label, className = "", children }: { label: string; className?: string; children: ReactNode }) {
  return (
    <label className={`field ${className}`}>
      <span className="label">{label}</span>
      {children}
    </label>
  );
}
