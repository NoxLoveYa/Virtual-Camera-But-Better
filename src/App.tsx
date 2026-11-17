import { useEffect, useRef, useState } from "react";
import { useStore } from "./store";
import { AddFilter } from "./components/AddFilter";
import { ControlsCard } from "./components/ControlsCard";
import { FilterCard } from "./components/FilterCard";
import { OutputPanel } from "./components/OutputPanel";
import { PresetMenu } from "./components/PresetMenu";
import { Preview } from "./components/Preview";
import { SourcePanel } from "./components/SourcePanel";
import { Stats } from "./components/Stats";
import { TitleBar } from "./components/TitleBar";
import { Toast } from "./components/Toast";

export default function App() {
  const chain = useStore((s) => s.chain);
  const collapsed = useStore((s) => s.collapsed);
  const error = useStore((s) => s.error);
  const state = useStore((s) => s.engine.state);
  const reason = useStore((s) => s.engine.reason);
  const { setAllCollapsed, moveFilter, dismissError } = useStore.getState();
  const [dragFrom, setDragFrom] = useState<number | null>(null);
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
          {(error || waiting) && (
            <div className="banners">
              {error && (
                <div className="banner error" onClick={dismissError}>
                  {error}
                </div>
              )}
              {waiting && (
                <div className="banner warn" role="status">
                  <span>
                    {reason ? reason.charAt(0).toUpperCase() + reason.slice(1) : "Waiting for the camera"}. Better Cam keeps
                    trying and goes live as soon as the camera is available.
                  </span>
                </div>
              )}
            </div>
          )}

          <div className="chain-block">
            <span className="label chain-label">Filter chain · applied top to bottom</span>

            <ol className="chain">
              <li className="terminal">Camera in</li>
              {chain.map((f, i) => (
                <FilterCard
                  key={f.id}
                  filter={f}
                  index={i}
                  count={chain.length}
                  onDragStart={() => setDragFrom(i)}
                  onDrop={() => {
                    if (dragFrom !== null && dragFrom !== i) moveFilter(dragFrom, i);
                    setDragFrom(null);
                  }}
                />
              ))}
              <li className="terminal">Better Cam out</li>
            </ol>
          </div>
        </div>
      </div>
      <footer className="footer">
        <Toast />
        <Stats />
      </footer>
    </>
  );
}
