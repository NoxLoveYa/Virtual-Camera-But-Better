import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "../api";
import { CAPTURE_KEYS, effectiveSource, problemText } from "../camera/schema";
import { useStore } from "../store";
import { labelOf } from "../filters/schema";
import { chromaOffset, pictureOffset, pointLabel, readHeader, type FrameHeader } from "../ui/previewFrame";
import { YUV_TO_RGB_GLSL } from "../ui/yuv";
import { lastFrame } from "../ui/lastFrame";
import { sourceChange, type SourceSize } from "../ui/previewTool";
import { isOrientKey, type Split } from "../ui/compare";
import { useCompare } from "../ui/compareState";
import { CompareOrient, CompareSplit } from "./CompareBar";
import { CropOverlay } from "./CropOverlay";
import { PickOverlay } from "./PickOverlay";

/** The Install button's tooltip while the data folder is encrypted: the banner sits above the preview, not below it. */
const PREVIEW_BLOCKED_REASON = "An administrator must turn off encryption first (see the banner above).";

const COMPARE_OFF_HINT = "Turn Compare off. Esc does it when focus is in the preview or the navigator; elsewhere Esc closes what is in front first";
const COMPARE_HINT = "Compare the camera picture before the filters with the result: drag the bar over the picture";

/** Whether a key press is typing into a field (the orientation key must not take it). */
const isTyping = (t: EventTarget | null) => t instanceof HTMLElement && (t.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(t.tagName));

const VERTEX = `#version 300 es
out vec2 uv;
void main() {
  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  uv = vec2(p.x, 1.0 - p.y);
  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;

// The frame is NV12: Y is one R8 texture, UV one RG8 texture of half the size (linear filtering interpolates the chroma
// between its texels). The matrix (BT.709 limited range, what the engine encodes) is `yuv.ts`'s, the one the eyedropper uses.
// A Compare frame has two pictures: the first (the camera, "Before") is in `luma`/`chroma`, the second (the output, "After")
// in `lumaB`/`chromaB`. `split` is (1 for two pictures, the bar's place 0..1, 1 for a bar that lies across); the pixels
// before the bar (left of it, or above it) are the first picture, the rest the second. The bar moves with a uniform, not a frame.
const FRAGMENT = `#version 300 es
precision highp float;
uniform sampler2D luma;
uniform sampler2D chroma;
uniform sampler2D lumaB;
uniform sampler2D chromaB;
uniform vec3 split;
in vec2 uv;
out vec4 color;
${YUV_TO_RGB_GLSL}
void main() {
  bool second = split.x > 0.5 && (split.z > 0.5 ? uv.y : uv.x) >= split.y;
  if (second) color = vec4(yuvToRgb(texture(lumaB, uv).r, texture(chromaB, uv).rg), 1.0);
  else color = vec4(yuvToRgb(texture(luma, uv).r, texture(chroma, uv).rg), 1.0);
}`;

/** The WebGL textures the frames are uploaded to and drawn from: the planes of up to two pictures. The planes are top row first: no swap or flip on the CPU. */
class FrameView {
  private gl: WebGL2RenderingContext;
  private luma: WebGLTexture;
  private chroma: WebGLTexture;
  private lumaB: WebGLTexture;
  private chromaB: WebGLTexture;
  private splitAt: WebGLUniformLocation | null;
  private w = 0;
  private h = 0;
  private wB = 0;
  private hB = 0;
  /** The picture on the canvas came from a Compare frame (two pictures). */
  private pair = false;
  private drawn = false;

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
    // unit 0: Y, unit 1: UV (first picture); unit 2: Y, unit 3: UV (second picture, Compare only)
    const plane = (unit: number, name: string) => {
      const texture = gl.createTexture()!;
      gl.activeTexture(gl.TEXTURE0 + unit);
      gl.bindTexture(gl.TEXTURE_2D, texture);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
      gl.uniform1i(gl.getUniformLocation(program, name), unit);
      return texture;
    };
    this.luma = plane(0, "luma");
    this.chroma = plane(1, "chroma");
    this.lumaB = plane(2, "lumaB");
    this.chromaB = plane(3, "chromaB");
    this.splitAt = gl.getUniformLocation(program, "split");
    // The second picture's planes exist before any Compare frame: a sampler bound to an incomplete texture warns on every draw.
    gl.activeTexture(gl.TEXTURE0 + 2);
    gl.bindTexture(gl.TEXTURE_2D, this.lumaB);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.R8, 1, 1, 0, gl.RED, gl.UNSIGNED_BYTE, new Uint8Array(1));
    gl.activeTexture(gl.TEXTURE0 + 3);
    gl.bindTexture(gl.TEXTURE_2D, this.chromaB);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RG8, 1, 1, 0, gl.RG, gl.UNSIGNED_BYTE, new Uint8Array(2));
    // rows of the planes are `stride` bytes apart, which is `UNPACK_ROW_LENGTH` texels of 1 byte (Y) or `stride / 2` of 2 (UV)
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    this.gl = gl;
  }

  /** Draw the textures as they are, with the bar where `split` puts it (it only matters for a Compare frame). */
  private paint(split: Split) {
    const { gl } = this;
    gl.uniform3f(this.splitAt, this.pair ? 1 : 0, split.pos, split.orientation === "horizontal" ? 1 : 0);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
    this.drawn = true;
  }

  draw(buf: ArrayBuffer, h: FrameHeader, split: Split) {
    const { gl, canvas } = this;
    // The canvas has the picture's own size; CSS (`object-fit: contain`) letterboxes it inside the 16:9 box.
    if (canvas.width !== h.width || canvas.height !== h.height) {
      canvas.width = h.width;
      canvas.height = h.height;
    }
    gl.viewport(0, 0, h.width, h.height);
    const upload = (unit: number, texture: WebGLTexture, fresh: boolean, internal: number, format: number, w: number, hh: number, rowLength: number, data: Uint8Array) => {
      gl.activeTexture(gl.TEXTURE0 + unit);
      gl.bindTexture(gl.TEXTURE_2D, texture);
      // rows packed with no padding (the stride the engine sends today) take the browser's plain copy
      gl.pixelStorei(gl.UNPACK_ROW_LENGTH, rowLength === w ? 0 : rowLength);
      if (fresh) gl.texImage2D(gl.TEXTURE_2D, 0, internal, w, hh, 0, format, gl.UNSIGNED_BYTE, data);
      else gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, w, hh, format, gl.UNSIGNED_BYTE, data);
    };
    const picture = (index: 0 | 1, luma: WebGLTexture, chroma: WebGLTexture, fresh: boolean, unit: number) => {
      const y = new Uint8Array(buf, pictureOffset(h, index), h.stride * h.height);
      const uv = new Uint8Array(buf, chromaOffset(h, index), (h.stride * h.height) / 2);
      upload(unit, luma, fresh, gl.R8, gl.RED, h.width, h.height, h.stride, y);
      upload(unit + 1, chroma, fresh, gl.RG8, gl.RG, h.width / 2, h.height / 2, h.stride / 2, uv);
    };
    picture(0, this.luma, this.chroma, this.w !== h.width || this.h !== h.height, 0);
    this.w = h.width;
    this.h = h.height;
    if (h.pair) {
      picture(1, this.lumaB, this.chromaB, this.wB !== h.width || this.hB !== h.height, 2);
      this.wB = h.width;
      this.hB = h.height;
    }
    this.pair = !!h.pair;
    this.paint(split);
  }

  /** The bar moved (or Compare ended) with no new frame: draw the textures again. */
  redraw(split: Split) {
    if (this.drawn) this.paint(split);
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
  // The size of the picture on the canvas, for the Compare bar (it lies on the picture, not on the letterbox).
  const [picture, setPicture] = useState<{ w: number; h: number } | null>(null);
  // Where the Compare bar splits a pair frame; with Compare off a pair frame in flight shows the output only (the bar at 0).
  const split = useRef<Split>({ pos: 0, orientation: "vertical" });

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
  // The source size of the frames since a tool began: a camera reopened at another size ends the tool (the picture it works on is gone).
  const toolSource = useRef<SourceSize | null>(null);

  const onFrame = useCallback((buf: ArrayBuffer, mine: number) => {
    if (mine !== gen.current) return;
    const header = readHeader(buf);
    if (!header) return;
    // The tools read the last frame as one picture: a Compare frame (two) is never theirs (they end Compare when they start).
    if (!header.pair) lastFrame.set(buf, header);
    const { previewTool, endPreviewTool } = useStore.getState();
    if (previewTool) {
      const seen = sourceChange(toolSource.current, { w: header.sourceWidth, h: header.sourceHeight }, previewTool.since, performance.now());
      toolSource.current = seen.next;
      if (seen.changed) endPreviewTool();
    } else toolSource.current = null;
    try {
      const canvas = canvasRef.current;
      if (canvas) {
        view.current ??= new FrameView(canvas);
        view.current.draw(buf, header, split.current);
        drawn.current += 1;
        setShown(true);
        setPicture((p) => (p && p.w === header.width && p.h === header.height ? p : { w: header.width, h: header.height }));
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
    // A new engine starts at the output: a Compare the page still shows as on would be a bar over one picture.
    if (useStore.getState().previewPoint === "compare") useStore.getState().setPreviewPoint(null);
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
    if (!keepFrame) {
      setShown(false);
      lastFrame.clear();
    }
  }, [keepFrame]);
  useEffect(() => {
    setShown(false);
    lastFrame.clear();
  }, [deviceId]);

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
          title: vcamBlocked ? PREVIEW_BLOCKED_REASON : undefined,
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

  const chain = useStore((s) => s.chain);
  const extensions = useStore((s) => s.extensions);
  const point = useStore((s) => s.previewPoint);
  const caption = showing ? pointLabel(point, chain.map((f) => ({ id: f.id, label: labelOf(f.kind, extensions) }))) : null;

  // Compare (L4): a toggle. While it is on the preview point is "compare": the engine sends the camera picture and the output in one
  // frame and the bar splits them here, so a drag needs no round trip. Whatever else takes the point (a tool, the preview going
  // off) ends it with it.
  const compareOn = point === "compare";
  const { setPreviewPoint } = useStore.getState();
  const compareButton = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    const apply = () => {
      const c = useCompare.getState();
      const next: Split = { pos: useStore.getState().previewPoint === "compare" ? c.pos : 0, orientation: c.orientation };
      if (next.pos === split.current.pos && next.orientation === split.current.orientation) return;
      split.current = next;
      view.current?.redraw(next);
    };
    apply();
    const stops = [useCompare.subscribe(apply), useStore.subscribe(apply)];
    return () => stops.forEach((stop) => stop());
  }, []);
  // Esc turns Compare off, the orientation key turns the bar. Compare gets only an Escape nothing else used: a menu, the nav flyout,
  // a dialog and a drag preventDefault theirs, and in the inspector Escape goes back to the navigator (Inspector.tsx). So the first
  // Escape closes the thing in front; Compare goes off when focus is in the preview, the navigator or nowhere.
  useEffect(() => {
    if (!compareOn) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented) return;
      if (e.key === "Escape") {
        e.preventDefault();
        // With the bar itself focused it goes away under the focus, which would fall to <body>: the Compare button takes it.
        const inBar = e.target instanceof Element && e.target.closest(".compare-layer") !== null;
        setPreviewPoint(null);
        if (inBar) compareButton.current?.focus();
      } else if (isOrientKey(e, isTyping(e.target))) {
        e.preventDefault();
        useCompare.getState().swapOrientation();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [compareOn, setPreviewPoint]);
  // The picture stopped showing or the window is hidden (the engine already took its point back then), or this view is going
  // away: no one looks at the bar, so Compare is over.
  useEffect(() => {
    if ((!showing || !visible) && useStore.getState().previewPoint === "compare") setPreviewPoint(null);
  }, [showing, visible, setPreviewPoint]);
  useEffect(
    () => () => {
      if (useStore.getState().previewPoint === "compare") useStore.getState().setPreviewPoint(null);
    },
    [],
  );

  // A tool (eyedropper, crop frame) works on a live picture: when it stops showing (paused, reopened, stopped), the tool is over.
  const tool = useStore((s) => s.previewTool);
  useEffect(() => {
    if (!showing && useStore.getState().previewTool) useStore.getState().endPreviewTool();
  }, [showing]);

  return (
    <section className="panel preview" aria-label="Preview">
      <div className={`preview-box${tool ? " has-tool" : ""}`} ref={boxRef}>
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
        {showing && tool?.kind === "pick" && <PickOverlay key={tool.filter} boxRef={boxRef} filter={tool.filter} />}
        {showing && tool?.kind === "crop" && <CropOverlay key={tool.filter} boxRef={boxRef} filter={tool.filter} />}
        {showing && compareOn && <CompareSplit boxRef={boxRef} picture={picture} />}
        <div className="preview-bar">
          {showing ? (
            <span className="preview-fps" title="Frames drawn per second">
              {fps} fps
            </span>
          ) : (
            <span />
          )}
          <div className="preview-tools">
            {showing && compareOn && <CompareOrient />}
            <button
              ref={compareButton}
              type="button"
              className="preview-compare"
              aria-pressed={compareOn}
              disabled={!showing || tool !== null}
              title={tool !== null ? "Finish the pick or the crop first" : !showing ? "Nothing to compare: the camera picture is not showing" : compareOn ? COMPARE_OFF_HINT : COMPARE_HINT}
              onClick={() => setPreviewPoint(compareOn ? null : "compare")}
            >
              Compare
            </button>
            <label className="preview-toggle">
              <span className="label">Preview</span>
              <button type="button" role="switch" aria-checked={previewOn} className="switch" onClick={() => setPreviewOn(!previewOn)} />
            </label>
          </div>
        </div>
        {caption && (
          <span className="preview-caption" role="status">
            {caption}
          </span>
        )}
      </div>
    </section>
  );
}
