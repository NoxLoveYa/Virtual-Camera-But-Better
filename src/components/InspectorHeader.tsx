import type { ReactNode } from "react";

interface Props {
  /** Names the heading (and its tooltip, for a title that was cut with an ellipsis). */
  title: string;
  /** Before the title: a filter's grip. */
  lead?: ReactNode;
  /** After the title: the extension and bypass tags. */
  badges?: ReactNode;
  /** Under the title: "Step 3 of 4" or a section's summary. */
  sub?: ReactNode;
  /** The end of the title line: the On switch and ⋯, or a section's one action. */
  end?: ReactNode;
  /** The row of buttons under the title: the ones on the left, then the ones on the right. */
  actions?: { start?: ReactNode; end?: ReactNode };
  /** Full-width status lines under everything else (needs, notes, errors). */
  children?: ReactNode;
}

/**
 * The top of the inspector (docs/LAYOUT-INSPECTOR.md 1.7): who is shown, what can be done with it, and what it says about
 * itself. It sits above the scrolling body, so it stays where it is while a tall item scrolls.
 */
export function InspectorHeader({ title, lead, badges, sub, end, actions, children }: Props) {
  return (
    <header className="insp-head">
      <div className="insp-top">
        {lead}
        <div className="insp-id">
          <div className="insp-line">
            <h2 className="insp-title" title={title}>
              {title}
            </h2>
            {badges}
          </div>
          {sub && <div className="insp-sub">{sub}</div>}
        </div>
        {end && <div className="insp-end">{end}</div>}
      </div>
      {actions && (actions.start || actions.end) && (
        <div className="insp-actions">
          <div className="insp-actions-start">{actions.start}</div>
          {actions.end}
        </div>
      )}
      {children}
    </header>
  );
}
