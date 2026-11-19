import { useEffect, useLayoutEffect, useRef, useState, type CSSProperties } from "react";
import { useStore } from "./store";
import { useChainDrag } from "./ui/useChainDrag";
import { cancelInsertDrag, useInsertView } from "./ui/insertDragSession";
import { effectiveWidths } from "./ui/layout";
import { useFileDrop } from "./ui/useFileDrop";
import { DropChooser } from "./components/DropChooser";
import { EngineBanner } from "./components/EngineBanner";
import { Icon } from "./components/Icon";
import { Inspector } from "./components/Inspector";
import { Navigator } from "./components/Navigator";
import { NvidiaPrompt } from "./components/NvidiaPrompt";
import { Preview } from "./components/Preview";
import { Splitter } from "./components/Splitter";
import { Stats } from "./components/Stats";
import { TitleBar } from "./components/TitleBar";
import { Toast } from "./components/Toast";
import { VcamBanner } from "./components/VcamBanner";

export default function App() {
  const error = useStore((s) => s.error);
  const state = useStore((s) => s.engine.state);
  const vcamBanner = useStore((s) => s.vcam?.state === "missing" || s.vcam?.state === "outdated" || !!s.vcam?.problem);
  const navWidth = useStore((s) => s.ui.navWidth);
  const inspectorWidth = useStore((s) => s.ui.inspectorWidth);
  const workspaceRef = useRef<HTMLDivElement>(null);
  const [total, setTotal] = useState(() => window.innerWidth);
  const reorder = useChainDrag();
  const fileDrop = useFileDrop();
  const insert = useInsertView((s) => s.view);
  const init = useRef(false);

  // Leaving the page in the middle of a drag out of the Add menu releases everything it holds.
  useEffect(() => cancelInsertDrag, []);

  useEffect(() => {
    if (init.current) return;
    init.current = true;
    useStore.getState().init();
  }, []);

  // The workspace's own width, read before the first paint and on every resize: the widths of the regions are worked out from it.
  useLayoutEffect(() => {
    const el = workspaceRef.current;
    if (!el) return;
    const read = () => setTotal(el.clientWidth);
    read();
    const ro = new ResizeObserver(read);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const waiting = state === "reconnecting";
  const widths = effectiveWidths(total, navWidth, inspectorWidth);

  return (
    <>
      <TitleBar />
      {/* Problems come first under the title bar, across the whole window, and first in tab order after it. */}
      {(error || waiting || vcamBanner) && (
        <div className="banners">
          <VcamBanner />
          <EngineBanner />
        </div>
      )}
      {/* Three docked regions: the navigator (sections and the filter chain), the preview (always in view) and the inspector (the
          one selected item). The widths are CSS variables, so a splitter drag does not render the app (see Splitter). */}
      <div
        className="workspace"
        ref={workspaceRef}
        data-nav-mode="full"
        style={{ "--nav": `${widths.nav}px`, "--insp": `${widths.inspector}px` } as CSSProperties}
      >
        <Navigator reorder={reorder} fileLineTop={fileDrop.lineTop} />
        <Splitter side="nav" total={total} workspaceRef={workspaceRef} />
        <main className="center" data-region="center">
          <Preview />
        </main>
        <Splitter side="inspector" total={total} workspaceRef={workspaceRef} />
        <Inspector drag={reorder.drag} reorder={reorder.handlers} />
      </div>
      {fileDrop.chooser && <DropChooser chooser={fileDrop.chooser} onChoose={fileDrop.choose} />}
      {reorder.lift && (
        <div className="reorder-lift" ref={reorder.liftRef} style={{ left: reorder.lift.left, width: Math.min(reorder.lift.width, 300) }} aria-hidden>
          <Icon name="grip" />
          <span className="lift-title">{reorder.lift.label}</span>
          {reorder.lift.bypassed && <span className="tag">Bypassed</span>}
        </div>
      )}
      {insert && (
        <div
          className="reorder-lift insert-lift"
          data-over={insert.lineTop !== null}
          style={{ left: Math.max(0, Math.min(insert.x + 14, window.innerWidth - 270)), top: insert.y }}
          aria-hidden
        >
          <Icon name="plus" />
          <span className="lift-title">{insert.label}</span>
        </div>
      )}
      <div className="reorder-live" role="status" aria-live="polite" aria-atomic="true">
        {reorder.announcement}
      </div>
      <footer className="footer">
        <Toast />
        {fileDrop.hint && <div className="drop-hint">{fileDrop.hint}</div>}
        <Stats />
      </footer>
      <NvidiaPrompt />
    </>
  );
}
