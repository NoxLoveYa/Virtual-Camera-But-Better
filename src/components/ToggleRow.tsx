/** A property row with a switch at the end of its control column; clicking anywhere on the row flips it. The label wraps beside the switch. */
export function ToggleRow({ label, on, onChange }: { label: string; on: boolean; onChange: (on: boolean) => void }) {
  return (
    <label className="prop bool">
      <span className="pf-label">{label}</span>
      <button type="button" role="switch" aria-checked={on} className="switch" onClick={() => onChange(!on)} />
    </label>
  );
}
