import { useEffect, useRef, useState } from "react";
import { useStore } from "./store";
import { AddFilter } from "./components/AddFilter";
import { FilterCard } from "./components/FilterCard";
import { PresetMenu } from "./components/PresetMenu";
import { TitleBar } from "./components/TitleBar";
import { Toast } from "./components/Toast";

export default function App() {
  const s = useStore();
  const [dragFrom, setDragFrom] = useState<number | null>(null);
  const init = useRef(false);

  useEffect(() => {
    if (init.current) return;
    init.current = true;
    useStore.getState().init();
  }, []);

  const allFolded = s.chain.length > 0 && s.chain.every((f) => s.collapsed[f.id]);

  return (
    <>
      <TitleBar />
      <div className="toolbar">
        <PresetMenu />
        <span className="spacer" />
        <button className="ghost" onClick={() => s.setAllCollapsed(!allFolded)}>
          {allFolded ? "Expand all" : "Collapse all"}
        </button>
        <AddFilter />
      </div>
      <div className="scroll">
        <div className="app">
          {s.error && (
            <div className="banner error" onClick={s.dismissError}>
              {s.error}
            </div>
          )}

          <span className="label chain-label">Filter chain · applied top to bottom</span>

          <ol className="chain">
            <li className="terminal">Camera in</li>
            {s.chain.map((f, i) => (
              <FilterCard
                key={f.id}
                filter={f}
                index={i}
                count={s.chain.length}
                onDragStart={() => setDragFrom(i)}
                onDrop={() => {
                  if (dragFrom !== null && dragFrom !== i) s.moveFilter(dragFrom, i);
                  setDragFrom(null);
                }}
              />
            ))}
            <li className="terminal">Better Cam out</li>
          </ol>
        </div>
      </div>
      <footer className="footer">
        <Toast />
      </footer>
    </>
  );
}
