import { useEffect, useRef } from "react";
import { useStore } from "./store";
import { finalIndex } from "./ui/chainDrag";
import { useChainDrag } from "./ui/useChainDrag";
import { AddFilter } from "./components/AddFilter";
import { ControlsCard } from "./components/ControlsCard";
import { EngineBanner } from "./components/EngineBanner";
import { FilterCard } from "./components/FilterCard";
import { Icon } from "./components/Icon";
import { NvidiaPrompt } from "./components/NvidiaPrompt";
import { OutputPanel } from "./components/OutputPanel";
import { PresetMenu } from "./components/PresetMenu";
import { Preview } from "./components/Preview";
import { SourcePanel } from "./components/SourcePanel";
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
  const { setAllCollapsed } = useStore.getState();
  const reorder = useChainDrag();
  const init = useRef(false);

  useEffect(() => {
    if (init.current) return;
    init.current = true;
    useStore.getState().init();
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
      {/* Two columns that scroll on their own; below 1100 px they stack (camera, output, then the chain) in one scroll. */}
      <div className="cols">
        <div className="scroll app col col-left">
          <Preview />
          <SourcePanel />
          <ControlsCard />
          <OutputPanel />
        </div>

        <div className="scroll app col col-right">
          {(error || waiting || vcamBanner) && (
            <div className="banners">
              <VcamBanner />
              <EngineBanner />
            </div>
          )}

          <div className="chain-block" ref={reorder.blockRef}>
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
          </div>
        </div>
      </div>
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
        <Stats />
      </footer>
      <NvidiaPrompt />
    </>
  );
}
