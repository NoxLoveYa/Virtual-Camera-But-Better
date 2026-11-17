import { useState } from "react";
import { Select } from "./Select";

interface Props {
  value: string;
  options: string[];
  onChange: (value: string) => void;
  /** Typed values must match this as a whole; anything else is shown as invalid and not applied. */
  pattern?: RegExp;
  placeholder?: string;
  /** Names the controls for screen readers. */
  label: string;
}

const CUSTOM = "\u0000custom";

/** A list of choices with a final "Custom…" entry that reveals a text box for a value of your own. */
export function ComboField({ value, options, onChange, pattern, placeholder, label }: Props) {
  const listed = options.includes(value);
  const [custom, setCustom] = useState(!listed);
  const showText = custom || !listed;
  const [text, setText] = useState(listed ? "" : value);
  // The stored value changed from outside (a preset): the text box follows it.
  const [seen, setSeen] = useState(value);
  if (seen !== value) {
    setSeen(value);
    if (!listed) setText(value);
  }

  const bad = showText && text.trim() !== "" && pattern !== undefined && !pattern.test(text.trim());

  return (
    <div className="combo">
      <Select
        value={showText ? CUSTOM : value}
        options={[...options.map((o) => ({ value: o, label: o })), { value: CUSTOM, label: "Custom…" }]}
        onChange={(v) => {
          if (v === CUSTOM) {
            setText(listed ? "" : value);
            setCustom(true);
          } else {
            setCustom(false);
            onChange(v);
          }
        }}
      />
      {showText && (
        <input
          type="text"
          className={bad ? "bad" : undefined}
          aria-label={`${label}, custom value`}
          aria-invalid={bad}
          placeholder={placeholder}
          autoComplete="off"
          spellCheck={false}
          value={text}
          onChange={(e) => {
            const t = e.target.value.trim();
            setText(e.target.value);
            if (t !== "" && (!pattern || pattern.test(t)) && t !== value) onChange(t);
          }}
        />
      )}
    </div>
  );
}
