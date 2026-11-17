//! Readback (plan 4.2 stage 5): a ring of 3 staging textures per plane.
//!
//! [`ReadbackRing::submit`] copies the encoded planes into the next staging slot and returns at once;
//! [`ReadbackRing::poll`] maps the oldest pending slot. A caller that maps one frame later (frame N+1 maps frame N)
//! never waits for the GPU, because the copy finished long before. When all three slots are busy the oldest frame is
//! dropped, so the ring never holds more than 3 frames of latency.

use std::collections::VecDeque;
use std::sync::Arc;

use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_FLAG_DO_NOT_WAIT,
    D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING;

use super::{GpuDevice, GpuError, GpuResult};

/// One plane of a frame to read back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneDesc {
    pub format: DXGI_FORMAT,
    pub width: u32,
    pub height: u32,
    pub bytes_per_texel: u32,
}

impl PlaneDesc {
    /// Bytes of real pixels in a row.
    pub fn row_bytes(&self) -> usize {
        (self.width * self.bytes_per_texel) as usize
    }
}

/// A mapped plane of a finished frame. Rows are `row_pitch` bytes apart (the driver's choice, usually wider than
/// `row_bytes`).
pub struct MappedPlane<'a> {
    data: &'a [u8],
    pub row_pitch: usize,
    pub row_bytes: usize,
    pub rows: usize,
}

impl<'a> MappedPlane<'a> {
    /// Wrap memory laid out with the given pitch (also used by tests with made-up pitches).
    pub fn new(data: &'a [u8], row_pitch: usize, row_bytes: usize, rows: usize) -> MappedPlane<'a> {
        MappedPlane {
            data,
            row_pitch,
            row_bytes,
            rows,
        }
    }

    /// The pixels of row `y`, without the padding.
    pub fn row(&self, y: usize) -> &'a [u8] {
        &self.data[y * self.row_pitch..y * self.row_pitch + self.row_bytes]
    }

    /// Copy all rows into `dst`, `dst_stride` bytes apart. The stride must be at least `row_bytes`; bytes between
    /// `row_bytes` and the stride are left alone.
    pub fn copy_to(&self, dst: &mut [u8], dst_stride: usize) {
        for y in 0..self.rows {
            dst[y * dst_stride..y * dst_stride + self.row_bytes].copy_from_slice(self.row(y));
        }
    }
}

/// Staging ring for frames made of one or more planes. `T` travels with each frame (capture time, sizes).
pub struct ReadbackRing<T> {
    gpu: Arc<GpuDevice>,
    planes: Vec<PlaneDesc>,
    slots: Vec<Vec<ID3D11Texture2D>>,
    pending: VecDeque<(usize, T)>,
    next: usize,
    /// Frames dropped because the ring was full.
    pub dropped: u64,
}

impl<T> ReadbackRing<T> {
    /// Slots in the ring.
    pub const SLOTS: usize = 3;

    pub fn new(gpu: Arc<GpuDevice>, planes: &[PlaneDesc]) -> GpuResult<ReadbackRing<T>> {
        let mut slots = Vec::new();
        for _ in 0..Self::SLOTS {
            let mut set = Vec::new();
            for p in planes {
                set.push(gpu.create_texture(&D3D11_TEXTURE2D_DESC {
                    Width: p.width,
                    Height: p.height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: p.format,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                })?);
            }
            slots.push(set);
        }
        Ok(ReadbackRing {
            gpu,
            planes: planes.to_vec(),
            slots,
            pending: VecDeque::new(),
            next: 0,
            dropped: 0,
        })
    }

    pub fn planes(&self) -> &[PlaneDesc] {
        &self.planes
    }

    /// Frames copied but not yet mapped.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Forget every pending frame (the output format changed).
    pub fn clear(&mut self) {
        self.pending.clear();
    }

    /// Copy `sources` (one texture per plane, same formats and sizes as the ring was made with) into the next slot
    /// and submit the work to the GPU. Never waits. Returns the frame that was dropped to make room, if any.
    pub fn submit(&mut self, sources: &[&ID3D11Texture2D], tag: T) -> GpuResult<Option<T>> {
        if sources.len() != self.planes.len() {
            return Err(GpuError::Invalid(format!(
                "readback expects {} planes, got {}",
                self.planes.len(),
                sources.len()
            )));
        }
        let evicted = if self.pending.len() >= Self::SLOTS {
            self.dropped += 1;
            self.pending.pop_front().map(|(_, t)| t)
        } else {
            None
        };
        let slot = self.next;
        self.next = (self.next + 1) % Self::SLOTS;
        // SAFETY: plain calls; source and staging textures have the same format and size.
        unsafe {
            for (staging, src) in self.slots[slot].iter().zip(sources) {
                self.gpu.context.CopyResource(staging, *src);
            }
            self.gpu.context.Flush();
        }
        self.pending.push_back((slot, tag));
        Ok(evicted)
    }

    /// Map the oldest pending frame and hand its planes to `f`. With `wait` false, a frame the GPU has not finished
    /// gives `Ok(None)` at once and stays pending; with `wait` true the call blocks until it is ready.
    pub fn poll<R>(
        &mut self,
        wait: bool,
        f: impl FnOnce(&[MappedPlane<'_>], T) -> R,
    ) -> GpuResult<Option<R>> {
        let Some(&(slot, _)) = self.pending.front() else {
            return Ok(None);
        };
        let flags = if wait {
            0
        } else {
            D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32
        };
        let ctx = &self.gpu.context;
        let mut mapped: Vec<D3D11_MAPPED_SUBRESOURCE> = Vec::new();
        for staging in &self.slots[slot] {
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            // SAFETY: plain call; `m` is a local.
            let r = unsafe { ctx.Map(staging, 0, D3D11_MAP_READ, flags, Some(&mut m)) };
            match r {
                Ok(()) => mapped.push(m),
                Err(e) => {
                    for (done, staging) in mapped.iter().zip(&self.slots[slot]) {
                        let _ = done;
                        // SAFETY: unmaps what this call mapped.
                        unsafe { ctx.Unmap(staging, 0) };
                    }
                    return if e.code() == DXGI_ERROR_WAS_STILL_DRAWING {
                        Ok(None)
                    } else {
                        Err(e.into())
                    };
                }
            }
        }
        let planes: Vec<MappedPlane<'_>> = mapped
            .iter()
            .zip(&self.planes)
            .map(|(m, p)| {
                let pitch = m.RowPitch as usize;
                let rows = p.height as usize;
                // SAFETY: the driver maps `RowPitch * rows` readable bytes (the last row may be shorter, which the
                // slice length respects) until the Unmap below.
                let data = unsafe {
                    std::slice::from_raw_parts(
                        m.pData as *const u8,
                        pitch * (rows - 1) + p.row_bytes(),
                    )
                };
                MappedPlane::new(data, pitch, p.row_bytes(), rows)
            })
            .collect();
        let (_, tag) = self.pending.pop_front().expect("front was checked");
        let out = f(&planes, tag);
        drop(planes);
        for (staging, _) in self.slots[slot].iter().zip(&mapped) {
            // SAFETY: balances the Map calls above.
            unsafe { ctx.Unmap(staging, 0) };
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_to_skips_the_padding_of_odd_pitches() {
        // 5 pixels per row, rows 13 bytes apart (as a driver might lay them out), 3 rows.
        let (pitch, row_bytes, rows) = (13usize, 5usize, 3usize);
        let mut src = vec![0xEEu8; pitch * (rows - 1) + row_bytes];
        for y in 0..rows {
            for x in 0..row_bytes {
                src[y * pitch + x] = (y * 10 + x) as u8;
            }
        }
        let plane = MappedPlane::new(&src, pitch, row_bytes, rows);
        assert_eq!(plane.row(1), &[10, 11, 12, 13, 14]);
        // Destination stride 7: padding bytes stay untouched.
        let mut dst = vec![0xAAu8; 7 * rows];
        plane.copy_to(&mut dst, 7);
        for y in 0..rows {
            for x in 0..row_bytes {
                assert_eq!(dst[y * 7 + x], (y * 10 + x) as u8);
            }
            assert_eq!(&dst[y * 7 + row_bytes..(y + 1) * 7], &[0xAA, 0xAA]);
        }
    }
}
