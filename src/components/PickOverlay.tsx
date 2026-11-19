import { useEffect, useState, type PointerEvent as ReactPointerEvent, type RefObject } from "react";
import { useStore } from "../store";
import { colorToHex, withRgb } from "../filters/values";
import { sampleAverage, toCanvasPixel } from "../ui/eyedropper";
import { lastFrame } from "../ui/lastFrame";

interface Props {
  boxRef: RefObject<HTMLDivElement | null>;
  /** The Chroma Key or Color Key card this pick is for. */
  filter: string;
}

/** The colour of the preview pixels under the pointer (5×5 average of the last frame), or `null` in the letterbox bars. */
function colorAt(e: { clientX: number; clientY: number }, box: HTMLElement): { rgb: [number, number, number]; x: number; y: number } | null {
  const frame = lastFrame.get();
  if (!frame) return null;
  const rect = box.getBoundingClientRect();
  const px = toCanvasPixel(e.clientX, e.clientY, rect, frame.header);
  if (!px) return null;
  return { rgb: sampleAverage(frame.buf, frame.header, px.x, px.y), x: e.clientX - rect.left, y: e.clientY - rect.top };
}

/**
 * The eyedropper over the preview (L2): a crosshair, a swatch with the colour under the pointer that follows it, and a click that sets
 * the card's key colour to that colour. Escape cancels without a change. The picture is the one entering the card, with the filters before it.
 */
export function PickOverlay({ boxRef, filter }: Props) {
  const [hover, setHover] = useState<{ rgb: [number, number, number]; x: number; y: number } | null>(null);
  const { applyPickedColor, endPreviewTool } = useStore.getState();

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      endPreviewTool();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [endPreviewTool]);

  const move = (e: ReactPointerEvent) => setHover(boxRef.current ? colorAt(e, boxRef.current) : null);
  const box = boxRef.current?.getBoundingClientRect();
  // The swatch sits below and right of the pointer, and flips to the other side near the box's edge.
  const flipX = hover && box ? hover.x > box.width - 130 : false;
  const flipY = hover && box ? hover.y > box.height - 50 : false;

  return (
    <div
      className="pick-overlay"
      data-tool="pick"
      onPointerMove={move}
      onPointerLeave={() => setHover(null)}
      onClick={(e) => {
        const at = boxRef.current ? colorAt(e, boxRef.current) : null;
        if (at) applyPickedColor(filter, at.rgb);
      }}
    >
      {hover && (
        <span className="pick-swatch" style={{ left: hover.x + (flipX ? -122 : 14), top: hover.y + (flipY ? -40 : 14) }}>
          <i style={{ background: colorToHex(withRgb(0, ...hover.rgb)) }} />
          {colorToHex(withRgb(0, ...hover.rgb))}
        </span>
      )}
      <span className="tool-hint">Click the backdrop to pick its colour · Esc cancels</span>
    </div>
  );
}
