import { useEffect, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useStore, type EngineInfo } from "../store";
import { Icon as Glyph } from "./Icon";
import { Popover } from "./Popover";

const win = getCurrentWindow();

const Icon = ({ d }: { d: string }) => (
  <svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" strokeWidth="1">
    <path d={d} />
  </svg>
);

function Setting({ title, hint, on, onToggle }: { title: string; hint: string; on: boolean; onToggle: () => void }) {
  return (
    <div className="row" onClick={onToggle}>
      <span>
        <b>{title}</b>
        <small>{hint}</small>
      </span>
      <button role="switch" aria-checked={on} aria-label={title} className="switch" />
    </div>
  );
}

function Settings() {
  const { autostart, closeToTray, setAutostart, setCloseToTray } = useStore();
  const [open, setOpen] = useState(false);

  return (
    <Popover
      open={open}
      onClose={() => setOpen(false)}
      align="right"
      className="w320"
      anchor={
        <button className={open ? "toggled" : ""} aria-haspopup="menu" aria-expanded={open} onClick={() => setOpen(!open)}>
          Settings
        </button>
      }
    >
      <Setting
        title="Launch on Windows startup"
        hint="Starts in the tray and turns the camera on if it was running."
        on={autostart}
        onToggle={() => setAutostart(!autostart)}
      />
      <Setting
        title="Minimize to tray when closed"
        hint="Closing the window keeps the camera running. Quit from the tray icon."
        on={closeToTray}
        onToggle={() => setCloseToTray(!closeToTray)}
      />
    </Popover>
  );
}

/** What the title bar says about the engine. Pause counts while the camera is on; a reopen after a setting change is still live. */
function statusOf(state: EngineInfo["state"], reason: string | null, paused: boolean) {
  const live = state === "live" || (state === "standby" && reason === "reopening");
  if (live && paused) return { text: "Paused", dot: "warn", tone: "warn-text" };
  if (live) return { text: "Live", dot: "live", tone: "live-text" };
  if (state === "reconnecting") return { text: "Reconnecting", dot: "warn", tone: "warn-text" };
  if (state === "standby") return { text: "Standby", dot: "standby", tone: "" };
  return { text: "Idle", dot: "", tone: "" };
}

export function TitleBar() {
  const state = useStore((s) => s.engine.state);
  const reason = useStore((s) => s.engine.reason);
  const paused = useStore((s) => s.paused);
  const toggleRun = useStore((s) => s.toggleRun);
  const togglePause = useStore((s) => s.togglePause);
  const [maximized, setMaximized] = useState(false);
  const running = state !== "stopped";
  const status = statusOf(state, reason, paused);

  useEffect(() => {
    const sync = () => win.isMaximized().then(setMaximized);
    sync();
    const off = win.onResized(sync);
    return () => {
      off.then((f) => f());
    };
  }, []);

  return (
    <div className="titlebar" data-tauri-drag-region>
      <div className="brand">
        <span className={`dot ${status.dot}`} />
        <span className="name">Better Cam</span>
        <span className={`status ${status.tone}`}>{status.text}</span>
      </div>
      <div className="actions">
        <button
          className={paused ? "active" : ""}
          disabled={!running}
          onClick={togglePause}
          aria-pressed={paused}
          title="Show the “Camera paused” slate to apps. The camera stays open."
        >
          <Glyph name="pause" size={12} />
          {paused ? "Paused" : "Pause"}
        </button>
        <button className={running ? "danger" : "primary"} onClick={toggleRun}>
          <Glyph name={running ? "stop" : "play"} size={12} />
          {running ? "Stop" : "Start"}
        </button>
        <Settings />
      </div>
      <div className="wc">
        <button aria-label="Minimize" onClick={() => win.minimize()}>
          <Icon d="M0 5h10" />
        </button>
        <button aria-label={maximized ? "Restore" : "Maximize"} onClick={() => win.toggleMaximize()}>
          <Icon d={maximized ? "M2.5 2.5v-2h7v7h-2M.5 2.5h7v7h-7z" : "M.5.5h9v9h-9z"} />
        </button>
        <button className="close" aria-label="Close" onClick={() => win.close()}>
          <Icon d="M0 0l10 10M10 0L0 10" />
        </button>
      </div>
    </div>
  );
}
