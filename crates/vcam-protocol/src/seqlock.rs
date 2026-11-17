//! Access to the shared frame file: header fields as atomics, and the seqlock that publishes frames.
//!
//! Writer (one process): pick the slot that is neither `latest_slot` nor the one the reader marked in use, set the
//! slot's `seq` odd, copy the frame, set `seq` even, then store `latest_slot` with release ordering.
//! Reader (the DLL): read `latest_slot`, read `seq`, copy, re-read `seq`; retry once when it changed or was odd.
//!
//! The frame bytes themselves are copied with plain memcpy while the other side may be writing them. That is the
//! usual seqlock trade-off: a copy that overlapped a write is thrown away when the sequence check fails, and no
//! decision is ever taken from bytes that were not confirmed by an unchanged even `seq`.

use std::marker::PhantomData;
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

use crate::header::{
    off, slot_off, Format, RawHeader, SlotMeta, WriterState, ADVERTISED_MAX, FILE_SIZE,
    HEADER_SIZE, NO_SLOT, SLOT_CAPACITY, SLOT_COUNT,
};
use crate::validate::{
    frame_bytes, min_stride, validate_format, validate_slot, FormatError, SlotError, ValidHeader,
};
use crate::MAGIC;

/// A view of the shared file: `len` bytes at `base`. Cheap to build, does not own the memory.
///
/// A view made with `writable == false` never stores: every write method returns without touching memory, because
/// a store into a read-only mapping would fault.
pub struct Shared {
    base: *mut u8,
    len: usize,
    writable: bool,
}

// SAFETY: all access goes through atomics or bounds-checked raw copies; the memory is shared between processes by
// design.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// Format of a frame about to be published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotFormat {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub subtype: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishError {
    ReadOnly,
    Format(FormatError),
    BadStride,
    TooLarge,
}

/// What the reader's callback did with a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// The callback copied the frame.
    Taken,
    /// The callback did not want it (already seen, or the wrong format).
    Skip,
}

/// Result of [`Shared::read_latest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// No frame has been published yet.
    NoFrame,
    /// The callback copied this slot and the sequence check confirmed the copy.
    Taken(SlotMeta),
    /// The callback skipped this slot.
    Skipped(SlotMeta),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// The writer kept changing the slot; try again later.
    Torn,
    /// The slot descriptor was stable but invalid: a hostile or broken file.
    Invalid(SlotError),
}

/// Frame bytes of a slot, valid for the duration of the reader callback.
pub struct SlotData<'a> {
    ptr: *const u8,
    len: usize,
    _m: PhantomData<&'a [u8]>,
}

impl SlotData<'_> {
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// Copy `dst.len()` bytes starting at `src_off` of the slot data. Panics when the range is outside the slot
    /// (the descriptor was validated, so that is a caller bug).
    pub fn copy_to(&self, src_off: usize, dst: &mut [u8]) {
        let end = src_off.checked_add(dst.len()).expect("range overflow");
        assert!(end <= self.len, "read past the slot data");
        // SAFETY: range checked above; the slot lies inside the mapping (validated by the caller of `new`).
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr.add(src_off), dst.as_mut_ptr(), dst.len())
        };
    }
}

impl Shared {
    /// Wrap `len` bytes at `base`.
    ///
    /// # Safety
    /// `base` must be valid for reads (and writes when `writable`) of `len` bytes for as long as the `Shared` is used,
    /// and aligned to 8 bytes. Returns `None` when `len` is smaller than the header or `base` is misaligned.
    pub unsafe fn new(base: *mut u8, len: usize, writable: bool) -> Option<Shared> {
        if base.is_null() || len < HEADER_SIZE || base as usize % 8 != 0 {
            return None;
        }
        Some(Shared {
            base,
            len,
            writable,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    fn a32(&self, off: usize) -> &AtomicU32 {
        debug_assert!(off % 4 == 0 && off + 4 <= HEADER_SIZE);
        // SAFETY: `off` is a header offset (< HEADER_SIZE <= len), 4-aligned from an 8-aligned base.
        unsafe { AtomicU32::from_ptr(self.base.add(off) as *mut u32) }
    }
    fn a64(&self, off: usize) -> &AtomicU64 {
        debug_assert!(off % 8 == 0 && off + 8 <= HEADER_SIZE);
        // SAFETY: as in `a32`, 8-aligned.
        unsafe { AtomicU64::from_ptr(self.base.add(off) as *mut u64) }
    }

    /// Every header field, each loaded atomically (the set is not a consistent snapshot, every field is validated
    /// on its own).
    pub fn header(&self) -> RawHeader {
        RawHeader::load(&|o| self.a32(o).load(Ordering::Acquire), &|o| {
            self.a64(o).load(Ordering::Acquire)
        })
    }

    /// Descriptor of slot `i` (unvalidated, not checked for tearing).
    pub fn slot_meta(&self, i: usize) -> SlotMeta {
        let so = slot_off(i.min(SLOT_COUNT - 1));
        SlotMeta {
            index: i as u32,
            seq: self.a64(so + off::SLOT_SEQ).load(Ordering::Acquire),
            width: self.a32(so + off::SLOT_WIDTH).load(Ordering::Relaxed),
            height: self.a32(so + off::SLOT_HEIGHT).load(Ordering::Relaxed),
            stride: self.a32(so + off::SLOT_STRIDE).load(Ordering::Relaxed),
            subtype: self.a32(so + off::SLOT_SUBTYPE).load(Ordering::Relaxed),
            capture_qpc: self.a64(so + off::SLOT_CAPTURE_QPC).load(Ordering::Relaxed),
            data_offset: self.a64(so + off::SLOT_DATA_OFFSET).load(Ordering::Relaxed),
            data_size: self.a64(so + off::SLOT_DATA_SIZE).load(Ordering::Relaxed),
        }
    }

    // ---- writer side ----------------------------------------------------------------------------------------

    /// Reset the whole header and publish it: magic last, so a reader never sees a half-built header.
    /// Fails when the mapping is smaller than [`FILE_SIZE`] or read-only.
    pub fn init_writer(
        &self,
        pid: u32,
        qpc_frequency: u64,
        now_qpc: u64,
    ) -> Result<(), PublishError> {
        if !self.writable {
            return Err(PublishError::ReadOnly);
        }
        if (self.len as u64) < FILE_SIZE {
            return Err(PublishError::TooLarge);
        }
        self.a32(off::MAGIC).store(0, Ordering::Release);
        // SAFETY: the header is inside the mapping; atomics are not used on these bytes while we are the only
        // initialiser (readers see magic == 0 and stay away).
        unsafe { std::ptr::write_bytes(self.base.add(4), 0, HEADER_SIZE - 4) };
        self.a32(off::PROTOCOL)
            .store(crate::PROTOCOL_VERSION, Ordering::Relaxed);
        self.a32(off::HEADER_SIZE)
            .store(HEADER_SIZE as u32, Ordering::Relaxed);
        self.a32(off::SLOT_COUNT)
            .store(SLOT_COUNT as u32, Ordering::Relaxed);
        self.a64(off::FILE_SIZE).store(FILE_SIZE, Ordering::Relaxed);
        self.a64(off::SLOT_CAPACITY)
            .store(SLOT_CAPACITY, Ordering::Relaxed);
        self.a32(off::WRITER_PID).store(pid, Ordering::Relaxed);
        self.a32(off::WRITER_STATE)
            .store(WriterState::Closed as u32, Ordering::Relaxed);
        self.a64(off::WRITER_HEARTBEAT)
            .store(now_qpc, Ordering::Relaxed);
        self.a64(off::QPC_FREQUENCY)
            .store(qpc_frequency, Ordering::Relaxed);
        self.a32(off::READER_SLOT).store(NO_SLOT, Ordering::Relaxed);
        self.a32(off::LATEST_SLOT).store(NO_SLOT, Ordering::Relaxed);
        self.a32(off::MAGIC).store(MAGIC, Ordering::Release);
        Ok(())
    }

    pub fn set_writer_state(&self, state: WriterState) {
        if self.writable {
            self.a32(off::WRITER_STATE)
                .store(state as u32, Ordering::Release);
        }
    }

    pub fn set_writer_heartbeat(&self, now_qpc: u64) {
        if self.writable {
            self.a64(off::WRITER_HEARTBEAT)
                .store(now_qpc, Ordering::Release);
        }
    }

    /// Write the advertised media types (at most [`ADVERTISED_MAX`]); the count goes last.
    pub fn set_advertised(&self, formats: &[Format]) {
        if !self.writable {
            return;
        }
        let n = formats.len().min(ADVERTISED_MAX);
        self.a32(off::ADVERTISED_COUNT).store(0, Ordering::Release);
        for (i, f) in formats[..n].iter().enumerate() {
            self.store_format(off::ADVERTISED + i * off::FMT_SIZE, f);
        }
        self.a32(off::ADVERTISED_COUNT)
            .store(n as u32, Ordering::Release);
    }

    fn store_format(&self, o: usize, f: &Format) {
        self.a32(o + off::FMT_WIDTH)
            .store(f.width, Ordering::Relaxed);
        self.a32(o + off::FMT_HEIGHT)
            .store(f.height, Ordering::Relaxed);
        self.a32(o + off::FMT_FPS_NUM)
            .store(f.fps_num, Ordering::Relaxed);
        self.a32(o + off::FMT_FPS_DEN)
            .store(f.fps_den, Ordering::Relaxed);
        self.a32(o + off::FMT_SUBTYPE)
            .store(f.subtype, Ordering::Relaxed);
    }

    /// Publish one frame. `fill` receives the slot's data bytes (exactly the frame size) and writes the frame.
    /// Returns the slot's new (even) `seq`.
    pub fn publish(
        &self,
        f: SlotFormat,
        capture_qpc: u64,
        now_qpc: u64,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<u64, PublishError> {
        if !self.writable {
            return Err(PublishError::ReadOnly);
        }
        validate_format(f.width, f.height, f.subtype).map_err(PublishError::Format)?;
        if f.stride < min_stride(f.width, f.subtype) {
            return Err(PublishError::BadStride);
        }
        let size =
            frame_bytes(f.width, f.height, f.stride, f.subtype).ok_or(PublishError::BadStride)?;
        if size > SLOT_CAPACITY || (self.len as u64) < FILE_SIZE {
            return Err(PublishError::TooLarge);
        }
        let latest = self.a32(off::LATEST_SLOT).load(Ordering::Acquire);
        let in_use = self.a32(off::READER_SLOT).load(Ordering::Acquire);
        // three slots, at most two excluded: one is always free
        let slot = (0..SLOT_COUNT as u32)
            .find(|&i| i != latest && i != in_use)
            .unwrap_or(0) as usize;
        let so = slot_off(slot);
        let data_offset = HEADER_SIZE as u64 + slot as u64 * SLOT_CAPACITY;

        let seq = self.a64(so + off::SLOT_SEQ);
        let even = seq.load(Ordering::Relaxed) & !1;
        seq.store(even + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        self.a32(so + off::SLOT_WIDTH)
            .store(f.width, Ordering::Relaxed);
        self.a32(so + off::SLOT_HEIGHT)
            .store(f.height, Ordering::Relaxed);
        self.a32(so + off::SLOT_STRIDE)
            .store(f.stride, Ordering::Relaxed);
        self.a32(so + off::SLOT_SUBTYPE)
            .store(f.subtype, Ordering::Relaxed);
        self.a64(so + off::SLOT_CAPTURE_QPC)
            .store(capture_qpc, Ordering::Relaxed);
        self.a64(so + off::SLOT_DATA_OFFSET)
            .store(data_offset, Ordering::Relaxed);
        self.a64(so + off::SLOT_DATA_SIZE)
            .store(size, Ordering::Relaxed);
        // SAFETY: data_offset + size <= HEADER_SIZE + 3 * SLOT_CAPACITY = FILE_SIZE <= len (checked above).
        let dst = unsafe {
            std::slice::from_raw_parts_mut(self.base.add(data_offset as usize), size as usize)
        };
        fill(dst);
        seq.store(even + 2, Ordering::Release);
        self.a32(off::LATEST_SLOT)
            .store(slot as u32, Ordering::Release);
        self.a64(off::WRITER_FRAMES).fetch_add(1, Ordering::Relaxed);
        self.a64(off::WRITER_HEARTBEAT)
            .store(now_qpc, Ordering::Release);
        Ok(even + 2)
    }

    // ---- DLL side -------------------------------------------------------------------------------------------

    pub fn set_negotiated(&self, f: &Format) {
        if self.writable {
            self.store_format(off::NEGOTIATED, f);
        }
    }

    pub fn set_consumer_active(&self, active: bool) {
        if self.writable {
            self.a32(off::CONSUMER_ACTIVE)
                .store(active as u32, Ordering::Release);
        }
    }

    pub fn set_consumer_heartbeat(&self, now_qpc: u64) {
        if self.writable {
            self.a64(off::CONSUMER_HEARTBEAT)
                .store(now_qpc, Ordering::Release);
        }
    }

    pub fn set_stats(&self, p50_us: u32, p95_us: u32, delivered: u64, repeated: u64) {
        if self.writable {
            self.a32(off::LATENCY_P50_US)
                .store(p50_us, Ordering::Relaxed);
            self.a32(off::LATENCY_P95_US)
                .store(p95_us, Ordering::Relaxed);
            self.a64(off::DELIVERED).store(delivered, Ordering::Relaxed);
            self.a64(off::REPEATED).store(repeated, Ordering::Relaxed);
        }
    }

    /// Read the newest frame. `f` is called with the validated descriptor and the slot data after the descriptor was
    /// confirmed by an unchanged even `seq`; the sequence is checked again afterwards and the whole read retried
    /// once when it changed.
    pub fn read_latest<F>(&self, v: &ValidHeader, mut f: F) -> Result<Read, ReadError>
    where
        F: FnMut(&SlotMeta, &SlotData<'_>) -> Disposition,
    {
        let mut result = Err(ReadError::Torn);
        for _ in 0..2 {
            let latest = self.a32(off::LATEST_SLOT).load(Ordering::Acquire);
            if latest as usize >= SLOT_COUNT {
                result = Ok(Read::NoFrame);
                break;
            }
            let slot = latest as usize;
            let so = slot_off(slot);
            if self.writable {
                // hint for the writer: do not reuse this slot while we copy
                self.a32(off::READER_SLOT).store(latest, Ordering::SeqCst);
            }
            let seq = self.a64(so + off::SLOT_SEQ);
            let s1 = seq.load(Ordering::Acquire);
            if s1 == 0 {
                result = Ok(Read::NoFrame);
                break;
            }
            if s1 & 1 == 1 {
                continue;
            }
            let meta = self.slot_meta(slot);
            fence(Ordering::Acquire);
            if seq.load(Ordering::Relaxed) != s1 {
                continue;
            }
            let meta = SlotMeta { seq: s1, ..meta };
            if let Err(e) = validate_slot(&meta, v) {
                result = Err(ReadError::Invalid(e));
                break;
            }
            // validate_slot guarantees data_offset + data_size <= file_size <= len
            if meta.data_offset + meta.data_size > self.len as u64 {
                result = Err(ReadError::Invalid(SlotError::OutOfBounds));
                break;
            }
            let data = SlotData {
                // SAFETY: inside the mapping, see the check above.
                ptr: unsafe { self.base.add(meta.data_offset as usize) },
                len: meta.data_size as usize,
                _m: PhantomData,
            };
            let d = f(&meta, &data);
            fence(Ordering::Acquire);
            if seq.load(Ordering::Relaxed) != s1 {
                continue;
            }
            result = Ok(match d {
                Disposition::Taken => Read::Taken(meta),
                Disposition::Skip => Read::Skipped(meta),
            });
            break;
        }
        if self.writable {
            self.a32(off::READER_SLOT).store(NO_SLOT, Ordering::Release);
        }
        result
    }

    /// Overwrite a header u32 (tests: corrupt the file while a reader runs). Ignored on a read-only view.
    #[doc(hidden)]
    pub fn poke32(&self, offset: usize, value: u32) {
        if self.writable && offset % 4 == 0 && offset + 4 <= HEADER_SIZE {
            self.a32(offset).store(value, Ordering::SeqCst);
        }
    }

    /// Overwrite a header u64 (tests). Ignored on a read-only view.
    #[doc(hidden)]
    pub fn poke64(&self, offset: usize, value: u64) {
        if self.writable && offset % 8 == 0 && offset + 8 <= HEADER_SIZE {
            self.a64(offset).store(value, Ordering::SeqCst);
        }
    }

    /// Overwrite bytes anywhere in the view, e.g. frame data (tests: simulate a writer that is mid-frame). Ignored
    /// on a read-only view or when the range is outside the view.
    #[doc(hidden)]
    pub fn poke_bytes(&self, offset: usize, bytes: &[u8]) {
        if self.writable
            && offset
                .checked_add(bytes.len())
                .is_some_and(|e| e <= self.len)
        {
            // SAFETY: the range is inside the view (checked above) and the view is writable.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(offset), bytes.len())
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{SUBTYPE_NV12, SUBTYPE_RGB32};
    use crate::validate::validate_header;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Heap memory aligned for u64 atomics, big enough for a whole file.
    pub struct Heap {
        words: Vec<u64>,
    }
    impl Heap {
        pub fn new() -> Heap {
            Heap {
                words: vec![0u64; FILE_SIZE as usize / 8 + 1],
            }
        }
        pub fn shared(&mut self, writable: bool) -> Shared {
            unsafe {
                Shared::new(
                    self.words.as_mut_ptr() as *mut u8,
                    FILE_SIZE as usize,
                    writable,
                )
                .unwrap()
            }
        }
    }

    fn valid(s: &Shared) -> ValidHeader {
        validate_header(&s.header(), s.len() as u64, Some(1_000_000), None).unwrap()
    }

    fn fresh() -> (Heap, Shared) {
        let mut heap = Heap::new();
        let s = heap.shared(true);
        s.init_writer(42, 1_000_000, 777).unwrap();
        s.set_writer_state(WriterState::Live);
        (heap, s)
    }

    #[test]
    fn init_publishes_a_valid_empty_header() {
        let (_h, s) = fresh();
        let raw = s.header();
        assert_eq!((raw.magic, raw.protocol, raw.writer_pid), (MAGIC, 1, 42));
        assert_eq!(raw.latest_slot, NO_SLOT);
        let v = valid(&s);
        assert_eq!(v.state, WriterState::Live);
        let r = s.read_latest(&v, |_, _| panic!("no frame yet"));
        assert_eq!(r, Ok(Read::NoFrame));
    }

    #[test]
    fn small_buffers_and_misaligned_views_are_refused() {
        let mut heap = Heap::new();
        let base = heap.words.as_mut_ptr() as *mut u8;
        assert!(unsafe { Shared::new(base, HEADER_SIZE - 1, false) }.is_none());
        assert!(unsafe { Shared::new(base.add(4), HEADER_SIZE, false) }.is_none());
        assert!(unsafe { Shared::new(std::ptr::null_mut(), HEADER_SIZE, false) }.is_none());
    }

    #[test]
    fn publish_then_read_round_trips_and_rotates_slots() {
        let (_h, s) = fresh();
        let v = valid(&s);
        let fmt = SlotFormat {
            width: 64,
            height: 32,
            stride: 64,
            subtype: SUBTYPE_NV12,
        };
        let mut last_slot = u32::MAX;
        for n in 0..9u8 {
            s.publish(fmt, 1000 + n as u64, 5000, |d| {
                assert_eq!(d.len(), 64 * 32 * 3 / 2);
                d.fill(n)
            })
            .unwrap();
            let mut got = vec![0u8; 64 * 32 * 3 / 2];
            let r = s
                .read_latest(&v, |m, data| {
                    assert_eq!((m.width, m.height, m.subtype), (64, 32, SUBTYPE_NV12));
                    data.copy_to(0, &mut got);
                    Disposition::Taken
                })
                .unwrap();
            let Read::Taken(m) = r else { panic!("{r:?}") };
            assert!(got.iter().all(|&b| b == n));
            assert_eq!(m.capture_qpc, 1000 + n as u64);
            assert_ne!(m.index, last_slot, "the writer must move to another slot");
            last_slot = m.index;
        }
        assert_eq!(s.header().writer_frames, 9);
        assert_eq!(s.header().writer_heartbeat, 5000);
    }

    #[test]
    fn skip_is_reported_and_reader_slot_is_released() {
        let (_h, s) = fresh();
        let v = valid(&s);
        let fmt = SlotFormat {
            width: 16,
            height: 16,
            stride: 16,
            subtype: SUBTYPE_NV12,
        };
        s.publish(fmt, 1, 1, |d| d.fill(1)).unwrap();
        let r = s.read_latest(&v, |_, _| Disposition::Skip).unwrap();
        assert!(matches!(r, Read::Skipped(_)));
        assert_eq!(s.header().reader_slot, NO_SLOT);
    }

    #[test]
    fn writer_avoids_the_slot_the_reader_marked() {
        let (_h, s) = fresh();
        let fmt = SlotFormat {
            width: 16,
            height: 16,
            stride: 16,
            subtype: SUBTYPE_NV12,
        };
        s.publish(fmt, 1, 1, |_| {}).unwrap();
        let latest = s.header().latest_slot;
        let marked = (latest + 1) % 3;
        s.poke32(off::READER_SLOT, marked);
        for _ in 0..20 {
            let before = s.header().latest_slot;
            s.publish(fmt, 1, 1, |_| {}).unwrap();
            let now = s.header().latest_slot;
            assert_ne!(now, marked);
            assert_ne!(now, before);
        }
    }

    #[test]
    fn publish_rejects_bad_formats() {
        let (_h, s) = fresh();
        let ok = SlotFormat {
            width: 64,
            height: 64,
            stride: 64,
            subtype: SUBTYPE_NV12,
        };
        assert!(s.publish(ok, 0, 0, |_| {}).is_ok());
        let odd = SlotFormat { width: 63, ..ok };
        assert_eq!(
            s.publish(odd, 0, 0, |_| {}),
            Err(PublishError::Format(FormatError::OddSize))
        );
        let big = SlotFormat {
            width: 4098,
            height: 64,
            stride: 4098,
            subtype: SUBTYPE_NV12,
        };
        assert_eq!(
            s.publish(big, 0, 0, |_| {}),
            Err(PublishError::Format(FormatError::TooLarge))
        );
        let short = SlotFormat { stride: 10, ..ok };
        assert_eq!(s.publish(short, 0, 0, |_| {}), Err(PublishError::BadStride));
        // 4096x2304 NV12 is 14 MB: inside the width/height limits but not a format, because it does not fit a slot
        let huge = SlotFormat {
            width: 4096,
            height: 2304,
            stride: 4096,
            subtype: SUBTYPE_NV12,
        };
        assert_eq!(
            s.publish(huge, 0, 0, |_| {}),
            Err(PublishError::Format(FormatError::TooLarge))
        );
        // a legal size with a padded stride that no longer fits is caught by the slot check
        let padded = SlotFormat {
            width: 3840,
            height: 2160,
            stride: 4096,
            subtype: SUBTYPE_NV12,
        };
        assert_eq!(s.publish(padded, 0, 0, |_| {}), Err(PublishError::TooLarge));
        let rgb = SlotFormat {
            width: 1920,
            height: 1080,
            stride: 1920 * 4,
            subtype: SUBTYPE_RGB32,
        };
        assert!(s.publish(rgb, 0, 0, |_| {}).is_ok());
    }

    #[test]
    fn read_only_view_never_writes() {
        let (mut h, s) = fresh();
        let _ = s;
        let ro = h.shared(false);
        ro.set_negotiated(&Format::new(1, 2, 3, SUBTYPE_NV12));
        ro.set_consumer_active(true);
        ro.set_stats(1, 2, 3, 4);
        ro.poke32(off::MAGIC, 0);
        assert_eq!(ro.header().negotiated, Format::default());
        assert_eq!(ro.header().magic, MAGIC);
        let fmt = SlotFormat {
            width: 16,
            height: 16,
            stride: 16,
            subtype: SUBTYPE_NV12,
        };
        assert_eq!(ro.publish(fmt, 0, 0, |_| {}), Err(PublishError::ReadOnly));
        assert_eq!(ro.init_writer(1, 1, 1), Err(PublishError::ReadOnly));
    }

    #[test]
    fn corrupted_slot_descriptors_are_invalid_not_dangerous() {
        let (_h, s) = fresh();
        let v = valid(&s);
        let fmt = SlotFormat {
            width: 64,
            height: 64,
            stride: 64,
            subtype: SUBTYPE_NV12,
        };
        s.publish(fmt, 1, 1, |d| d.fill(9)).unwrap();
        let slot = s.header().latest_slot as usize;
        let so = slot_off(slot);
        // offset past the end of the file
        s.poke64(so + off::SLOT_DATA_OFFSET, FILE_SIZE - 10);
        let r = s.read_latest(&v, |_, _| panic!("must not reach the callback"));
        assert_eq!(r, Err(ReadError::Invalid(SlotError::OutOfBounds)));
        // odd width
        s.poke64(
            so + off::SLOT_DATA_OFFSET,
            HEADER_SIZE as u64 + slot as u64 * SLOT_CAPACITY,
        );
        s.poke32(so + off::SLOT_WIDTH, 63);
        let r = s.read_latest(&v, |_, _| panic!("must not reach the callback"));
        assert_eq!(
            r,
            Err(ReadError::Invalid(SlotError::Format(FormatError::OddSize)))
        );
        // an odd seq looks like a write in progress: torn, not invalid
        s.poke32(so + off::SLOT_WIDTH, 64);
        s.poke64(so + off::SLOT_SEQ, 3);
        let r = s.read_latest(&v, |_, _| panic!("must not reach the callback"));
        assert_eq!(r, Err(ReadError::Torn));
        assert_eq!(s.header().reader_slot, NO_SLOT);
    }

    /// Words of a frame all hold the frame number: a torn frame mixes two numbers.
    fn check_frame(buf: &[u8]) -> u64 {
        let first = u64::from_le_bytes(buf[..8].try_into().unwrap());
        for c in buf.chunks_exact(8) {
            assert_eq!(
                u64::from_le_bytes(c.try_into().unwrap()),
                first,
                "torn frame"
            );
        }
        first
    }

    fn run_pair(duration: Duration, writer_period: Option<Duration>) -> (u64, u64, u64) {
        let mut heap = Heap::new();
        let s = Arc::new(heap.shared(true));
        s.init_writer(1, 1_000_000, 0).unwrap();
        s.set_writer_state(WriterState::Live);
        let v = valid(&s);
        let stop = Arc::new(AtomicBool::new(false));
        // 128x64 NV12: 12 KB, every u64 equals the frame number
        let fmt = SlotFormat {
            width: 128,
            height: 64,
            stride: 128,
            subtype: SUBTYPE_NV12,
        };
        let size = 128 * 64 * 3 / 2;

        let w = {
            let (s, stop) = (s.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut n = 1u64;
                let began = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    s.publish(fmt, n, n, |d| {
                        for c in d.chunks_exact_mut(8) {
                            c.copy_from_slice(&n.to_le_bytes());
                        }
                    })
                    .unwrap();
                    n += 1;
                    match writer_period {
                        Some(p) => {
                            // absolute schedule: sleep until the next 1 ms boundary
                            let due = began + p * n as u32;
                            while Instant::now() < due {
                                std::hint::spin_loop();
                            }
                        }
                        None => std::thread::yield_now(),
                    }
                }
                n - 1
            })
        };
        let (reads, torn, frames) = {
            let mut buf = vec![0u8; size];
            let (mut reads, mut torn, mut frames) = (0u64, 0u64, 0u64);
            let mut last = 0u64;
            let began = Instant::now();
            while began.elapsed() < duration {
                match s.read_latest(&v, |_, data| {
                    data.copy_to(0, &mut buf);
                    Disposition::Taken
                }) {
                    Ok(Read::Taken(_)) => {
                        let n = check_frame(&buf);
                        assert!(n >= last, "frame numbers went backwards: {last} -> {n}");
                        if n != last {
                            frames += 1;
                        }
                        last = n;
                        reads += 1;
                    }
                    Ok(_) => {}
                    Err(ReadError::Torn) => torn += 1,
                    Err(e) => panic!("{e:?}"),
                }
            }
            (reads, torn, frames)
        };
        stop.store(true, Ordering::Relaxed);
        let written = w.join().unwrap();
        assert!(frames > 0 && written > 0);
        (written, reads + torn, frames)
    }

    #[test]
    fn writer_at_1khz_and_reader_see_no_torn_frames_for_2_seconds() {
        let (written, reads, distinct) =
            run_pair(Duration::from_secs(2), Some(Duration::from_millis(1)));
        eprintln!(
            "1 kHz: written {written}, reader attempts {reads}, distinct frames seen {distinct}"
        );
        assert!(written >= 1500, "writer ran at {written} frames in 2 s");
        assert!(
            distinct >= 1000,
            "reader only saw {distinct} distinct frames"
        );
    }

    #[test]
    fn unthrottled_writer_never_produces_a_torn_frame() {
        let (written, reads, distinct) = run_pair(Duration::from_millis(1000), None);
        eprintln!("unthrottled: written {written}, reader attempts {reads}, distinct frames seen {distinct}");
        assert!(written > 10_000);
    }
}
