import { useEffect, useRef, type CSSProperties } from "react";
import { useShallow } from "zustand/react/shallow";
import { useStore } from "./store";
import { finalIndex } from "./ui/chainDrag";
import { useChainDrag } from "./ui/useChainDrag";
import { cancelInsertDrag, useInsertView } from "./ui/insertDragSession";
import { useFileDrop } from "./ui/useFileDrop";
import { AddFilter } from "./components/AddFilter";
import { ControlsCard } from "./components/ControlsCard";
import { DropChooser } from "./components/DropChooser";
import { EmptyChain } from "./components/EmptyChain";
import { EngineBanner } from "./components/EngineBanner";
import { FilterCard } from "./components/FilterCard";
import { Icon } from "./components/Icon";
import { NvidiaPrompt } from "./components/NvidiaPrompt";
import { OutputPanel } from "./components/OutputPanel";
import { PresetMenu } from "./components/PresetMenu";
import { Preview } from "./components/Preview";
import { SourcePanel } from "./components/SourcePanel";
import { Splitter } from "./components/Splitter";
import { Stats } from "./components/Stats";
import { TitleBar } from "./components/TitleBar";
import { Toast } from "./components/Toast";
import { VcamBanner } from "./components/VcamBanner";

export default function App() {
  // The ids and nothing else: a slider moving inside a card changes the chain but not this list, so the page does not
  // render again (each card reads its own filter).
  const ids = useStore(useShallow((s) => s.chain.map((f) => f.id)));
  const allFolded = useStore((s) => s.chain.length > 0 && s.chain.every((f) => s.collapsed[f.id]));
  const error = useStore((s) => s.error);
  const state = useStore((s) => s.engine.state);
  const vcamBanner = useStore((s) => s.vcam?.state === "missing" || s.vcam?.state === "outdated" || !!s.vcam?.problem);
  const split = useStore((s) => s.ui.split);
  const { setAllCollapsed } = useStore.getState();
  const colsRef = useRef<HTMLDivElement>(null);
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

  // Stacked, the preview band covers the top of the scroll. Its height is the scroll's top padding for scrollIntoView and
  // focus, so a control that is scrolled to lands below the band, not under it (`--band`, used by styles.css).
  useEffect(() => {
    const cols = colsRef.current;
    const pin = cols?.querySelector<HTMLElement>(".preview-pin");
    if (!cols || !pin) return;
    const ro = new ResizeObserver(() => cols.style.setProperty("--band", `${pin.offsetHeight}px`));
    ro.observe(pin);
    return () => ro.disconnect();
  }, []);

  const waiting = state === "reconnecting";

  return (
    <>
      <TitleBar />
      <div className="toolbar">
        <PresetMenu />
        <span className="spacer" />
        <button className="ghost" onClick={() => setAllCollapsed(!allFolded)}>
          {allFolded ? "Expand all" : "Collapse all"}
        </button>
        <AddFilter />
      </div>
      {/* Problems come first under the toolbar, across the whole window, and first in tab order after it. */}
      {(error || waiting || vcamBanner) && (
        <div className="banners">
          <VcamBanner />
          <EngineBanner />
        </div>
      )}
      {/* Two columns. The left one holds the pinned preview above its own scroll (Camera, controls, Output); the right one
          scrolls the chain; --split sets the left width. Below 1100 px the columns stack in one scroll under a sticky preview
          band (camera, output, then the chain) and the splitter is hidden. */}
      <div className="cols" ref={colsRef} style={{ "--split": split } as CSSProperties}>
        <div className="col col-left">
          <div className="preview-pin">
            <Preview />
          </div>
          <div className="scroll app left-scroll">
            <SourcePanel />
            <ControlsCard />
            <OutputPanel />
          </div>
        </div>
        <Splitter colsRef={colsRef} />

        <div className="scroll app col col-right">
          <div className="chain-block" ref={reorder.blockRef} data-drop="chain">
            <span className="label chain-label">Filter chain · applied top to bottom</span>

            <ol className="chain" ref={reorder.chainRef}>
              <li className="terminal">Camera in</li>
              {ids.length === 0 && <EmptyChain />}
              {ids.map((id) => (
                <FilterCard key={id} id={id} dragMode={reorder.drag?.id === id ? reorder.drag.mode : null} reorder={reorder.handlers} />
              ))}
              <li className="terminal">Better Cam out</li>
            </ol>
            {reorder.drag && (
              <div
                className={`drop-line${finalIndex(reorder.drag.from, reorder.drag.k) === reorder.drag.from ? " noop" : ""}`}
                style={{ top: reorder.lineTop }}
                aria-hidden
              />
            )}
            {fileDrop.lineTop !== null && <div className="drop-line file-line" style={{ top: fileDrop.lineTop }} aria-hidden />}
            {insert?.lineTop != null && <div className="drop-line insert-line" style={{ top: insert.lineTop }} aria-hidden />}
          </div>
        </div>
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
