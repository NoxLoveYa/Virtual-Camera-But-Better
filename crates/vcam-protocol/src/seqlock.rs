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
    off, slot_off, Format, RawHeader, SlotMeta, WriterState, ADVERTISED_MAX, FEATURE_FRAME_EVENT,
    FILE_SIZE, HEADER_SIZE, NO_SLOT, SLOT_CAPACITY, SLOT_COUNT,
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
    /// The [`crate::Writer`] was retired ([`crate::Writer::retire`]): it no longer writes to the file.
    Retired,
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
    #[allow(clippy::manual_is_multiple_of)] // MSRV 1.87: unsigned `is_multiple_of` is stable only from 1.90
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

    #[allow(clippy::manual_is_multiple_of)] // MSRV 1.87: unsigned `is_multiple_of` is stable only from 1.90
    fn a32(&self, off: usize) -> &AtomicU32 {
        debug_assert!(off % 4 == 0 && off + 4 <= HEADER_SIZE);
        // SAFETY: `off` is a header offset (< HEADER_SIZE <= len), 4-aligned from an 8-aligned base.
        unsafe { AtomicU32::from_ptr(self.base.add(off) as *mut u32) }
    }
    #[allow(clippy::manual_is_multiple_of)] // MSRV 1.87: unsigned `is_multiple_of` is stable only from 1.90
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

    /// `(signal_seq, writer_frames)`, `signal_seq` first, each loaded with acquire ordering: the two counters a pump
    /// that waits on the frame event compares after a wake-up, without loading the whole header. The order is the same
    /// as in [`Shared::header`] (see `RawHeader::load`).
    pub fn signal_counters(&self) -> (u64, u64) {
        let seq = self.a64(off::SIGNAL_SEQ).load(Ordering::Acquire);
        let frames = self.a64(off::WRITER_FRAMES).load(Ordering::Acquire);
        (seq, frames)
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
    ///
    /// The whole header is zeroed, the frame event fields (`features`, both nonces, `signal_seq` and the DLL's `wake_*`
    /// diagnostics, bytes 448..528) included, in this version and in every older one alike: an older app that starts
    /// over the file of a newer one clears the advert, so a reader falls back to polling. A newer app advertises its
    /// events afterwards ([`Shared::advertise_frame_event`]).
    pub fn init_writer(
        &self,
        pid: u32,
        qpc_frequency: u64,
        now_qpc: u64,
    ) -> Result<(), PublishError> {
        self.init_writer_with(pid, qpc_frequency, now_qpc, &|| {})
    }

    /// [`init_writer`](Self::init_writer); `before_wipe` runs after the advert was withdrawn and before the header is
    /// wiped (a test looks at the file there).
    fn init_writer_with(
        &self,
        pid: u32,
        qpc_frequency: u64,
        now_qpc: u64,
        before_wipe: &dyn Fn(),
    ) -> Result<(), PublishError> {
        if !self.writable {
            return Err(PublishError::ReadOnly);
        }
        if (self.len as u64) < FILE_SIZE {
            return Err(PublishError::TooLarge);
        }
        self.a32(off::MAGIC).store(0, Ordering::Release);
        // The stores of the wipe below (relaxed atomics and a plain `write_bytes`) may become visible in any order on a
        // weakly ordered CPU (ARM64). A reader that loads a nonce, `features` and the nonce again could then see a
        // half-wiped nonce (`hi` already 0, `lo` still the old value: a different, non-zero nonce) next to the old
        // `features`, and accept it. With `features` cleared and released first, a reader that sees any wiped byte sees
        // `features == 0` after it. (The nonce at 512..528 is wiped by the same loop and is covered the same way.)
        self.a32(off::FEATURES).store(0, Ordering::Relaxed);
        fence(Ordering::Release);
        before_wipe();
        // The frame event fields are read through atomics by readers that do not look at `magic` first (the DLL polls
        // them while the file is being started over), so they are wiped with atomic stores, not by the plain
        // `write_bytes` below (a non-atomic write that races an atomic read is a data race). Every location is wiped
        // with an atomic of the size it is accessed with everywhere else: `features` (448) and its padding (452),
        // `wake_mode` (480) and its padding (484) are 32-bit fields (two 32-bit stores each, never one 64-bit store over
        // both: a mixed-size access to the same bytes is not defined for atomics); the nonces, `signal_seq` and the
        // counters are 64-bit.
        for o in [
            off::FEATURES,
            off::FEATURES + 4,
            off::WAKE_MODE,
            off::WAKE_MODE + 4,
        ] {
            self.a32(o).store(0, Ordering::Relaxed);
        }
        for o in [
            off::EVENT_NONCE_LO,
            off::EVENT_NONCE_HI,
            off::SIGNAL_SEQ,
            off::WAKE_EVENTS,
            off::WAKE_MISSED,
            off::WAKE_SPURIOUS,
            off::EVENT_NONCE1_LO,
            off::EVENT_NONCE1_HI,
        ] {
            self.a64(o).store(0, Ordering::Relaxed);
        }
        // SAFETY: the rest of the header is inside the mapping; atomics are not used on these bytes while we are the
        // only initialiser (readers see magic == 0 and stay away).
        unsafe {
            std::ptr::write_bytes(self.base.add(4), 0, off::FEATURES - 4);
            std::ptr::write_bytes(
                self.base.add(off::EVENT_NONCE1_HI + 8),
                0,
                HEADER_SIZE - (off::EVENT_NONCE1_HI + 8),
            );
        }
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

    /// Forget the newest frame: `latest_slot` becomes [`NO_SLOT`], so a reader that attaches finds no frame until the
    /// next [`publish`](Self::publish). The slots keep their data (a reader that is copying one is not disturbed) and
    /// `writer_frames` is untouched. The engine calls it while nobody reads the camera, so the picture of an earlier
    /// session (another camera, an older filter chain) is never what a new app gets first. Ignored on a read-only view.
    pub fn clear_latest(&self) {
        if self.writable {
            self.a32(off::LATEST_SLOT).store(NO_SLOT, Ordering::Release);
        }
    }

    /// Tell readers that the frame events named by `nonces` (`nonces[i]` names event `.i`) exist
    /// ([`crate::signal`]): the nonces first, `features` last with release ordering, so a reader that sees the bit
    /// finds the nonces. `signal_seq` is not touched: it starts at 0 with [`init_writer`](Self::init_writer). Ignored
    /// on a read-only view. A zero nonce is not an advert and clears the bit instead.
    pub fn advertise_frame_event(&self, nonces: [u128; 2]) {
        if !self.writable {
            return;
        }
        if nonces.contains(&0) {
            self.clear_features();
            return;
        }
        // An advert that is already up (another call on a live file) is taken down first: a reader that sees any of the
        // new nonce bytes then sees `features == 0` after them, as with `init_writer`, never the old bit with a mix.
        self.a32(off::FEATURES).store(0, Ordering::Relaxed);
        fence(Ordering::Release);
        self.a64(off::EVENT_NONCE_LO)
            .store(nonces[0] as u64, Ordering::Release);
        self.a64(off::EVENT_NONCE_HI)
            .store((nonces[0] >> 64) as u64, Ordering::Release);
        self.a64(off::EVENT_NONCE1_LO)
            .store(nonces[1] as u64, Ordering::Release);
        self.a64(off::EVENT_NONCE1_HI)
            .store((nonces[1] >> 64) as u64, Ordering::Release);
        self.a32(off::FEATURES)
            .store(FEATURE_FRAME_EVENT, Ordering::Release);
    }

    /// Withdraw the advert: `features` becomes 0 (the nonces stay; readers need the bit).
    pub fn clear_features(&self) {
        if self.writable {
            self.a32(off::FEATURES).store(0, Ordering::Release);
        }
    }

    /// Store the count of signals ([`crate::signal`] calls this between the reset and the set of an event).
    pub fn set_signal_seq(&self, n: u64) {
        if self.writable {
            self.a64(off::SIGNAL_SEQ).store(n, Ordering::Release);
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

    /// The DLL's diagnostics of how the pump waits (`wake_mode` of [`crate::header::wake_mode`] and three counters).
    /// Ignored on a read-only view. The caller writes them only while the header is valid, like the statistics.
    pub fn set_wake_stats(&self, mode: u32, events: u64, missed: u64, spurious: u64) {
        if self.writable {
            self.a32(off::WAKE_MODE).store(mode, Ordering::Relaxed);
            self.a64(off::WAKE_EVENTS).store(events, Ordering::Relaxed);
            self.a64(off::WAKE_MISSED).store(missed, Ordering::Relaxed);
            self.a64(off::WAKE_SPURIOUS)
                .store(spurious, Ordering::Relaxed);
        }
    }

    /// Read the newest frame. `f` is called with the validated descriptor and the slot data after the descriptor was
    /// confirmed by an unchanged even `seq`; the sequence is checked again afterwards and the whole read retried
    /// once when it changed.
    pub fn read_latest<F>(&self, v: &ValidHeader, f: F) -> Result<Read, ReadError>
    where
        F: FnMut(&SlotMeta, &SlotData<'_>) -> Disposition,
    {
        self.read_impl(None, v, f)
    }

    /// Like [`read_latest`](Self::read_latest) for slot `slot` instead of the newest one (a repeat of the frame
    /// that was shown last, which sits in a slot that is no longer the newest when the writer sent something the
    /// reader does not use). Same rules: the descriptor is validated, `f` runs only on a stable even `seq`, and the
    /// result is `Taken` only when `seq` is unchanged after `f` returned. A slot the writer has not filled yet is
    /// `NoFrame`; a `slot` past the last one is `NoFrame` too.
    pub fn read_slot<F>(&self, slot: usize, v: &ValidHeader, f: F) -> Result<Read, ReadError>
    where
        F: FnMut(&SlotMeta, &SlotData<'_>) -> Disposition,
    {
        self.read_impl(Some(slot.min(SLOT_COUNT) as u32), v, f)
    }

    fn read_impl<F>(&self, fixed: Option<u32>, v: &ValidHeader, mut f: F) -> Result<Read, ReadError>
    where
        F: FnMut(&SlotMeta, &SlotData<'_>) -> Disposition,
    {
        let mut result = Err(ReadError::Torn);
        for _ in 0..2 {
            let index = match fixed {
                Some(s) => s,
                None => self.a32(off::LATEST_SLOT).load(Ordering::Acquire),
            };
            if index as usize >= SLOT_COUNT {
                result = Ok(Read::NoFrame);
                break;
            }
            let slot = index as usize;
            let so = slot_off(slot);
            if self.writable {
                // hint for the writer: do not reuse this slot while we copy
                self.a32(off::READER_SLOT).store(index, Ordering::SeqCst);
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
    #[allow(clippy::manual_is_multiple_of)] // MSRV 1.87: unsigned `is_multiple_of` is stable only from 1.90
    pub fn poke32(&self, offset: usize, value: u32) {
        if self.writable && offset % 4 == 0 && offset + 4 <= HEADER_SIZE {
            self.a32(offset).store(value, Ordering::SeqCst);
        }
    }

    /// Overwrite a header u64 (tests). Ignored on a read-only view.
    #[doc(hidden)]
    #[allow(clippy::manual_is_multiple_of)] // MSRV 1.87: unsigned `is_multiple_of` is stable only from 1.90
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
    fn init_zeroes_the_frame_event_bytes_and_the_advert_round_trips() {
        let mut heap = Heap::new();
        let s = heap.shared(true);
        // an earlier writer left an advert and the DLL left diagnostics
        s.init_writer(1, 1_000_000, 1).unwrap();
        s.advertise_frame_event([
            0xAAAA_BBBB_CCCC_DDDD_1111_2222_3333_4444,
            0x5555_6666_7777_8888_9999_0000_1234_5678,
        ]);
        s.set_signal_seq(9);
        s.set_wake_stats(1, 2, 3, 4);
        let h = s.header();
        assert_eq!(h.features, FEATURE_FRAME_EVENT);
        assert_eq!(
            (h.event_nonce_lo, h.event_nonce_hi),
            (0x1111_2222_3333_4444, 0xAAAA_BBBB_CCCC_DDDD)
        );
        assert_eq!(
            (h.event_nonce1_lo, h.event_nonce1_hi),
            (0x9999_0000_1234_5678, 0x5555_6666_7777_8888)
        );
        assert_eq!(
            (h.event_nonce_lo_again, h.event_nonce_hi_again),
            (h.event_nonce_lo, h.event_nonce_hi)
        );
        assert_eq!(
            (h.event_nonce1_lo_again, h.event_nonce1_hi_again),
            (h.event_nonce1_lo, h.event_nonce1_hi)
        );
        assert_eq!((h.signal_seq, h.wake_mode), (9, 1));
        assert_eq!((h.wake_events, h.wake_missed, h.wake_spurious), (2, 3, 4));
        // the second nonce sits at 512..528
        assert_eq!(off::EVENT_NONCE1_LO, 512);
        assert_eq!(off::EVENT_NONCE1_HI, 520);
        // a new init (this version's, and the old one's was the same `write_bytes`) clears 448..528
        for o in (off::FEATURES..528).step_by(8) {
            s.poke64(o, u64::MAX);
        }
        s.init_writer(2, 1_000_000, 2).unwrap();
        let h = s.header();
        assert_eq!((h.features, h.signal_seq, h.wake_mode), (0, 0, 0));
        assert_eq!((h.event_nonce_lo, h.event_nonce_hi), (0, 0));
        assert_eq!((h.event_nonce1_lo, h.event_nonce1_hi), (0, 0));
        assert_eq!((h.wake_events, h.wake_missed, h.wake_spurious), (0, 0, 0));
        assert_eq!(valid(&s).wake, None);
        // the advert is valid again after an advertise; clearing keeps the nonces but drops the bit
        s.advertise_frame_event([5, 6]);
        assert_eq!(valid(&s).wake.map(|w| w.nonces), Some([5, 6]));
        s.clear_features();
        assert_eq!(valid(&s).wake, None);
        assert_eq!(s.header().event_nonce_lo, 5);
        assert_eq!(s.header().event_nonce1_lo, 6);
        // a zero nonce, in either place, is no advert
        s.advertise_frame_event([7, 8]);
        s.advertise_frame_event([0, 8]);
        assert_eq!(valid(&s).wake, None);
        s.advertise_frame_event([7, 8]);
        s.advertise_frame_event([7, 0]);
        assert_eq!(valid(&s).wake, None);
    }

    #[test]
    fn init_withdraws_the_advert_before_it_wipes_the_nonces() {
        // On a weakly ordered CPU the wipe's plain stores can become visible in any order. `features` has to be 0 (and
        // released) before the first of them, or a reader could accept a half-wiped nonce next to the old bit.
        let mut heap = Heap::new();
        let s = heap.shared(true);
        s.init_writer(1, 1_000_000, 1).unwrap();
        s.advertise_frame_event([0x11, 0x22]);
        assert_eq!(s.header().features, FEATURE_FRAME_EVENT);
        let looked = std::cell::Cell::new(false);
        s.init_writer_with(2, 1_000_000, 2, &|| {
            let h = s.header();
            assert_eq!(
                h.features, 0,
                "the advert was not withdrawn before the wipe"
            );
            assert_eq!(
                (h.event_nonce_lo, h.event_nonce1_lo),
                (0x11, 0x22),
                "the nonces were wiped before the hook"
            );
            looked.set(true);
        })
        .unwrap();
        assert!(looked.get());
        assert_eq!(s.header().event_nonce_lo, 0);
    }

    #[test]
    fn a_read_only_view_cannot_touch_the_frame_event_bytes() {
        let mut heap = Heap::new();
        let w = heap.shared(true);
        w.init_writer(1, 1_000_000, 1).unwrap();
        w.advertise_frame_event([3, 30]);
        w.set_signal_seq(4);
        let ro = heap.shared(false);
        ro.advertise_frame_event([99, 98]);
        ro.clear_features();
        ro.set_signal_seq(100);
        ro.set_wake_stats(1, 1, 1, 1);
        let h = ro.header();
        assert_eq!((h.features, h.event_nonce_lo, h.signal_seq), (1, 3, 4));
        assert_eq!(h.event_nonce1_lo, 30);
        assert_eq!((h.wake_mode, h.wake_events), (0, 0));
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
    fn a_chosen_slot_is_read_with_the_same_rules_as_the_newest() {
        let (_h, s) = fresh();
        let v = valid(&s);
        let fmt = SlotFormat {
            width: 16,
            height: 16,
            stride: 16,
            subtype: SUBTYPE_NV12,
        };
        // nothing published: no frame, whichever slot is asked for
        assert_eq!(
            s.read_slot(0, &v, |_, _| panic!("no frame")),
            Ok(Read::NoFrame)
        );
        assert_eq!(
            s.read_slot(7, &v, |_, _| panic!("no slot")),
            Ok(Read::NoFrame)
        );
        s.publish(fmt, 11, 1, |d| d.fill(1)).unwrap();
        let first = s.header().latest_slot as usize;
        s.publish(fmt, 22, 2, |d| d.fill(2)).unwrap();
        let second = s.header().latest_slot as usize;
        assert_ne!(first, second);
        // the older slot still holds the older frame
        let mut got = vec![0u8; 16 * 24];
        let r = s
            .read_slot(first, &v, |m, data| {
                assert_eq!((m.index as usize, m.capture_qpc), (first, 11));
                data.copy_to(0, &mut got);
                Disposition::Taken
            })
            .unwrap();
        assert!(matches!(r, Read::Taken(m) if m.index as usize == first));
        assert!(got.iter().all(|&b| b == 1));
        assert_eq!(s.header().reader_slot, NO_SLOT, "the mark is released");
        // while it is read, the writer is told to keep away from it
        let mut marked = None;
        s.read_slot(first, &v, |_, _| {
            marked = Some(s.header().reader_slot);
            Disposition::Skip
        })
        .unwrap();
        assert_eq!(marked, Some(first as u32));
        // a slot the writer has not filled yet
        let empty = (0..3).find(|&i| i != first && i != second).unwrap();
        assert_eq!(
            s.read_slot(empty, &v, |_, _| panic!("empty")),
            Ok(Read::NoFrame)
        );
        // the writer reuses the slot while the callback runs, on the retry too: the copy is rejected
        let so = slot_off(first);
        let mut seq = 100;
        let r = s.read_slot(first, &v, |_, _| {
            seq += 2;
            s.poke64(so + off::SLOT_SEQ, seq);
            Disposition::Taken
        });
        assert_eq!(r, Err(ReadError::Torn));
        // a write in progress (odd seq) is torn too, and a damaged descriptor is invalid
        s.poke64(so + off::SLOT_SEQ, 5);
        assert_eq!(
            s.read_slot(first, &v, |_, _| panic!("odd")),
            Err(ReadError::Torn)
        );
        s.poke64(so + off::SLOT_SEQ, 6);
        s.poke32(so + off::SLOT_WIDTH, 15);
        assert!(matches!(
            s.read_slot(first, &v, |_, _| panic!("invalid")),
            Err(ReadError::Invalid(_))
        ));
        assert_eq!(s.header().reader_slot, NO_SLOT);
    }

    #[test]
    fn clearing_the_latest_frame_leaves_a_new_reader_nothing_until_the_next_publish() {
        let (_h, s) = fresh();
        let v = valid(&s);
        let fmt = SlotFormat {
            width: 16,
            height: 16,
            stride: 16,
            subtype: SUBTYPE_NV12,
        };
        s.publish(fmt, 11, 1, |d| d.fill(1)).unwrap();
        s.publish(fmt, 22, 2, |d| d.fill(2)).unwrap();
        assert_ne!(s.header().latest_slot, NO_SLOT);
        let frames = s.header().writer_frames;
        s.clear_latest();
        let h = s.header();
        assert_eq!(h.latest_slot, NO_SLOT);
        assert_eq!(
            h.writer_frames, frames,
            "the count of published frames stays"
        );
        assert_eq!(h.writer_state, WriterState::Live as u32, "the state stays");
        // a reader that attaches now finds nothing, and the callback never runs
        assert_eq!(
            s.read_latest(&v, |_, _| panic!("an old frame was offered")),
            Ok(Read::NoFrame)
        );
        // the next publish is what it gets
        s.publish(fmt, 33, 3, |d| d.fill(3)).unwrap();
        let mut seen = None;
        let r = s
            .read_latest(&v, |m, data| {
                let mut b = [0u8; 1];
                data.copy_to(0, &mut b);
                seen = Some((m.capture_qpc, b[0]));
                Disposition::Taken
            })
            .unwrap();
        assert!(matches!(r, Read::Taken(_)));
        assert_eq!(seen, Some((33, 3)));
        // clearing twice is harmless, and publishing goes on after it
        s.clear_latest();
        s.clear_latest();
        assert_eq!(s.header().latest_slot, NO_SLOT);
        s.publish(fmt, 44, 4, |d| d.fill(4)).unwrap();
        assert_ne!(s.header().latest_slot, NO_SLOT);
    }

    #[test]
    fn a_read_only_view_cannot_clear_the_latest_frame() {
        // the same bytes seen through a read-only view: poke through the writable one
        let mut heap = Heap::new();
        let w = heap.shared(true);
        w.init_writer(1, 1_000_000, 1).unwrap();
        let fmt = SlotFormat {
            width: 16,
            height: 16,
            stride: 16,
            subtype: SUBTYPE_NV12,
        };
        w.publish(fmt, 1, 1, |d| d.fill(9)).unwrap();
        let slot = w.header().latest_slot;
        let ro = heap.shared(false);
        ro.clear_latest();
        assert_eq!(
            ro.header().latest_slot,
            slot,
            "a read-only view never stores"
        );
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
