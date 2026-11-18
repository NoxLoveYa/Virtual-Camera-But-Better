import { useEffect, useRef, type CSSProperties } from "react";
import { useStore } from "./store";
import { finalIndex } from "./ui/chainDrag";
import { useChainDrag } from "./ui/useChainDrag";
import { useFileDrop } from "./ui/useFileDrop";
import { AddFilter } from "./components/AddFilter";
import { ControlsCard } from "./components/ControlsCard";
import { DropChooser } from "./components/DropChooser";
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
  const chain = useStore((s) => s.chain);
  const collapsed = useStore((s) => s.collapsed);
  const error = useStore((s) => s.error);
  const state = useStore((s) => s.engine.state);
  const vcamBanner = useStore((s) => s.vcam?.state === "missing" || s.vcam?.state === "outdated");
  const split = useStore((s) => s.ui.split);
  const { setAllCollapsed } = useStore.getState();
  const colsRef = useRef<HTMLDivElement>(null);
  const reorder = useChainDrag();
  const fileDrop = useFileDrop();
  const init = useRef(false);

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

  const allFolded = chain.length > 0 && chain.every((f) => collapsed[f.id]);
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
          {(error || waiting || vcamBanner) && (
            <div className="banners">
              <VcamBanner />
              <EngineBanner />
            </div>
          )}

          <div className="chain-block" ref={reorder.blockRef} data-drop="chain">
            <span className="label chain-label">Filter chain · applied top to bottom</span>

            <ol className="chain" ref={reorder.chainRef}>
              <li className="terminal">Camera in</li>
              {chain.map((f, i) => (
                <FilterCard key={f.id} filter={f} index={i} count={chain.length} reorder={reorder} />
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
