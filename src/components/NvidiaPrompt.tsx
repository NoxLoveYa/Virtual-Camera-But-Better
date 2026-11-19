import { useEffect } from "react";
import { useStore } from "../store";

const mb = (n: number) => `${Math.round(n / 1048576)} MB`;

const STATUS = {
  verify: "Verifying the download…",
  install: "Waiting for the NVIDIA installer. Finish it in its window.",
} as const;

export function NvidiaPrompt() {
  const nvidiaPrompt = useStore((s) => s.nvidiaPrompt);
  const gpu = useStore((s) => s.gpu);
  const available = useStore((s) => s.nvidia?.available === true);
  const nvidiaInstall = useStore((s) => s.nvidiaInstall);
  const nvidiaError = useStore((s) => s.nvidiaError);
  const { installNvidia, closeNvidiaPrompt } = useStore.getState();
  const busy = nvidiaInstall !== null;

  useEffect(() => {
    if (!nvidiaPrompt || busy) return;
    const esc = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault(); // used up here: Compare skips a prevented Escape
      closeNvidiaPrompt();
    };
    document.addEventListener("keydown", esc);
    return () => document.removeEventListener("keydown", esc);
  }, [nvidiaPrompt, busy, closeNvidiaPrompt]);

  // The runtime can show up while its installer is still open; the dialog stays until that one has closed.
  if (!nvidiaPrompt || !gpu || (available && !busy)) return null;

  const downloading = nvidiaInstall?.stage === "download";
  const pct = downloading && nvidiaInstall.total > 0 ? (nvidiaInstall.done / nvidiaInstall.total) * 100 : 0;

  return (
    <div className="backdrop" onMouseDown={() => !busy && closeNvidiaPrompt()}>
      <div className="modal" role="dialog" aria-modal="true" aria-labelledby="nv-title" onMouseDown={(e) => e.stopPropagation()}>
        <h2 id="nv-title">Install NVIDIA video effects</h2>
        <p>
          <b>{gpu.name}</b> detected. NVIDIA's background removal and background blur need its Video Effects runtime, which isn't
          installed yet (or is out of date). It's the same package OBS uses.
        </p>
        <ul>
          <li>Downloads about 725–750 MB from nvidia.com.</li>
          <li>
            Checks the download against a fingerprint recorded by Better Cam, because Windows can't check NVIDIA's own signature
            on this file, then asks for administrator rights.
          </li>
        </ul>

        {busy && (
          <div className="progress-block">
            <div className="progress" aria-hidden>
              <div className={downloading ? "" : "indeterminate"} style={downloading ? { width: `${pct}%` } : undefined} />
            </div>
            <small>
              {downloading
                ? `Downloading… ${mb(nvidiaInstall.done)}${nvidiaInstall.total ? ` of ${mb(nvidiaInstall.total)}` : ""}`
                : STATUS[nvidiaInstall.stage as keyof typeof STATUS]}
            </small>
          </div>
        )}
        {nvidiaError && <div className="modal-error">{nvidiaError}</div>}

        <div className="modal-actions">
          <button disabled={busy} onClick={closeNvidiaPrompt}>
            Not now
          </button>
          <button className="primary" disabled={busy} onClick={installNvidia}>
            {busy ? "Installing…" : nvidiaError ? "Try again" : "Download & install"}
          </button>
        </div>
      </div>
    </div>
  );
}
