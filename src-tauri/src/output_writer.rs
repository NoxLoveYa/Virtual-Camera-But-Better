//! The writer side of the pipeline: encode the output frame on the GPU, read it back through the staging ring and
//! publish it into `frames.bin` (plan 2.2 `[writer]`, plan 2.5).
//!
//! The virtual camera DLL only shows a frame whose size and subtype equal what its stream negotiated, so
//! [`OutputWriter::target`] follows `Writer::negotiated()` frame by frame: a consumer that asks for 1280x720 RGB32
//! gets 1280x720 BGRX, even while the configured output is 1920x1080. With no consumer attached the configured size
//! is rendered as NV12.

use std::sync::Arc;

use vcam_protocol::{Format, SlotFormat, Writer, SUBTYPE_NV12, SUBTYPE_RGB32};
use windows::core::BOOL;
use windows::Win32::Graphics::Direct3D11::{ID3D11Query, D3D11_QUERY_DESC, D3D11_QUERY_EVENT};

use crate::gpu::output::{EncodeFormat, Encoder};
use crate::gpu::readback::{MappedPlane, ReadbackRing};
use crate::gpu::{GpuError, GpuResult, Renderer, RgbaTexture};

/// What to render for the writer right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub width: u32,
    pub height: u32,
    pub format: EncodeFormat,
}

impl Target {
    pub fn format_name(&self) -> &'static str {
        match self.format {
            EncodeFormat::Nv12 => "nv12",
            EncodeFormat::Bgra => "rgb32",
        }
    }
}

/// The frame the writer should render: the negotiated format while a consumer is attached, else `configured` as
/// NV12.
pub fn target_for(writer: &Writer, configured: (u32, u32)) -> Target {
    target_from(writer.negotiated(), configured)
}

/// [`target_for`] for a format that was read already (the render loop reads the consumer once a frame and uses the
/// answer for the size, the format and the decision to publish at all).
pub fn target_from(negotiated: Option<Format>, configured: (u32, u32)) -> Target {
    match negotiated {
        Some(f) => Target {
            width: f.width,
            height: f.height,
            format: if f.subtype == SUBTYPE_RGB32 {
                EncodeFormat::Bgra
            } else {
                EncodeFormat::Nv12
            },
        },
        None => Target {
            width: configured.0,
            height: configured.1,
            format: EncodeFormat::Nv12,
        },
    }
}

/// Travels with a frame through the readback ring.
#[derive(Clone, Copy, Debug)]
pub struct FrameTag {
    /// Capture time in QPC ticks (the sample time the DLL stamps on the frame).
    pub capture_qpc: u64,
    /// Capture time in 100 ns units, as the capture layer reports it (for latency numbers).
    pub capture_100ns: i64,
    /// When the capture callback received the frame, 100 ns units.
    pub arrival_100ns: i64,
}

/// A frame that reached `frames.bin`.
#[derive(Clone, Copy, Debug)]
pub struct Published {
    pub tag: FrameTag,
    pub width: u32,
    pub height: u32,
    pub format: EncodeFormat,
    /// QPC ticks when the publish finished.
    pub publish_qpc: u64,
    /// Time spent mapping the staging textures and copying into the slot, milliseconds.
    pub copy_ms: f64,
}

/// How many frames the CPU may run ahead of the GPU on frames that are not read back.
const FENCE_DEPTH: usize = 2;

/// A frame that is read back makes the render thread wait for the GPU, which is what keeps the CPU from queuing work
/// faster than the GPU does it. A frame that is not read back (no app reads the virtual camera, or the frame would be
/// dropped by it) waits for nothing, so an event query per frame stands in: before frame N is queued, frame
/// N - [`FENCE_DEPTH`] must be done. With a GPU that keeps up (the normal case) the query is long done and this costs
/// two calls.
struct Fence {
    queries: Vec<ID3D11Query>,
    issued: Vec<bool>,
    next: usize,
}

impl Fence {
    fn new(renderer: &Renderer) -> Option<Fence> {
        let desc = D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT,
            MiscFlags: 0,
        };
        let mut queries = Vec::with_capacity(FENCE_DEPTH);
        for _ in 0..FENCE_DEPTH {
            let mut q = None;
            // SAFETY: plain call; `desc` and the out pointer are locals.
            unsafe { renderer.gpu.device.CreateQuery(&desc, Some(&mut q)).ok()? };
            queries.push(q?);
        }
        Some(Fence {
            issued: vec![false; queries.len()],
            queries,
            next: 0,
        })
    }

    fn done(&self, renderer: &Renderer, i: usize) -> bool {
        let mut done = BOOL(0);
        // SAFETY: the data pointer is a local `BOOL`, the size of an event query's data. Flushing is allowed: the
        // frame this waits for may still be in the command buffer.
        let r = unsafe {
            renderer.gpu.context.GetData(
                &self.queries[i],
                Some(&mut done as *mut _ as *mut core::ffi::c_void),
                std::mem::size_of::<BOOL>() as u32,
                0,
            )
        };
        // S_FALSE (not ready) leaves `done` false. An error (a lost device) ends the wait: the loop notices it.
        r.is_err() || done.as_bool()
    }

    /// Wait (at most 100 ms) until the frame queued [`FENCE_DEPTH`] calls ago is done, then queue this one's marker.
    fn pace(&mut self, renderer: &Renderer) {
        let i = self.next;
        if self.issued[i] {
            let started = std::time::Instant::now();
            while !self.done(renderer, i) && started.elapsed() < std::time::Duration::from_millis(100) {
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        }
        // SAFETY: plain call on the render thread's context.
        unsafe { renderer.gpu.context.End(&self.queries[i]) };
        self.issued[i] = true;
        self.next = (i + 1) % self.queries.len();
    }
}

/// Encoder, readback ring and `frames.bin` publisher.
pub struct OutputWriter {
    writer: Arc<Writer>,
    encoder: Encoder,
    ring: Option<(Target, ReadbackRing<FrameTag>)>,
    renderer: Arc<Renderer>,
    fence: Option<Fence>,
    /// Frames the ring dropped because it was full.
    pub dropped: u64,
    /// Frames `Writer::publish` refused.
    pub rejected: u64,
    /// Frames that were rendered and not encoded, read back or published ([`OutputWriter::skip`]).
    pub skipped: u64,
}

impl OutputWriter {
    pub fn new(renderer: Arc<Renderer>, writer: Arc<Writer>) -> GpuResult<OutputWriter> {
        Ok(OutputWriter {
            writer,
            encoder: Encoder::new(renderer.clone())?,
            ring: None,
            fence: Fence::new(&renderer),
            renderer,
            dropped: 0,
            rejected: 0,
            skipped: 0,
        })
    }

    /// A frame that goes nowhere: no encode, no readback, no publish. Hands the frame's commands to the GPU and keeps
    /// the CPU at most [`FENCE_DEPTH`] frames ahead of it ([`Fence`]).
    pub fn skip(&mut self) {
        self.skipped += 1;
        self.renderer.flush();
        if let Some(f) = self.fence.as_mut() {
            f.pace(&self.renderer);
        }
    }

    pub fn writer(&self) -> &Arc<Writer> {
        &self.writer
    }

    /// See [`target_for`].
    pub fn target(&self, configured: (u32, u32)) -> Target {
        target_for(&self.writer, configured)
    }

    /// Frames waiting in the readback ring.
    pub fn pending(&self) -> usize {
        self.ring.as_ref().map_or(0, |(_, r)| r.pending())
    }

    /// Encode `frame` (which must be exactly `target.width` x `target.height`) and queue its readback. A format or
    /// size change drops what is still in flight.
    pub fn submit(&mut self, frame: &RgbaTexture, target: Target, tag: FrameTag) -> GpuResult<()> {
        if frame.width != target.width || frame.height != target.height {
            return Err(GpuError::Invalid(format!(
                "frame is {}x{}, target is {}x{}",
                frame.width, frame.height, target.width, target.height
            )));
        }
        let targets = self.encoder.encode(frame, target.format)?;
        if self.ring.as_ref().map(|(t, _)| *t) != Some(target) {
            self.ring = None;
            self.ring = Some((
                target,
                ReadbackRing::new(self.renderer.gpu.clone(), &targets.plane_descs())?,
            ));
        }
        let (_, ring) = self.ring.as_mut().expect("ring was just made");
        if ring.submit(&targets.textures(), tag)?.is_some() {
            self.dropped += 1;
        }
        Ok(())
    }

    /// Publish the oldest finished frame. With `wait` false it returns `Ok(None)` when the GPU is still busy.
    pub fn poll(&mut self, wait: bool) -> GpuResult<Option<Published>> {
        let Some((target, ring)) = self.ring.as_mut() else {
            return Ok(None);
        };
        let target = *target;
        let writer = self.writer.clone();
        let started = std::time::Instant::now();
        let result = ring.poll(wait, |planes, tag| {
            publish_planes(&writer, target, planes, tag)
        })?;
        let Some(result) = result else {
            return Ok(None);
        };
        match result {
            Ok(mut p) => {
                p.copy_ms = started.elapsed().as_secs_f64() * 1000.0;
                Ok(Some(p))
            }
            Err(e) => {
                self.rejected += 1;
                Err(e)
            }
        }
    }

    /// Forget frames in flight.
    pub fn clear(&mut self) {
        if let Some((_, r)) = self.ring.as_mut() {
            r.clear();
        }
    }
}

/// Copy the mapped planes of one frame into a new slot of the file.
fn publish_planes(
    writer: &Writer,
    target: Target,
    planes: &[MappedPlane<'_>],
    tag: FrameTag,
) -> GpuResult<Published> {
    let (w, h) = (target.width, target.height);
    let (stride, subtype) = match target.format {
        EncodeFormat::Nv12 => (w, SUBTYPE_NV12),
        EncodeFormat::Bgra => (w * 4, SUBTYPE_RGB32),
    };
    let stride_us = stride as usize;
    writer
        .publish(
            SlotFormat {
                width: w,
                height: h,
                stride,
                subtype,
            },
            tag.capture_qpc,
            |dst| match target.format {
                EncodeFormat::Nv12 => {
                    let luma = stride_us * h as usize;
                    planes[0].copy_to(&mut dst[..luma], stride_us);
                    planes[1].copy_to(&mut dst[luma..], stride_us);
                }
                EncodeFormat::Bgra => planes[0].copy_to(dst, stride_us),
            },
        )
        .map_err(|e| GpuError::Invalid(format!("publish refused the frame: {e:?}")))?;
    Ok(Published {
        tag,
        width: w,
        height: h,
        format: target.format,
        publish_qpc: vcam_protocol::qpc_now(),
        copy_ms: 0.0,
    })
}
