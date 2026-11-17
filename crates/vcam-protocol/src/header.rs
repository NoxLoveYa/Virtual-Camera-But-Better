//! Layout of `frames.bin` (plan 2.5): a 4096-byte header followed by three frame slots.
//!
//! All integers are little-endian. Fields are accessed by byte offset (see [`off`]); the `#[repr(C)]` mirror at the
//! bottom of this file exists so the compiler checks those offsets against a normal struct layout.

use core::mem::{offset_of, size_of};

/// `'BCAM'` as a little-endian u32.
pub const MAGIC: u32 = u32::from_le_bytes(*b"BCAM");
/// Bytes of the header; the first slot's data starts here.
pub const HEADER_SIZE: usize = 4096;
/// Number of frame slots.
pub const SLOT_COUNT: usize = 3;
/// Entries in the advertised media type list.
pub const ADVERTISED_MAX: usize = 8;
/// `latest_slot` / `reader_slot` value for "none".
pub const NO_SLOT: u32 = u32::MAX;

/// Largest frame the protocol accepts.
pub const MAX_WIDTH: u32 = 4096;
pub const MAX_HEIGHT: u32 = 2304;
/// RGB32 is only advertised and accepted up to 1080p.
pub const RGB32_MAX_WIDTH: u32 = 1920;
pub const RGB32_MAX_HEIGHT: u32 = 1080;

const fn round_up(v: u64, align: u64) -> u64 {
    v.div_ceil(align) * align
}
/// Bytes reserved for one slot: a 3840x2160 NV12 frame rounded up to 64 KB (12.5 MB).
pub const SLOT_CAPACITY: u64 = round_up(3840 * 2160 * 3 / 2, 64 * 1024);
/// Size the writer gives the file.
pub const FILE_SIZE: u64 = HEADER_SIZE as u64 + SLOT_COUNT as u64 * SLOT_CAPACITY;
/// Upper bound the reader accepts for a mapped file (a hostile or foreign file is rejected before mapping).
pub const MAX_FILE_SIZE: u64 = 256 * 1024 * 1024;
/// Heartbeat older than this means "writer gone".
pub const HEARTBEAT_TIMEOUT_MS: u64 = 1000;

/// `subtype`: NV12, BT.709 limited range. Y plane then interleaved UV, both with the slot's stride.
pub const SUBTYPE_NV12: u32 = u32::from_le_bytes(*b"NV12");
/// `subtype`: BGRX (`MFVideoFormat_RGB32`), top-down.
pub const SUBTYPE_RGB32: u32 = u32::from_le_bytes(*b"BGRX");

/// Byte offsets of the header fields.
pub mod off {
    pub const MAGIC: usize = 0;
    pub const PROTOCOL: usize = 4;
    pub const HEADER_SIZE: usize = 8;
    pub const SLOT_COUNT: usize = 12;
    pub const FILE_SIZE: usize = 16;
    pub const SLOT_CAPACITY: usize = 24;
    pub const WRITER_PID: usize = 32;
    pub const WRITER_STATE: usize = 36;
    pub const WRITER_HEARTBEAT: usize = 40;
    pub const QPC_FREQUENCY: usize = 48;
    pub const WRITER_FRAMES: usize = 56;
    pub const ADVERTISED_COUNT: usize = 64;
    pub const ADVERTISED: usize = 72;
    // -- written by the DLL --
    pub const CONSUMER_ACTIVE: usize = 232;
    pub const READER_SLOT: usize = 236;
    pub const CONSUMER_HEARTBEAT: usize = 240;
    pub const NEGOTIATED: usize = 248;
    pub const LATENCY_P50_US: usize = 268;
    pub const LATENCY_P95_US: usize = 272;
    pub const DELIVERED: usize = 280;
    pub const REPEATED: usize = 288;
    // -- slots --
    pub const LATEST_SLOT: usize = 296;
    pub const SLOTS: usize = 304;

    /// Offsets inside one [`Format`](super::Format).
    pub const FMT_WIDTH: usize = 0;
    pub const FMT_HEIGHT: usize = 4;
    pub const FMT_FPS_NUM: usize = 8;
    pub const FMT_FPS_DEN: usize = 12;
    pub const FMT_SUBTYPE: usize = 16;
    pub const FMT_SIZE: usize = 20;

    /// Offsets inside one slot descriptor.
    pub const SLOT_SEQ: usize = 0;
    pub const SLOT_WIDTH: usize = 8;
    pub const SLOT_HEIGHT: usize = 12;
    pub const SLOT_STRIDE: usize = 16;
    pub const SLOT_SUBTYPE: usize = 20;
    pub const SLOT_CAPTURE_QPC: usize = 24;
    pub const SLOT_DATA_OFFSET: usize = 32;
    pub const SLOT_DATA_SIZE: usize = 40;
    pub const SLOT_SIZE: usize = 48;
}

/// Offset of slot descriptor `i`.
pub const fn slot_off(i: usize) -> usize {
    off::SLOTS + i * off::SLOT_SIZE
}

#[repr(C)]
struct FormatLayout {
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
    subtype: u32,
}

#[repr(C)]
struct SlotLayout {
    seq: u64,
    width: u32,
    height: u32,
    stride: u32,
    subtype: u32,
    capture_qpc: u64,
    data_offset: u64,
    data_size: u64,
}

#[repr(C)]
struct HeaderLayout {
    magic: u32,
    protocol: u32,
    header_size: u32,
    slot_count: u32,
    file_size: u64,
    slot_capacity: u64,
    writer_pid: u32,
    writer_state: u32,
    writer_heartbeat: u64,
    qpc_frequency: u64,
    writer_frames: u64,
    advertised_count: u32,
    reserved0: u32,
    advertised: [FormatLayout; ADVERTISED_MAX],
    consumer_active: u32,
    reader_slot: u32,
    consumer_heartbeat: u64,
    negotiated: FormatLayout,
    latency_p50_us: u32,
    latency_p95_us: u32,
    reserved1: u32,
    delivered: u64,
    repeated: u64,
    latest_slot: u32,
    reserved2: u32,
    slots: [SlotLayout; SLOT_COUNT],
}

// The offsets above are the contract; the compiler checks them against the struct.
const _: () = {
    assert!(offset_of!(HeaderLayout, magic) == off::MAGIC);
    assert!(offset_of!(HeaderLayout, protocol) == off::PROTOCOL);
    assert!(offset_of!(HeaderLayout, header_size) == off::HEADER_SIZE);
    assert!(offset_of!(HeaderLayout, slot_count) == off::SLOT_COUNT);
    assert!(offset_of!(HeaderLayout, file_size) == off::FILE_SIZE);
    assert!(offset_of!(HeaderLayout, slot_capacity) == off::SLOT_CAPACITY);
    assert!(offset_of!(HeaderLayout, writer_pid) == off::WRITER_PID);
    assert!(offset_of!(HeaderLayout, writer_state) == off::WRITER_STATE);
    assert!(offset_of!(HeaderLayout, writer_heartbeat) == off::WRITER_HEARTBEAT);
    assert!(offset_of!(HeaderLayout, qpc_frequency) == off::QPC_FREQUENCY);
    assert!(offset_of!(HeaderLayout, writer_frames) == off::WRITER_FRAMES);
    assert!(offset_of!(HeaderLayout, advertised_count) == off::ADVERTISED_COUNT);
    assert!(offset_of!(HeaderLayout, advertised) == off::ADVERTISED);
    assert!(offset_of!(HeaderLayout, consumer_active) == off::CONSUMER_ACTIVE);
    assert!(offset_of!(HeaderLayout, reader_slot) == off::READER_SLOT);
    assert!(offset_of!(HeaderLayout, consumer_heartbeat) == off::CONSUMER_HEARTBEAT);
    assert!(offset_of!(HeaderLayout, negotiated) == off::NEGOTIATED);
    assert!(offset_of!(HeaderLayout, latency_p50_us) == off::LATENCY_P50_US);
    assert!(offset_of!(HeaderLayout, latency_p95_us) == off::LATENCY_P95_US);
    assert!(offset_of!(HeaderLayout, delivered) == off::DELIVERED);
    assert!(offset_of!(HeaderLayout, repeated) == off::REPEATED);
    assert!(offset_of!(HeaderLayout, latest_slot) == off::LATEST_SLOT);
    assert!(offset_of!(HeaderLayout, slots) == off::SLOTS);
    assert!(size_of::<FormatLayout>() == off::FMT_SIZE);
    assert!(offset_of!(FormatLayout, height) == off::FMT_HEIGHT);
    assert!(offset_of!(FormatLayout, fps_num) == off::FMT_FPS_NUM);
    assert!(offset_of!(FormatLayout, fps_den) == off::FMT_FPS_DEN);
    assert!(offset_of!(FormatLayout, subtype) == off::FMT_SUBTYPE);
    assert!(size_of::<SlotLayout>() == off::SLOT_SIZE);
    assert!(offset_of!(SlotLayout, width) == off::SLOT_WIDTH);
    assert!(offset_of!(SlotLayout, height) == off::SLOT_HEIGHT);
    assert!(offset_of!(SlotLayout, stride) == off::SLOT_STRIDE);
    assert!(offset_of!(SlotLayout, subtype) == off::SLOT_SUBTYPE);
    assert!(offset_of!(SlotLayout, capture_qpc) == off::SLOT_CAPTURE_QPC);
    assert!(offset_of!(SlotLayout, data_offset) == off::SLOT_DATA_OFFSET);
    assert!(offset_of!(SlotLayout, data_size) == off::SLOT_DATA_SIZE);
    assert!(size_of::<HeaderLayout>() <= HEADER_SIZE);
    // every u64 is 8-aligned, so atomic access through a page-aligned mapping is aligned
    assert!(
        off::FILE_SIZE % 8 == 0
            && off::WRITER_HEARTBEAT % 8 == 0
            && off::CONSUMER_HEARTBEAT % 8 == 0
    );
    assert!(off::DELIVERED % 8 == 0 && off::REPEATED % 8 == 0 && off::SLOTS % 8 == 0);
    assert!(off::SLOT_CAPTURE_QPC % 8 == 0 && off::SLOT_SIZE % 8 == 0);
};

/// Video format: used for the advertised list, `negotiated`, and slot descriptors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Format {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    /// [`SUBTYPE_NV12`] or [`SUBTYPE_RGB32`].
    pub subtype: u32,
}

impl Format {
    pub fn new(width: u32, height: u32, fps: u32, subtype: u32) -> Format {
        Format {
            width,
            height,
            fps_num: fps,
            fps_den: 1,
            subtype,
        }
    }
    /// Frame rate rounded to a whole number (0 when the denominator is 0).
    pub fn fps(&self) -> u32 {
        round_fps(self.fps_num, self.fps_den).unwrap_or(0)
    }
}

/// `num / den` rounded to the nearest whole number; `None` when `den` is 0. Computed in 64 bits, so values near
/// `u32::MAX` neither overflow nor wrap; a result above `u32::MAX` saturates.
pub fn round_fps(num: u32, den: u32) -> Option<u32> {
    let den = den as u64;
    if den == 0 {
        return None;
    }
    Some(((num as u64 + den / 2) / den).min(u32::MAX as u64) as u32)
}

/// What the writer is doing (`writer_state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum WriterState {
    Closed = 0,
    Paused = 1,
    Starting = 2,
    Live = 3,
}

impl WriterState {
    pub fn from_u32(v: u32) -> Option<WriterState> {
        match v {
            0 => Some(WriterState::Closed),
            1 => Some(WriterState::Paused),
            2 => Some(WriterState::Starting),
            3 => Some(WriterState::Live),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            WriterState::Closed => "closed",
            WriterState::Paused => "paused",
            WriterState::Starting => "starting",
            WriterState::Live => "live",
        }
    }
}

/// Descriptor of one slot as read from the file (unvalidated).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlotMeta {
    /// Slot index 0..3 (not stored in the file).
    pub index: u32,
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub subtype: u32,
    pub capture_qpc: u64,
    pub data_offset: u64,
    pub data_size: u64,
}

/// Every header field as plain values (unvalidated). Built from the mapping or from a plain read of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawHeader {
    pub magic: u32,
    pub protocol: u32,
    pub header_size: u32,
    pub slot_count: u32,
    pub file_size: u64,
    pub slot_capacity: u64,
    pub writer_pid: u32,
    pub writer_state: u32,
    pub writer_heartbeat: u64,
    pub qpc_frequency: u64,
    pub writer_frames: u64,
    pub advertised_count: u32,
    pub advertised: [Format; ADVERTISED_MAX],
    pub consumer_active: u32,
    pub reader_slot: u32,
    pub consumer_heartbeat: u64,
    pub negotiated: Format,
    pub latency_p50_us: u32,
    pub latency_p95_us: u32,
    pub delivered: u64,
    pub repeated: u64,
    pub latest_slot: u32,
}

impl RawHeader {
    /// Build a header from field accessors (`get32(offset)`, `get64(offset)`).
    pub fn load(get32: &dyn Fn(usize) -> u32, get64: &dyn Fn(usize) -> u64) -> RawHeader {
        let fmt = |o: usize| Format {
            width: get32(o + off::FMT_WIDTH),
            height: get32(o + off::FMT_HEIGHT),
            fps_num: get32(o + off::FMT_FPS_NUM),
            fps_den: get32(o + off::FMT_FPS_DEN),
            subtype: get32(o + off::FMT_SUBTYPE),
        };
        let mut advertised = [Format::default(); ADVERTISED_MAX];
        for (i, a) in advertised.iter_mut().enumerate() {
            *a = fmt(off::ADVERTISED + i * off::FMT_SIZE);
        }
        RawHeader {
            magic: get32(off::MAGIC),
            protocol: get32(off::PROTOCOL),
            header_size: get32(off::HEADER_SIZE),
            slot_count: get32(off::SLOT_COUNT),
            file_size: get64(off::FILE_SIZE),
            slot_capacity: get64(off::SLOT_CAPACITY),
            writer_pid: get32(off::WRITER_PID),
            writer_state: get32(off::WRITER_STATE),
            writer_heartbeat: get64(off::WRITER_HEARTBEAT),
            qpc_frequency: get64(off::QPC_FREQUENCY),
            writer_frames: get64(off::WRITER_FRAMES),
            advertised_count: get32(off::ADVERTISED_COUNT),
            advertised,
            consumer_active: get32(off::CONSUMER_ACTIVE),
            reader_slot: get32(off::READER_SLOT),
            consumer_heartbeat: get64(off::CONSUMER_HEARTBEAT),
            negotiated: fmt(off::NEGOTIATED),
            latency_p50_us: get32(off::LATENCY_P50_US),
            latency_p95_us: get32(off::LATENCY_P95_US),
            delivered: get64(off::DELIVERED),
            repeated: get64(off::REPEATED),
            latest_slot: get32(off::LATEST_SLOT),
        }
    }

    /// Parse the first [`HEADER_SIZE`] bytes of a plain read. `None` when `bytes` is shorter.
    pub fn from_bytes(bytes: &[u8]) -> Option<RawHeader> {
        if bytes.len() < HEADER_SIZE {
            return None;
        }
        let g32 = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        let g64 = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
        Some(RawHeader::load(&g32, &g64))
    }
}

/// Media types the camera advertises when no valid file exists: NV12 1080p and 720p, then RGB32 1080p and 720p, 30 fps.
pub fn default_advertised() -> Vec<Format> {
    let mut v = Vec::new();
    for subtype in [SUBTYPE_NV12, SUBTYPE_RGB32] {
        for &(w, h) in &crate::SIZES {
            v.push(Format::new(w, h, crate::FPS, subtype));
        }
    }
    v
}

/// The advertised list for an output size and rate (plan 4.3): the output size at its rate first, the same at 30 fps
/// when the rate is higher, then 1280x720 and 640x360 at 30 fps for 16:9 outputs larger than they are; NV12 for every
/// entry, then RGB32 for the entries up to 1080p. At most [`ADVERTISED_MAX`] entries.
///
/// An entry the protocol would refuse ([`crate::validate_format`]: odd size, past the limits, or not fitting a slot
/// like 4096x2304 NV12) is left out, so the list never offers a type a consumer could choose and then wait on
/// forever.
pub fn advertised_for_output(width: u32, height: u32, fps: u32) -> Vec<Format> {
    let mut sizes: Vec<(u32, u32, u32)> = vec![(width, height, fps)];
    if fps > 30 {
        sizes.push((width, height, 30));
    }
    if width as u64 * 9 == height as u64 * 16 {
        for &(w, h) in &[(1280u32, 720u32), (640, 360)] {
            if width > w {
                sizes.push((w, h, 30));
            }
        }
    }
    let fits =
        |w: u32, h: u32, subtype: u32| crate::validate::validate_format(w, h, subtype).is_ok();
    let mut out = Vec::new();
    for &(w, h, f) in &sizes {
        if fits(w, h, SUBTYPE_NV12) {
            out.push(Format::new(w, h, f, SUBTYPE_NV12));
        }
    }
    for &(w, h, f) in &sizes {
        if w <= RGB32_MAX_WIDTH && h <= RGB32_MAX_HEIGHT && fits(w, h, SUBTYPE_RGB32) {
            out.push(Format::new(w, h, f, SUBTYPE_RGB32));
        }
    }
    out.truncate(ADVERTISED_MAX);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_and_file_size() {
        assert_eq!(SLOT_CAPACITY, 12_451_840);
        assert_eq!(SLOT_CAPACITY % 65536, 0);
        const { assert!(SLOT_CAPACITY >= 3840 * 2160 * 3 / 2) };
        assert_eq!(FILE_SIZE, 4096 + 3 * 12_451_840);
    }

    #[test]
    fn advertised_list_follows_the_plan() {
        let l = advertised_for_output(1920, 1080, 60);
        let desc: Vec<_> = l
            .iter()
            .map(|f| (f.width, f.height, f.fps(), f.subtype))
            .collect();
        assert_eq!(
            desc,
            vec![
                (1920, 1080, 60, SUBTYPE_NV12),
                (1920, 1080, 30, SUBTYPE_NV12),
                (1280, 720, 30, SUBTYPE_NV12),
                (640, 360, 30, SUBTYPE_NV12),
                (1920, 1080, 60, SUBTYPE_RGB32),
                (1920, 1080, 30, SUBTYPE_RGB32),
                (1280, 720, 30, SUBTYPE_RGB32),
                (640, 360, 30, SUBTYPE_RGB32),
            ]
        );
        // 30 fps output: no duplicate 30 fps entry
        assert_eq!(advertised_for_output(1280, 720, 30).len(), 4);
        // 4K: NV12 only for the large sizes
        let k = advertised_for_output(3840, 2160, 30);
        assert!(k
            .iter()
            .filter(|f| f.subtype == SUBTYPE_RGB32)
            .all(|f| f.width <= 1920));
        assert!(k.len() <= ADVERTISED_MAX);
        // not 16:9: just the output
        assert_eq!(advertised_for_output(1000, 1000, 30).len(), 2);
    }

    #[test]
    fn defaults_match_p1() {
        let d = default_advertised();
        assert_eq!(d.len(), 4);
        assert_eq!((d[0].width, d[0].subtype), (1920, SUBTYPE_NV12));
        assert_eq!((d[3].height, d[3].subtype), (720, SUBTYPE_RGB32));
    }

    #[test]
    fn rounded_frame_rates_do_not_overflow() {
        // the old `num + den / 2` in u32 panicked in debug builds (wrapped in release) for these
        assert_eq!(round_fps(u32::MAX, u32::MAX - 1), Some(1));
        assert_eq!(round_fps(30, 1), Some(30));
        assert_eq!(round_fps(30000, 1001), Some(30));
        assert_eq!(round_fps(60, 0), None);
        assert_eq!(round_fps(0, 5), Some(0));
        assert_eq!(round_fps(u32::MAX, u32::MAX), Some(1));
        assert_eq!(round_fps(u32::MAX - 1, u32::MAX), Some(1));
        // num + den / 2 does not fit u32 here
        assert_eq!(round_fps(u32::MAX, 2), Some(1 << 31));
        assert_eq!(round_fps(u32::MAX, 1), Some(u32::MAX));
        let f = Format {
            fps_num: u32::MAX,
            fps_den: u32::MAX,
            ..Format::default()
        };
        assert_eq!(f.fps(), 1);
        assert_eq!(Format::new(1, 2, 30, SUBTYPE_NV12).fps(), 30);
        assert_eq!(Format { fps_den: 0, ..f }.fps(), 0);
    }

    #[test]
    fn from_bytes_needs_a_full_header() {
        assert!(RawHeader::from_bytes(&[0u8; 100]).is_none());
        let h = RawHeader::from_bytes(&vec![0u8; HEADER_SIZE]).unwrap();
        assert_eq!(h.magic, 0);
    }
}
