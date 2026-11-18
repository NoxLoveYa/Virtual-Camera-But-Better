import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "../api";
import { CAPTURE_KEYS, effectiveSource, problemText } from "../camera/schema";
import { useStore } from "../store";
import { BLOCKED_REASON } from "./VcamBanner";

/** The preview frame of `src-tauri/src/preview.rs`: 16 header bytes (four little-endian u32), then RGBA pixels. */
const HEADER_BYTES = 16;

interface FrameHeader {
  width: number;
  height: number;
  /** Bytes per row. */
  stride: number;
  frame: number;
}

function readHeader(buf: ArrayBuffer): FrameHeader | null {
  if (buf.byteLength < HEADER_BYTES) return null;
  const v = new DataView(buf);
  const [width, height, stride, frame] = [0, 4, 8, 12].map((at) => v.getUint32(at, true));
  const complete = width > 0 && height > 0 && stride >= width * 4 && stride % 4 === 0 && buf.byteLength >= HEADER_BYTES + stride * height;
  return complete ? { width, height, stride, frame } : null;
}

const VERTEX = `#version 300 es
out vec2 uv;
void main() {
  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  uv = vec2(p.x, 1.0 - p.y);
  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;

const FRAGMENT = `#version 300 es
precision mediump float;
uniform sampler2D picture;
in vec2 uv;
out vec4 color;
void main() { color = vec4(texture(picture, uv).rgb, 1.0); }`;

/** One WebGL texture the frames are uploaded to and drawn from. The pixels are RGBA, top row first: no swap or flip on the CPU. */
class FrameView {
  private gl: WebGL2RenderingContext;
  private texture: WebGLTexture;
  private w = 0;
  private h = 0;

  constructor(private canvas: HTMLCanvasElement) {
    const gl = canvas.getContext("webgl2", { alpha: false, antialias: false, depth: false, stencil: false, powerPreference: "low-power" });
    if (!gl) throw new Error("WebGL 2 is not available");
    const shader = (type: number, source: string) => {
      const s = gl.createShader(type)!;
      gl.shaderSource(s, source);
      gl.compileShader(s);
      if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s) ?? "shader error");
      return s;
    };
    const program = gl.createProgram()!;
    gl.attachShader(program, shader(gl.VERTEX_SHADER, VERTEX));
    gl.attachShader(program, shader(gl.FRAGMENT_SHADER, FRAGMENT));
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(program) ?? "link error");
    gl.useProgram(program);
    this.texture = gl.createTexture()!;
    gl.bindTexture(gl.TEXTURE_2D, this.texture);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 4);
    this.gl = gl;
  }

  draw(buf: ArrayBuffer, h: FrameHeader) {
    const { gl, canvas } = this;
    // The canvas has the picture's own size; CSS (`object-fit: contain`) letterboxes it inside the 16:9 box.
    if (canvas.width !== h.width || canvas.height !== h.height) {
      canvas.width = h.width;
      canvas.height = h.height;
    }
    gl.viewport(0, 0, h.width, h.height);
    gl.pixelStorei(gl.UNPACK_ROW_LENGTH, h.stride / 4);
    const pixels = new Uint8Array(buf, HEADER_BYTES, h.stride * h.height);
    if (this.w !== h.width || this.h !== h.height) {
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, h.width, h.height, 0, gl.RGBA, gl.UNSIGNED_BYTE, pixels);
      this.w = h.width;
      this.h = h.height;
    } else {
      gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, h.width, h.height, gl.RGBA, gl.UNSIGNED_BYTE, pixels);
    }
    gl.drawArrays(gl.TRIANGLES, 0, 3);
  }
}

function usePageVisible() {
  const [visible, setVisible] = useState(document.visibilityState === "visible");
  useEffect(() => {
    const on = () => setVisible(document.visibilityState === "visible");
    document.addEventListener("visibilitychange", on);
    return () => document.removeEventListener("visibilitychange", on);
  }, []);
  return visible;
}

/** What the preview says when there is no live picture: a line, a tooltip with the engine's own words, and the next step. */
interface Placeholder {
  text: string;
  title?: string;
  spinner?: boolean;
  action?: { label: string; onClick: () => void; primary?: boolean; disabled?: boolean; title?: string };
}

/** The live picture: frames from the engine over a Tauri Channel, drawn on a canvas. */
export function Preview() {
  const previewOn = useStore((s) => s.previewOn);
  const state = useStore((s) => s.engine.state);
  const reason = useStore((s) => s.engine.reason);
  const kind = useStore((s) => s.engine.kind);
  const apps = useStore((s) => s.engine.apps);
  const appsMaybe = useStore((s) => s.engine.appsMaybe);
  const paused = useStore((s) => s.paused);
  const deviceId = useStore((s) => s.source.video_device_id);
  const vcamMissing = useStore((s) => s.vcam?.state === "missing");
  const vcamBusy = useStore((s) => s.vcamBusy);
  // an install does what Repair does, so it is refused the same way while `%ProgramData%` is encrypted (as in `VcamBanner`)
  const vcamBlocked = useStore((s) => s.vcam?.repairable === false);
  const noCamera = useStore((s) => s.cameras !== null && s.cameras.length === 0);
  const { setPreviewOn, toggleRun, runVcam, refreshCameras } = useStore.getState();
  const visible = usePageVisible();

  const boxRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const view = useRef<FrameView | null>(null);
  const [size, setSize] = useState<{ w: number; h: number } | null>(null);
  const [shown, setShown] = useState(false);
  const [fps, setFps] = useState(0);
  const drawn = useRef(0);

  // The canvas box in device pixels is what the backend sizes the picture for (it caps it at 960x540).
  useEffect(() => {
    const el = boxRef.current!;
    let timer: number | undefined;
    const measure = () => {
      const r = el.getBoundingClientRect();
      const dpr = window.devicePixelRatio || 1;
      const w = Math.round(r.width * dpr);
      const h = Math.round(r.height * dpr);
      setSize((p) => (p && p.w === w && p.h === h ? p : { w, h }));
    };
    measure();
    const ro = new ResizeObserver(() => {
      clearTimeout(timer);
      timer = window.setTimeout(measure, 150);
    });
    ro.observe(el);
    return () => {
      ro.disconnect();
      clearTimeout(timer);
    };
  }, []);

  // Which frames this component is waiting for. A new `set_preview` call opens a new Channel and replaces the stream,
  // so frames of an older channel (`gen` differs) are dropped without an ack: the backend forgot them anyway.
  const gen = useRef(0);
  const want = useRef({ on: false, w: 0, h: 0 });
  const applied = useRef("");
  const busy = useRef(false);
  const dirty = useRef(false);

  const onFrame = useCallback((buf: ArrayBuffer, mine: number) => {
    if (mine !== gen.current) return;
    const header = readHeader(buf);
    if (!header) return;
    try {
      const canvas = canvasRef.current;
      if (canvas) {
        view.current ??= new FrameView(canvas);
        view.current.draw(buf, header);
        drawn.current += 1;
        setShown(true);
      }
    } catch (e) {
      console.error("preview: cannot draw the frame", e);
    } finally {
      // Always acknowledge, or the backend would hold the next frame back for a second.
      api.previewAck(header.frame).catch(() => {});
    }
  }, []);

  /** Brings the backend to `want`. One call at a time, in order: two overlapping `set_preview` calls could finish swapped. */
  const sync = useCallback(async () => {
    if (busy.current) {
      dirty.current = true;
      return;
    }
    busy.current = true;
    try {
      do {
        dirty.current = false;
        const { on, w, h } = want.current;
        const key = on ? `${w}x${h}` : "off";
        if (key === applied.current) continue;
        const mine = ++gen.current;
        applied.current = "";
        await api.setPreview(on, w, h, on ? (buf) => onFrame(buf, mine) : undefined);
        applied.current = key;
      } while (dirty.current);
    } catch (e) {
      console.error("preview: set_preview failed", e);
    } finally {
      busy.current = false;
    }
  }, [onFrame]);

  const streaming = previewOn && visible && !!size && size.w > 0 && size.h > 0;
  useEffect(() => {
    want.current = { on: streaming, w: size?.w ?? 0, h: size?.h ?? 0 };
    void sync();
  }, [streaming, size, sync]);
  useEffect(
    () => () => {
      want.current = { ...want.current, on: false };
      void sync();
    },
    [sync],
  );

  // The virtual camera was installed or removed: the backend engine (and its preview tap) is a new one, or none was
  // there when the preview was asked for. Ask again.
  const vcamEpoch = useStore((s) => s.vcamEpoch);
  const seenEpoch = useRef(vcamEpoch);
  useEffect(() => {
    if (seenEpoch.current === vcamEpoch) return;
    seenEpoch.current = vcamEpoch;
    applied.current = "";
    void sync();
  }, [vcamEpoch, sync]);

  // A lost WebGL context (GPU reset) is rebuilt on the next frame.
  useEffect(() => {
    const canvas = canvasRef.current!;
    const lost = (e: Event) => {
      e.preventDefault();
      view.current = null;
    };
    canvas.addEventListener("webglcontextlost", lost);
    return () => canvas.removeEventListener("webglcontextlost", lost);
  }, []);

  // Frames drawn per second.
  useEffect(() => {
    const t = setInterval(() => {
      setFps(drawn.current);
      drawn.current = 0;
    }, 1000);
    return () => clearInterval(t);
  }, []);

  // The last frame stays on the canvas, dimmed, while the camera is paused or reopened with other settings, so the picture
  // does not blink out. Any other state drops it, and so does another camera: a stale picture never stays up.
  const keepFrame = previewOn && (state === "live" || (state === "standby" && reason === "reopening"));
  useEffect(() => {
    if (!keepFrame) setShown(false);
  }, [keepFrame]);
  useEffect(() => setShown(false), [deviceId]);

  // The engine announces a reopen a moment after the edit (it gathers edits first). The preview says so at once: a change
  // to the capture settings of a live camera is "applying" until the engine has left live and come back (or 5 s pass).
  const captureKey = useStore((s) => JSON.stringify(CAPTURE_KEYS.map((k) => effectiveSource(s.source, s.inUse)[k])));
  const [applying, setApplying] = useState(false);
  const seenKey = useRef(captureKey);
  const left = useRef(false);
  useEffect(() => {
    if (seenKey.current === captureKey) return;
    seenKey.current = captureKey;
    if (state === "live") {
      left.current = false;
      setApplying(true);
    }
  }, [captureKey, state]);
  useEffect(() => {
    if (!applying) return;
    if (state !== "live") left.current = true;
    else if (left.current) setApplying(false);
  }, [applying, state]);
  useEffect(() => {
    if (!applying) return;
    const timer = setTimeout(() => setApplying(false), 5000);
    return () => clearTimeout(timer);
  }, [applying]);

  const refresh = () => void refreshCameras();
  let ph: Placeholder | null = null;
  let dim = false;
  if (!previewOn) ph = { text: "Preview off" };
  else if (state === "stopped") {
    if (vcamMissing) {
      ph = {
        text: "Install the virtual camera to start",
        action: {
          label: "Install",
          onClick: () => void runVcam("install"),
          disabled: vcamBusy !== null || vcamBlocked,
          title: vcamBlocked ? BLOCKED_REASON : undefined,
        },
      };
    } else if (noCamera) ph = { text: "No camera found", action: { label: "Refresh", onClick: refresh } };
    else ph = { text: "Camera stopped", action: { label: "Start", onClick: () => void toggleRun(), primary: true } };
  } else if (paused) {
    ph = { text: "Paused: apps see the paused slate" };
    dim = true;
  } else if (state === "live" && applying) {
    ph = { text: "Applying camera settings…", spinner: true };
    dim = true;
  } else if (state === "reconnecting") {
    const p = problemText(kind, reason, apps, appsMaybe);
    ph = { text: p.short, title: reason ?? undefined };
  } else if (state === "standby") {
    if (reason === "idle") ph = { text: "Camera off: no app is using Better Cam" };
    else {
      ph = { text: reason === "reopening" ? "Applying camera settings…" : "Opening the camera…", spinner: true };
      dim = reason === "reopening";
    }
  } else if (!shown) ph = { text: "Starting…" };
  const showing = ph === null;
  // A dimmed state shows the last frame at 35 %; any other placeholder hides the canvas.
  const canvasClass = showing ? "" : dim && shown ? " is-dim" : " is-off";

  return (
    <section className="panel preview" aria-label="Preview">
      <div className="preview-box" ref={boxRef}>
        <canvas ref={canvasRef} className={`preview-canvas${canvasClass}`} role="img" aria-label="Camera preview" />
        {ph && (
          <div className="preview-ph" title={ph.title}>
            <div className="ph-stack">
              <span className="ph-text" role="status">
                {ph.spinner && <span className="spinner" aria-hidden />}
                {ph.text}
              </span>
              {ph.action && (
                <button type="button" className={ph.action.primary ? "primary" : undefined} disabled={ph.action.disabled} title={ph.action.title} onClick={ph.action.onClick}>
                  {ph.action.label}
                </button>
              )}
            </div>
          </div>
        )}
        <div className="preview-bar">
          {showing ? (
            <span className="preview-fps" title="Frames drawn per second">
              {fps} fps
            </span>
          ) : (
            <span />
          )}
          <label className="preview-toggle">
            <span className="label">Preview</span>
            <button type="button" role="switch" aria-checked={previewOn} className="switch" onClick={() => setPreviewOn(!previewOn)} />
          </label>
        </div>
      </div>
    </section>
  );
}
