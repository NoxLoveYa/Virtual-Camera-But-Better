import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "../api";
import { useStore } from "../store";

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

const sentence = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

/** The live picture: frames from the engine over a Tauri Channel, drawn on a canvas. */
export function Preview() {
  const previewOn = useStore((s) => s.previewOn);
  const state = useStore((s) => s.engine.state);
  const reason = useStore((s) => s.engine.reason);
  const paused = useStore((s) => s.paused);
  const { setPreviewOn } = useStore.getState();
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

  const picture = previewOn && state === "live" && !paused;
  useEffect(() => {
    if (!picture) setShown(false);
  }, [picture]);

  let placeholder: string | null = null;
  if (!previewOn) placeholder = "Preview off";
  else if (state === "stopped") placeholder = "Camera stopped";
  else if (paused) placeholder = "Paused";
  else if (state === "reconnecting") placeholder = reason ? sentence(reason) : "Waiting for the camera";
  else if (state === "standby") placeholder = reason === "idle" ? "Camera off: no app is using Better Cam" : "Opening the camera…";
  else if (!shown) placeholder = "Starting…";
  const showing = placeholder === null;

  return (
    <section className="panel preview" aria-label="Preview">
      <div className="preview-box" ref={boxRef}>
        <canvas ref={canvasRef} className={`preview-canvas${showing ? "" : " is-off"}`} role="img" aria-label="Camera preview" />
        {placeholder && (
          <div className="preview-ph" role="status">
            {placeholder}
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
