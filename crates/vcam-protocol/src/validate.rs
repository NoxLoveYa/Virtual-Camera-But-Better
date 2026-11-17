//! Validation of everything read from `frames.bin`. The DLL runs these checks before it trusts any field: a bad or
//! hostile file must give the slate, never a crash or an out-of-bounds read.

use crate::header::{
    Format, RawHeader, SlotMeta, WriterState, ADVERTISED_MAX, HEADER_SIZE, HEARTBEAT_TIMEOUT_MS,
    MAX_HEIGHT, MAX_WIDTH, RGB32_MAX_HEIGHT, RGB32_MAX_WIDTH, SLOT_CAPACITY, SLOT_COUNT,
    SUBTYPE_NV12, SUBTYPE_RGB32,
};
use crate::MAGIC;

/// Largest `slot_capacity` accepted (a 4096x2304 NV12 frame is 14 MB; 64 MB is a sanity bound).
pub const MAX_SLOT_CAPACITY: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderError {
    BadMagic,
    BadProtocol,
    BadHeaderSize,
    BadSlotCount,
    /// `file_size` is smaller than header plus slots, or larger than the mapped file.
    BadFileSize,
    BadSlotCapacity,
    BadQpcFrequency,
    BadWriterState,
    /// Heartbeat further in the future than the timeout allows.
    HeartbeatInFuture,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatError {
    ZeroSize,
    /// Width or height is odd (NV12 needs even sizes; the protocol requires them for every subtype).
    OddSize,
    TooLarge,
    UnknownSubtype,
    /// RGB32 above 1920x1080.
    Rgb32TooLarge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotError {
    Format(FormatError),
    /// Stride smaller than one row.
    BadStride,
    /// `data_size` smaller than the frame needs, or larger than a slot.
    BadDataSize,
    /// `data_offset` inside the header, or `data_offset + data_size` past `file_size`.
    OutOfBounds,
}

impl From<FormatError> for SlotError {
    fn from(e: FormatError) -> Self {
        SlotError::Format(e)
    }
}

/// A header that passed [`validate_header`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidHeader {
    pub state: WriterState,
    pub writer_pid: u32,
    pub writer_heartbeat: u64,
    pub qpc_frequency: u64,
    pub file_size: u64,
    pub slot_capacity: u64,
    pub latest_slot: u32,
    /// Advertised entries that passed [`validate_advertised`] (invalid ones are dropped); empty means "use defaults".
    pub advertised: Vec<Format>,
}

/// Check a header read from a mapping (or a plain read) of `mapped_len` bytes.
///
/// `now_qpc` and `local_qpc_frequency` are given by readers that also judge liveness (the DLL); pass `None` for a
/// plain read that only wants the advertised list.
pub fn validate_header(
    h: &RawHeader,
    mapped_len: u64,
    local_qpc_frequency: Option<u64>,
    now_qpc: Option<u64>,
) -> Result<ValidHeader, HeaderError> {
    if h.magic != MAGIC {
        return Err(HeaderError::BadMagic);
    }
    if h.protocol != crate::PROTOCOL_VERSION {
        return Err(HeaderError::BadProtocol);
    }
    if h.header_size as usize != HEADER_SIZE || mapped_len < HEADER_SIZE as u64 {
        return Err(HeaderError::BadHeaderSize);
    }
    if h.slot_count as usize != SLOT_COUNT {
        return Err(HeaderError::BadSlotCount);
    }
    if h.slot_capacity == 0 || h.slot_capacity > MAX_SLOT_CAPACITY {
        return Err(HeaderError::BadSlotCapacity);
    }
    let needed = (HEADER_SIZE as u64)
        .checked_add(
            h.slot_capacity
                .checked_mul(SLOT_COUNT as u64)
                .ok_or(HeaderError::BadSlotCapacity)?,
        )
        .ok_or(HeaderError::BadSlotCapacity)?;
    if h.file_size < needed || h.file_size > mapped_len {
        return Err(HeaderError::BadFileSize);
    }
    if let Some(f) = local_qpc_frequency {
        if h.qpc_frequency != f {
            return Err(HeaderError::BadQpcFrequency);
        }
    }
    if h.qpc_frequency == 0 {
        return Err(HeaderError::BadQpcFrequency);
    }
    let state = WriterState::from_u32(h.writer_state).ok_or(HeaderError::BadWriterState)?;
    if let Some(now) = now_qpc {
        let slack = h.qpc_frequency.saturating_mul(HEARTBEAT_TIMEOUT_MS) / 1000;
        if h.writer_heartbeat > now.saturating_add(slack) {
            return Err(HeaderError::HeartbeatInFuture);
        }
    }
    Ok(ValidHeader {
        state,
        writer_pid: h.writer_pid,
        writer_heartbeat: h.writer_heartbeat,
        qpc_frequency: h.qpc_frequency,
        file_size: h.file_size,
        slot_capacity: h.slot_capacity,
        latest_slot: h.latest_slot,
        advertised: validate_advertised(h),
    })
}

/// Advertised entries that are sane: even non-zero size within limits, a known subtype (RGB32 up to 1080p), a rate
/// of 1 to 240 fps. At most [`ADVERTISED_MAX`] entries are read.
pub fn validate_advertised(h: &RawHeader) -> Vec<Format> {
    let n = (h.advertised_count as usize).min(ADVERTISED_MAX);
    h.advertised[..n]
        .iter()
        .copied()
        .filter(|f| {
            validate_format(f.width, f.height, f.subtype).is_ok()
                && f.fps_den != 0
                && (1..=240).contains(&f.fps())
        })
        .collect()
}

/// Dimensions and subtype of a frame.
pub fn validate_format(width: u32, height: u32, subtype: u32) -> Result<(), FormatError> {
    if width == 0 || height == 0 {
        return Err(FormatError::ZeroSize);
    }
    if width % 2 != 0 || height % 2 != 0 {
        return Err(FormatError::OddSize);
    }
    if width > MAX_WIDTH || height > MAX_HEIGHT {
        return Err(FormatError::TooLarge);
    }
    match subtype {
        SUBTYPE_NV12 => {
            // 4096x2304 passes the limits above but is 14 MB: it must also fit one slot, or a consumer that
            // picks it would wait on a writer that can never publish it
            if width as u64 * height as u64 * 3 / 2 > SLOT_CAPACITY {
                Err(FormatError::TooLarge)
            } else {
                Ok(())
            }
        }
        SUBTYPE_RGB32 => {
            if width > RGB32_MAX_WIDTH || height > RGB32_MAX_HEIGHT {
                Err(FormatError::Rgb32TooLarge)
            } else {
                Ok(())
            }
        }
        _ => Err(FormatError::UnknownSubtype),
    }
}

/// Bytes a frame needs for the given stride (NV12: Y plane plus UV plane at the same stride).
pub fn frame_bytes(width: u32, height: u32, stride: u32, subtype: u32) -> Option<u64> {
    let _ = width;
    match subtype {
        SUBTYPE_NV12 => Some(stride as u64 * height as u64 * 3 / 2),
        SUBTYPE_RGB32 => Some(stride as u64 * height as u64),
        _ => None,
    }
}

/// Minimum stride of a row.
pub fn min_stride(width: u32, subtype: u32) -> u32 {
    if subtype == SUBTYPE_RGB32 {
        width.saturating_mul(4)
    } else {
        width
    }
}

/// Check one slot descriptor against a validated header: format, stride, data size, and
/// `HEADER_SIZE <= data_offset`, `data_offset + data_size <= file_size`.
pub fn validate_slot(m: &SlotMeta, v: &ValidHeader) -> Result<(), SlotError> {
    validate_format(m.width, m.height, m.subtype)?;
    if m.stride < min_stride(m.width, m.subtype) {
        return Err(SlotError::BadStride);
    }
    let need = frame_bytes(m.width, m.height, m.stride, m.subtype).ok_or(SlotError::BadDataSize)?;
    if m.data_size < need || m.data_size > v.slot_capacity {
        return Err(SlotError::BadDataSize);
    }
    let end = m
        .data_offset
        .checked_add(m.data_size)
        .ok_or(SlotError::OutOfBounds)?;
    if m.data_offset < HEADER_SIZE as u64 || end > v.file_size {
        return Err(SlotError::OutOfBounds);
    }
    Ok(())
}

/// True when the writer's heartbeat is older than [`HEARTBEAT_TIMEOUT_MS`].
pub fn is_stale(v: &ValidHeader, now_qpc: u64) -> bool {
    let age = now_qpc.saturating_sub(v.writer_heartbeat);
    age > v.qpc_frequency.saturating_mul(HEARTBEAT_TIMEOUT_MS) / 1000
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{FILE_SIZE, SLOT_CAPACITY};

    fn good_header() -> RawHeader {
        let mut b = vec![0u8; HEADER_SIZE];
        let mut put32 = |o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
        put32(crate::off::MAGIC, MAGIC);
        put32(crate::off::PROTOCOL, 1);
        put32(crate::off::HEADER_SIZE, HEADER_SIZE as u32);
        put32(crate::off::SLOT_COUNT, 3);
        put32(crate::off::WRITER_STATE, 3);
        let mut put64 = |o: usize, v: u64| b[o..o + 8].copy_from_slice(&v.to_le_bytes());
        put64(crate::off::FILE_SIZE, FILE_SIZE);
        put64(crate::off::SLOT_CAPACITY, SLOT_CAPACITY);
        put64(crate::off::QPC_FREQUENCY, 10_000_000);
        put64(crate::off::WRITER_HEARTBEAT, 5_000_000);
        RawHeader::from_bytes(&b).unwrap()
    }

    fn good_slot(v: &ValidHeader) -> SlotMeta {
        let m = SlotMeta {
            index: 0,
            seq: 2,
            width: 1920,
            height: 1080,
            stride: 1920,
            subtype: SUBTYPE_NV12,
            capture_qpc: 1,
            data_offset: HEADER_SIZE as u64,
            data_size: 1920 * 1080 * 3 / 2,
        };
        assert!(validate_slot(&m, v).is_ok());
        m
    }

    #[test]
    fn good_header_passes() {
        let v =
            validate_header(&good_header(), FILE_SIZE, Some(10_000_000), Some(5_000_000)).unwrap();
        assert_eq!(v.state, WriterState::Live);
        assert!(!is_stale(&v, 5_000_000 + 9_000_000));
        assert!(is_stale(&v, 5_000_000 + 10_000_001));
    }

    #[test]
    fn header_fields_are_checked() {
        let ok = good_header();
        let check = |f: &dyn Fn(&mut RawHeader), len: u64, want: HeaderError| {
            let mut h = ok.clone();
            f(&mut h);
            assert_eq!(
                validate_header(&h, len, Some(10_000_000), Some(5_000_000)).unwrap_err(),
                want
            );
        };
        check(&|h| h.magic = 0, FILE_SIZE, HeaderError::BadMagic);
        check(&|h| h.protocol = 2, FILE_SIZE, HeaderError::BadProtocol);
        check(
            &|h| h.header_size = 64,
            FILE_SIZE,
            HeaderError::BadHeaderSize,
        );
        check(&|h| h.slot_count = 4, FILE_SIZE, HeaderError::BadSlotCount);
        check(
            &|h| h.slot_capacity = 0,
            FILE_SIZE,
            HeaderError::BadSlotCapacity,
        );
        check(
            &|h| h.slot_capacity = u64::MAX,
            FILE_SIZE,
            HeaderError::BadSlotCapacity,
        );
        // file_size larger than what is mapped: offsets would run past the end
        check(&|_| {}, FILE_SIZE - 1, HeaderError::BadFileSize);
        check(
            &|h| h.file_size = u64::MAX,
            FILE_SIZE,
            HeaderError::BadFileSize,
        );
        // file_size too small for three slots
        check(
            &|h| h.file_size = HEADER_SIZE as u64 + SLOT_CAPACITY,
            FILE_SIZE,
            HeaderError::BadFileSize,
        );
        check(
            &|h| h.qpc_frequency = 3,
            FILE_SIZE,
            HeaderError::BadQpcFrequency,
        );
        check(
            &|h| h.writer_state = 9,
            FILE_SIZE,
            HeaderError::BadWriterState,
        );
        check(
            &|h| h.writer_heartbeat = u64::MAX,
            FILE_SIZE,
            HeaderError::HeartbeatInFuture,
        );
        // mapped shorter than the header
        check(&|_| {}, 100, HeaderError::BadHeaderSize);
    }

    #[test]
    fn slot_offsets_past_the_file_end_are_rejected() {
        let v = validate_header(&good_header(), FILE_SIZE, None, None).unwrap();
        let ok = good_slot(&v);
        let mut m = ok;
        m.data_offset = v.file_size - 100;
        assert_eq!(validate_slot(&m, &v), Err(SlotError::OutOfBounds));
        m.data_offset = u64::MAX - 10;
        assert_eq!(validate_slot(&m, &v), Err(SlotError::OutOfBounds));
        m.data_offset = 0; // inside the header
        assert_eq!(validate_slot(&m, &v), Err(SlotError::OutOfBounds));
        // slot 2 at its natural place is fine, one byte further is not
        m.data_offset = HEADER_SIZE as u64 + 2 * SLOT_CAPACITY;
        assert!(validate_slot(&m, &v).is_ok());
        m.data_offset += SLOT_CAPACITY;
        assert_eq!(validate_slot(&m, &v), Err(SlotError::OutOfBounds));
    }

    #[test]
    fn odd_zero_and_oversized_frames_are_rejected() {
        let v = validate_header(&good_header(), FILE_SIZE, None, None).unwrap();
        let ok = good_slot(&v);
        let mut m = ok;
        m.width = 1919;
        assert_eq!(
            validate_slot(&m, &v),
            Err(SlotError::Format(FormatError::OddSize))
        );
        m = ok;
        m.height = 1081;
        assert_eq!(
            validate_slot(&m, &v),
            Err(SlotError::Format(FormatError::OddSize))
        );
        m = ok;
        m.width = 0;
        assert_eq!(
            validate_slot(&m, &v),
            Err(SlotError::Format(FormatError::ZeroSize))
        );
        m = ok;
        m.width = 4098;
        assert_eq!(
            validate_slot(&m, &v),
            Err(SlotError::Format(FormatError::TooLarge))
        );
        m = ok;
        m.height = 2306;
        assert_eq!(
            validate_slot(&m, &v),
            Err(SlotError::Format(FormatError::TooLarge))
        );
        m = ok;
        m.subtype = 7;
        assert_eq!(
            validate_slot(&m, &v),
            Err(SlotError::Format(FormatError::UnknownSubtype))
        );
        // the limits themselves: 4096x2304 NV12 is 14 MB and does not fit a slot, 3840x2160 does
        assert_eq!(
            validate_format(4096, 2304, SUBTYPE_NV12),
            Err(FormatError::TooLarge)
        );
        assert!(validate_format(3840, 2160, SUBTYPE_NV12).is_ok());
        assert_eq!(
            validate_format(4096, 2304, SUBTYPE_RGB32),
            Err(FormatError::Rgb32TooLarge)
        );
        assert!(validate_format(1920, 1080, SUBTYPE_RGB32).is_ok());
    }

    #[test]
    fn accepted_formats_always_fit_a_slot_and_nothing_that_fits_is_refused() {
        use crate::header::{advertised_for_output, SLOT_CAPACITY};
        // every even size inside the width/height limits: valid exactly when the tight NV12 frame fits a slot
        for w in (2..=MAX_WIDTH).step_by(14) {
            for h in (2..=MAX_HEIGHT).step_by(18) {
                let bytes = w as u64 * h as u64 * 3 / 2;
                let r = validate_format(w, h, SUBTYPE_NV12);
                assert_eq!(r.is_ok(), bytes <= SLOT_CAPACITY, "{w}x{h}: {r:?}");
                if r.is_ok() {
                    assert!(frame_bytes(w, h, w, SUBTYPE_NV12).unwrap() <= SLOT_CAPACITY);
                }
            }
        }
        assert!(validate_format(4096, 2026, SUBTYPE_NV12).is_ok());
        assert_eq!(
            validate_format(4096, 2160, SUBTYPE_NV12),
            Err(FormatError::TooLarge)
        );
        // what the app advertises is what the protocol accepts, for 4K and larger outputs too
        for (w, h) in [
            (4096, 2304),
            (4096, 2160),
            (3840, 2160),
            (1920, 1080),
            (4000, 2250),
        ] {
            for fps in [24, 30, 60] {
                let l = advertised_for_output(w, h, fps);
                // an output that cannot be a format and is not 16:9 advertises nothing (readers then use the
                // defaults); every other output advertises something
                let output_ok = validate_format(w, h, SUBTYPE_NV12).is_ok();
                let sixteen_nine = w as u64 * 9 == h as u64 * 16;
                assert_eq!(l.is_empty(), !output_ok && !sixteen_nine, "{w}x{h}");
                for f in &l {
                    assert!(
                        validate_format(f.width, f.height, f.subtype).is_ok(),
                        "{w}x{h}@{fps} advertises {f:?}"
                    );
                }
            }
        }
        // 4096x2304 is 16:9: it falls back to the smaller 16:9 sizes
        let l = advertised_for_output(4096, 2304, 30);
        assert!(l.iter().all(|f| f.width <= 1280));
        assert!(l
            .iter()
            .any(|f| f.width == 1280 && f.subtype == SUBTYPE_NV12));
    }

    #[test]
    fn extreme_frame_rates_neither_overflow_nor_pass() {
        let mut h = good_header();
        h.advertised_count = 5;
        h.advertised[0] = Format {
            fps_num: u32::MAX,
            fps_den: u32::MAX,
            ..Format::new(1280, 720, 30, SUBTYPE_NV12)
        }; // exactly 1 fps: valid, and `num + den / 2` overflowed u32
        h.advertised[1] = Format {
            fps_num: u32::MAX,
            fps_den: 1,
            ..Format::new(1280, 720, 30, SUBTYPE_NV12)
        }; // far above 240
        h.advertised[2] = Format {
            fps_num: u32::MAX - 1,
            fps_den: u32::MAX,
            ..Format::new(1280, 720, 30, SUBTYPE_NV12)
        };
        h.advertised[3] = Format {
            fps_num: 0,
            fps_den: u32::MAX,
            ..Format::new(1280, 720, 30, SUBTYPE_NV12)
        };
        h.advertised[4] = Format {
            fps_num: u32::MAX,
            fps_den: 2,
            ..Format::new(1280, 720, 30, SUBTYPE_NV12)
        };
        let v = validate_header(&h, FILE_SIZE, None, None).unwrap();
        let rates: Vec<u32> = v.advertised.iter().map(|f| f.fps()).collect();
        assert_eq!(rates, vec![1, 1]);
    }

    #[test]
    fn stride_and_data_size_are_checked() {
        let v = validate_header(&good_header(), FILE_SIZE, None, None).unwrap();
        let ok = good_slot(&v);
        let mut m = ok;
        m.stride = 1000;
        assert_eq!(validate_slot(&m, &v), Err(SlotError::BadStride));
        m = ok;
        m.data_size -= 1;
        assert_eq!(validate_slot(&m, &v), Err(SlotError::BadDataSize));
        m = ok;
        m.data_size = SLOT_CAPACITY + 1;
        assert_eq!(validate_slot(&m, &v), Err(SlotError::BadDataSize));
        // RGB32 needs 4 bytes per pixel
        m = ok;
        m.subtype = SUBTYPE_RGB32;
        assert_eq!(validate_slot(&m, &v), Err(SlotError::BadStride));
        m.stride = 1920 * 4;
        m.data_size = 1920 * 1080 * 4;
        assert!(validate_slot(&m, &v).is_ok());
    }

    #[test]
    fn invalid_advertised_entries_are_dropped() {
        let mut h = good_header();
        h.advertised_count = 12; // more than the table holds: clamped
        h.advertised[0] = Format::new(1920, 1080, 60, SUBTYPE_NV12);
        h.advertised[1] = Format::new(1921, 1080, 30, SUBTYPE_NV12); // odd
        h.advertised[2] = Format::new(3840, 2160, 30, SUBTYPE_RGB32); // RGB32 too large
        h.advertised[3] = Format::new(1280, 720, 0, SUBTYPE_NV12); // no rate
        h.advertised[4] = Format {
            fps_den: 0,
            ..Format::new(1280, 720, 30, SUBTYPE_NV12)
        };
        h.advertised[5] = Format::new(1280, 720, 30, SUBTYPE_RGB32);
        let v = validate_header(&h, FILE_SIZE, None, None).unwrap();
        assert_eq!(v.advertised.len(), 2);
        assert_eq!(v.advertised[1].subtype, SUBTYPE_RGB32);
    }
}
