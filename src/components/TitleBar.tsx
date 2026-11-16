import { useEffect, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useStore } from "../store";
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
        hint="Starts in the tray."
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

export function TitleBar() {
  const [maximized, setMaximized] = useState(false);

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
        <span className="dot" />
        <span className="name">Better Cam</span>
        <span className="status">Idle</span>
      </div>
      <div className="actions">
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
