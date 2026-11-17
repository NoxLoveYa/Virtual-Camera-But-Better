/** A label with a switch at its end; clicking anywhere on the row flips it. */
export function ToggleRow({ label, on, onChange }: { label: string; on: boolean; onChange: (on: boolean) => void }) {
  return (
    <label className="toggle">
      <span>{label}</span>
      <button type="button" role="switch" aria-checked={on} className="switch" onClick={() => onChange(!on)} />
    </label>
  );
}
