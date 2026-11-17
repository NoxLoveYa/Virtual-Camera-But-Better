//! Windows side of the transport: map `frames.bin`, QPC helpers, and the [`Writer`] the app uses.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::IntoRawHandle;
use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetFileAttributesW, GetFileSizeEx, SetEndOfFile, SetFileAttributesW,
    SetFilePointerEx, FILE_ATTRIBUTE_TEMPORARY, FILE_BEGIN, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, INVALID_FILE_ATTRIBUTES, OPEN_ALWAYS,
};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, VirtualQuery, FILE_MAP_ALL_ACCESS,
    FILE_MAP_READ, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READONLY,
    PAGE_READWRITE,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::header::{Format, WriterState, FILE_SIZE, HEADER_SIZE, MAX_FILE_SIZE};
use crate::secure;
use crate::seqlock::{PublishError, Shared, SlotFormat};

/// `%ProgramData%\BetterCam`, with `%ProgramData%` from `SHGetKnownFolderPath` (not the environment).
pub fn data_dir() -> PathBuf {
    secure::program_data_dir().join(crate::DATA_DIR_NAME)
}

/// `%ProgramData%\BetterCam\frames.bin`.
pub fn frames_path() -> PathBuf {
    data_dir().join(crate::FRAMES_FILE_NAME)
}

/// Current QPC ticks.
pub fn qpc_now() -> u64 {
    let mut v = 0i64;
    // QueryPerformanceCounter cannot fail on Windows XP and later.
    let _ = unsafe { QueryPerformanceCounter(&mut v) };
    v as u64
}

/// QPC ticks per second.
pub fn qpc_frequency() -> u64 {
    static F: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *F.get_or_init(|| {
        let mut v = 0i64;
        let _ = unsafe { QueryPerformanceFrequency(&mut v) };
        (v as u64).max(1)
    })
}

/// QPC ticks to 100 ns units (the Media Foundation time base).
pub fn qpc_to_hns(qpc: u64) -> i64 {
    (qpc as u128 * 10_000_000 / qpc_frequency() as u128).min(i64::MAX as u128) as i64
}

/// 100 ns units to QPC ticks.
pub fn hns_to_qpc(hns: i64) -> u64 {
    (hns.max(0) as u128 * qpc_frequency() as u128 / 10_000_000).min(u64::MAX as u128) as u64
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn win_err(e: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(e.code().0 & 0xFFFF)
}

#[cfg(test)]
type SizeHook = Box<dyn FnOnce()>;

#[cfg(test)]
thread_local! {
    /// Runs once on this thread between the size query and the mapping of [`MappedFile::map`] (tests only).
    static AFTER_SIZE_HOOK: std::cell::RefCell<Option<SizeHook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn run_after_size_hook() {
    if let Some(f) = AFTER_SIZE_HOOK.with(|h| h.borrow_mut().take()) {
        f();
    }
}

/// How a file is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    ReadOnly,
    ReadWrite,
}

/// An open, mapped `frames.bin`. Dropping it unmaps the view and closes both handles.
///
/// # What keeps a read inside the view
///
/// `len` is the number of bytes the code may touch, and every offset read from the file is checked against it
/// ([`crate::validate`]). The view must really be that long. Two facts of Windows guarantee it:
///
/// 1. **A mapped file cannot be cut below its section.** Once `CreateFileMappingW` has returned, truncating the file
///    (`SetEndOfFile`, `set_len`) fails with `ERROR_USER_MAPPED_FILE` for as long as the section exists, in
///    read-only and read-write mappings alike. Growing is allowed and harmless (the view stays in range).
///    This is what protects a reader after `open`; test `a_mapped_file_cannot_be_truncated_under_a_reader` pins
///    the behaviour down.
/// 2. **The window before the section exists is closed by checking afterwards.** The file size is read first, and
///    the file may shrink before the section is created. So the view is requested with exactly the checked size
///    (`MapViewOfFile` fails when the section is smaller), and `VirtualQuery` confirms the size of the view; a
///    shorter view fails the open cleanly. Test `a_file_that_shrinks_between_the_size_check_and_the_mapping_is_refused`
///    drives that window deterministically, `a_reader_survives_a_file_whose_size_keeps_switching` races it.
///
/// What no check can catch is a read error of the *medium* under the view (a removed disk or dropped network
/// share raises an in-page exception that `catch_unwind` cannot stop). That is why the DLL opens the file with
/// [`MappedFile::open_pinned`], which accepts only a local fixed disk.
pub struct MappedFile {
    file: HANDLE,
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
    shared: Shared,
}

// SAFETY: the handles and the view are plain kernel objects; the memory is accessed through `Shared`'s atomics.
unsafe impl Send for MappedFile {}
unsafe impl Sync for MappedFile {}

impl MappedFile {
    /// Open an existing file and map all of it. Rejects files shorter than the header or longer than
    /// [`MAX_FILE_SIZE`] before mapping anything.
    ///
    /// A link, a directory or a file with more than one hard link is refused (see [`secure`]); the location of the
    /// file is not checked, use [`MappedFile::open_pinned`] for a file in a folder that others can write to.
    pub fn open(path: &Path, access: Access) -> io::Result<MappedFile> {
        Self::open_with(path, access, None)
    }

    /// [`MappedFile::open`] for a file that must really be `path` below the trusted folder `root` on a local fixed
    /// disk (no junction or link in between; see [`secure::open_existing`]). The DLL uses this with
    /// `%ProgramData%`.
    pub fn open_pinned(path: &Path, root: &Path, access: Access) -> io::Result<MappedFile> {
        Self::open_with(path, access, Some(root))
    }

    fn open_with(path: &Path, access: Access, pin: Option<&Path>) -> io::Result<MappedFile> {
        let h = secure::open_existing(path, access == Access::ReadWrite, pin)?;
        Self::map(HANDLE(h.into_raw_handle()), access)
    }

    fn map(file: HANDLE, access: Access) -> io::Result<MappedFile> {
        let fail = |file: HANDLE, e: io::Error| {
            unsafe {
                let _ = CloseHandle(file);
            }
            Err(e)
        };
        let mut size = 0i64;
        if let Err(e) = unsafe { GetFileSizeEx(file, &mut size) } {
            return fail(file, win_err(e));
        }
        #[cfg(test)]
        run_after_size_hook();
        if size < HEADER_SIZE as i64 || size as u64 > MAX_FILE_SIZE {
            return fail(
                file,
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file size outside the accepted range",
                ),
            );
        }
        let (prot, view_access) = match access {
            Access::ReadOnly => (PAGE_READONLY, FILE_MAP_READ),
            Access::ReadWrite => (PAGE_READWRITE, FILE_MAP_ALL_ACCESS),
        };
        let mapping = match unsafe { CreateFileMappingW(file, None, prot, 0, 0, PCWSTR::null()) } {
            Ok(m) => m,
            Err(e) => return fail(file, win_err(e)),
        };
        let len = size as usize;
        // Map exactly the bytes that were checked: a section made from a file that shrank meanwhile is smaller than
        // `len` and the call fails instead of returning a short view.
        let view = unsafe { MapViewOfFile(mapping, view_access, 0, 0, len) };
        if view.Value.is_null() {
            let e = io::Error::last_os_error();
            unsafe {
                let _ = CloseHandle(mapping);
            }
            return fail(file, e);
        }
        // Belt and braces: the committed size of the view must cover `len`.
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        let got = unsafe {
            VirtualQuery(
                Some(view.Value as *const core::ffi::c_void),
                &mut mbi,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if got == 0 || mbi.RegionSize < len {
            unsafe {
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(mapping);
            }
            return fail(
                file,
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file shrank while it was being mapped",
                ),
            );
        }
        // SAFETY: the view covers `len` bytes (checked above) and a view is page aligned.
        let shared =
            unsafe { Shared::new(view.Value as *mut u8, len, access == Access::ReadWrite) }
                .expect("a mapped view of at least HEADER_SIZE bytes is aligned");
        Ok(MappedFile {
            file,
            mapping,
            view,
            len,
            shared,
        })
    }

    /// The shared view of the mapped bytes.
    pub fn shared(&self) -> &Shared {
        &self.shared
    }

    /// Length of the mapped file in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.mapping);
            let _ = CloseHandle(self.file);
        }
    }
}

/// Read the first [`HEADER_SIZE`] bytes of `path` with plain file I/O (no mapping, no thread). Returns the bytes and
/// the file length. Used where cost matters more than the live view: the advertised media types at source creation.
pub fn read_header_bytes(path: &Path) -> io::Result<(Vec<u8>, u64)> {
    read_header_bytes_with(path, None)
}

/// [`read_header_bytes`] for a file that must really be `path` below the trusted folder `root` (see
/// [`MappedFile::open_pinned`]).
pub fn read_header_bytes_pinned(path: &Path, root: &Path) -> io::Result<(Vec<u8>, u64)> {
    read_header_bytes_with(path, Some(root))
}

fn read_header_bytes_with(path: &Path, pin: Option<&Path>) -> io::Result<(Vec<u8>, u64)> {
    use std::io::Read;
    let mut f = std::fs::File::from(secure::open_existing(path, false, pin)?);
    let len = f.metadata()?.len();
    if len < HEADER_SIZE as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file shorter than the header",
        ));
    }
    let mut buf = vec![0u8; HEADER_SIZE];
    f.read_exact(&mut buf)?;
    Ok((buf, len))
}

/// The advertised media types of the file at `path`, or [`crate::default_advertised`] when it is missing, too short
/// or invalid. One plain read, no mapping.
pub fn read_advertised(path: &Path) -> Vec<Format> {
    advertised_from(read_header_bytes(path))
}

/// [`read_advertised`] for a file that must really be `path` below the trusted folder `root`.
pub fn read_advertised_pinned(path: &Path, root: &Path) -> Vec<Format> {
    advertised_from(read_header_bytes_pinned(path, root))
}

fn advertised_from(read: io::Result<(Vec<u8>, u64)>) -> Vec<Format> {
    let from_file = (|| {
        let (bytes, len) = read.ok()?;
        let raw = crate::RawHeader::from_bytes(&bytes)?;
        let v = crate::validate_header(&raw, len, None, None).ok()?;
        Some(v.advertised)
    })();
    match from_file {
        Some(list) if !list.is_empty() => list,
        _ => crate::default_advertised(),
    }
}

/// The app's end of the transport: creates `frames.bin`, publishes frames and state.
///
/// Dropping the writer sets `writer_state = closed` so the DLL switches to its slate at once.
pub struct Writer {
    map: MappedFile,
}

impl Writer {
    /// Create (or reuse) the file at `path` with `FILE_ATTRIBUTE_TEMPORARY`, size it to [`FILE_SIZE`], map it and
    /// write a fresh header (state `closed`, no advertised types).
    ///
    /// An existing file is never truncated: a running DLL may have it mapped, and Windows refuses to shrink a mapped
    /// file. A larger existing file is used as is.
    pub fn create(path: &Path) -> io::Result<Writer> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let w = wide(path);
        let file = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_ALWAYS,
                FILE_FLAGS_AND_ATTRIBUTES(FILE_ATTRIBUTE_TEMPORARY.0),
                None,
            )
        }
        .map_err(win_err)?;
        // an existing file keeps its attributes on OPEN_ALWAYS: make sure it is temporary too
        let attrs = unsafe { GetFileAttributesW(PCWSTR(w.as_ptr())) };
        if attrs != INVALID_FILE_ATTRIBUTES && attrs & FILE_ATTRIBUTE_TEMPORARY.0 == 0 {
            let _ = unsafe {
                SetFileAttributesW(
                    PCWSTR(w.as_ptr()),
                    FILE_FLAGS_AND_ATTRIBUTES(attrs | FILE_ATTRIBUTE_TEMPORARY.0),
                )
            };
        }
        let mut size = 0i64;
        let grow = unsafe { GetFileSizeEx(file, &mut size) }.and_then(|_| {
            if (size as u64) < FILE_SIZE {
                unsafe {
                    SetFilePointerEx(file, FILE_SIZE as i64, None, FILE_BEGIN)?;
                    SetEndOfFile(file)?;
                }
            }
            Ok(())
        });
        if let Err(e) = grow {
            unsafe {
                let _ = CloseHandle(file);
            }
            return Err(win_err(e));
        }
        let map = MappedFile::map(file, Access::ReadWrite)?;
        map.shared()
            .init_writer(std::process::id(), qpc_frequency(), qpc_now())
            .map_err(|e| io::Error::other(format!("{e:?}")))?;
        Ok(Writer { map })
    }

    /// [`Writer::create`] at `%ProgramData%\BetterCam\frames.bin`.
    pub fn create_default() -> io::Result<Writer> {
        Writer::create(&frames_path())
    }

    pub fn shared(&self) -> &Shared {
        self.map.shared()
    }

    /// Set the media types the camera advertises (read by the DLL at activation).
    pub fn set_advertised(&self, formats: &[Format]) {
        self.shared().set_advertised(formats);
    }

    /// Set `writer_state` and refresh the heartbeat.
    pub fn set_state(&self, state: WriterState) {
        self.shared().set_writer_state(state);
        self.heartbeat();
    }

    /// Refresh `writer_heartbeat`. Call it every frame and at least every 200 ms when idle.
    pub fn heartbeat(&self) {
        self.shared().set_writer_heartbeat(qpc_now());
    }

    /// True while a stream runs in Frame Server (`consumer_active != 0` and its heartbeat is under 1 s old).
    pub fn consumer_active(&self) -> bool {
        let h = self.shared().header();
        h.consumer_active != 0 && qpc_now().saturating_sub(h.consumer_heartbeat) <= qpc_frequency()
    }

    /// The format the DLL's stream negotiated, when a consumer is active and the format is valid. The writer
    /// delivers frames of exactly this size and subtype.
    pub fn negotiated(&self) -> Option<Format> {
        if !self.consumer_active() {
            return None;
        }
        let f = self.shared().header().negotiated;
        crate::validate_format(f.width, f.height, f.subtype).ok()?;
        Some(f)
    }

    /// Publish a frame (see [`Shared::publish`]). `capture_qpc` is when the frame was captured (QPC ticks); the
    /// heartbeat is refreshed too.
    pub fn publish(
        &self,
        f: SlotFormat,
        capture_qpc: u64,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<u64, PublishError> {
        self.shared().publish(f, capture_qpc, qpc_now(), fill)
    }

    /// Mark the file closed (the DLL shows "not running" immediately). Also done on drop.
    pub fn close(&self) {
        self.set_state(WriterState::Closed);
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{SUBTYPE_NV12, SUBTYPE_RGB32};
    use crate::seqlock::{Disposition, Read};
    use crate::validate::validate_header;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bettercam-proto-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn writer_creates_a_sized_temporary_file_and_a_reader_sees_frames() {
        let path = temp_file("rw.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), FILE_SIZE);
        let attrs = unsafe { GetFileAttributesW(PCWSTR(wide(&path).as_ptr())) };
        assert_ne!(
            attrs & FILE_ATTRIBUTE_TEMPORARY.0,
            0,
            "file must be FILE_ATTRIBUTE_TEMPORARY"
        );

        w.set_advertised(&crate::advertised_for_output(1280, 720, 30));
        w.set_state(WriterState::Live);
        let fmt = SlotFormat {
            width: 64,
            height: 64,
            stride: 64,
            subtype: SUBTYPE_NV12,
        };
        w.publish(fmt, 123, |d| d.fill(7)).unwrap();

        // second, read-only mapping of the same file (what stats does)
        let ro = MappedFile::open(&path, Access::ReadOnly).unwrap();
        assert!(!ro.shared().is_writable());
        let raw = ro.shared().header();
        let v = validate_header(
            &raw,
            ro.len() as u64,
            Some(qpc_frequency()),
            Some(qpc_now()),
        )
        .unwrap();
        assert_eq!(v.state, WriterState::Live);
        assert_eq!(v.advertised.len(), 4);
        let mut got = vec![0u8; 64 * 64 * 3 / 2];
        let r = ro
            .shared()
            .read_latest(&v, |_, d| {
                d.copy_to(0, &mut got);
                Disposition::Taken
            })
            .unwrap();
        assert!(matches!(r, Read::Taken(m) if m.capture_qpc == 123));
        assert!(got.iter().all(|&b| b == 7));

        // a read-write reader sees what the DLL would write
        let rw = MappedFile::open(&path, Access::ReadWrite).unwrap();
        rw.shared()
            .set_negotiated(&Format::new(640, 360, 30, SUBTYPE_RGB32));
        rw.shared().set_consumer_active(true);
        rw.shared().set_consumer_heartbeat(qpc_now());
        assert_eq!(
            w.negotiated(),
            Some(Format::new(640, 360, 30, SUBTYPE_RGB32))
        );
        rw.shared()
            .set_consumer_heartbeat(qpc_now().saturating_sub(2 * qpc_frequency()));
        assert_eq!(
            w.negotiated(),
            None,
            "a stale consumer heartbeat means no consumer"
        );

        // plain read of the advertised list
        assert_eq!(read_advertised(&path).len(), 4);
        drop(w);
        // dropping the writer closes the file for readers
        assert_eq!(
            ro.shared().header().writer_state,
            WriterState::Closed as u32
        );
    }

    #[test]
    fn reopening_keeps_the_file_and_resets_the_header() {
        let path = temp_file("reopen.bin");
        let _ = std::fs::remove_file(&path);
        let w1 = Writer::create(&path).unwrap();
        w1.set_state(WriterState::Live);
        let fmt = SlotFormat {
            width: 64,
            height: 64,
            stride: 64,
            subtype: SUBTYPE_NV12,
        };
        w1.publish(fmt, 1, |_| {}).unwrap();
        // while a mapping exists the file cannot shrink; a second writer must still work
        let w2 = Writer::create(&path).unwrap();
        assert_eq!(w2.shared().header().latest_slot, crate::NO_SLOT);
        assert_eq!(w2.shared().header().writer_frames, 0);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), FILE_SIZE);
    }

    #[test]
    fn hostile_files_are_rejected_before_mapping() {
        let short = temp_file("short.bin");
        std::fs::write(&short, vec![0u8; 100]).unwrap();
        assert!(MappedFile::open(&short, Access::ReadOnly).is_err());
        assert!(MappedFile::open(&temp_file("missing.bin"), Access::ReadOnly).is_err());
        // a file of zeros maps but has no magic
        let zeros = temp_file("zeros.bin");
        std::fs::write(&zeros, vec![0u8; 8192]).unwrap();
        let m = MappedFile::open(&zeros, Access::ReadOnly).unwrap();
        assert!(validate_header(&m.shared().header(), m.len() as u64, None, None).is_err());
        assert_eq!(read_advertised(&zeros), crate::default_advertised());
        assert_eq!(read_advertised(&short), crate::default_advertised());
        assert_eq!(
            read_advertised(&temp_file("missing.bin")),
            crate::default_advertised()
        );
    }

    #[test]
    fn a_file_shorter_than_its_header_claims_is_invalid() {
        // valid header, but the file is only 1 MB: file_size > mapped length
        let path = temp_file("trunc.bin");
        let _ = std::fs::remove_file(&path);
        {
            let w = Writer::create(&path).unwrap();
            w.set_state(WriterState::Live);
        }
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..1024 * 1024]).unwrap();
        let m = MappedFile::open(&path, Access::ReadOnly).unwrap();
        let err = validate_header(&m.shared().header(), m.len() as u64, None, None).unwrap_err();
        assert_eq!(err, crate::validate::HeaderError::BadFileSize);
    }

    /// A file with a valid header and published frames (the writer alternates between slots 0 and 1, so the latest
    /// slot is slot 1, about 12 MB into the file), the writer already gone so nothing maps it.
    fn file_with_three_frames(name: &str) -> PathBuf {
        let path = temp_file(name);
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let fmt = SlotFormat {
            width: 64,
            height: 64,
            stride: 64,
            subtype: SUBTYPE_NV12,
        };
        for n in 0..3u8 {
            w.publish(fmt, 100 + n as u64, |d| d.fill(n + 1)).unwrap();
        }
        // three publishes: slots 0, 1, 0 -> latest is 0; one more makes it 1
        w.publish(fmt, 200, |d| d.fill(9)).unwrap();
        assert_eq!(w.shared().header().latest_slot, 1);
        path
    }

    /// Read the newest frame the way the DLL does: validate the header against the mapped length, then
    /// `read_latest`, touching the first bytes of the slot data.
    fn read_like_the_dll(m: &MappedFile) {
        if let Ok(v) = validate_header(&m.shared().header(), m.len() as u64, None, None) {
            let _ = m.shared().read_latest(&v, |_, d| {
                let mut b = [0u8; 64];
                d.copy_to(0, &mut b);
                Disposition::Taken
            });
        }
    }

    #[test]
    fn a_file_that_shrinks_between_the_size_check_and_the_mapping_is_refused() {
        let path = file_with_three_frames("shrink-hook.bin");
        let p = path.clone();
        AFTER_SIZE_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
                f.set_len(8192).unwrap();
            }));
        });
        let r = MappedFile::open(&path, Access::ReadWrite);
        match r {
            Err(_) => {}
            Ok(m) => {
                // Not reached by fixed code. The old code gets here with `len` of 37 MB over an 8 KB view.
                panic!(
                    "mapped a {} byte view over a file that shrank to 8192",
                    m.len()
                );
            }
        }
        // the read-only path behaves the same
        let path = file_with_three_frames("shrink-hook-ro.bin");
        let p = path.clone();
        AFTER_SIZE_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
                f.set_len(8192).unwrap();
            }));
        });
        assert!(MappedFile::open(&path, Access::ReadOnly).is_err());
    }

    /// The reviewer's reproduction: one thread switches the file between 8 KB and full size while another opens,
    /// validates and reads. The old code died with 0xC0000005 within a second or two.
    #[test]
    fn a_reader_survives_a_file_whose_size_keeps_switching() {
        let path = file_with_three_frames("shrink-race.bin");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let toggler = {
            let (p, stop) = (path.clone(), stop.clone());
            std::thread::spawn(move || {
                let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    // shrinking fails while a mapping exists (see MappedFile): that is the point
                    let _ = f.set_len(8192);
                    let _ = f.set_len(FILE_SIZE);
                }
            })
        };
        let began = std::time::Instant::now();
        let (mut opened, mut refused) = (0u32, 0u32);
        while began.elapsed() < std::time::Duration::from_secs(3) {
            for access in [Access::ReadOnly, Access::ReadWrite] {
                match MappedFile::open(&path, access) {
                    Ok(m) => {
                        opened += 1;
                        read_like_the_dll(&m);
                    }
                    Err(_) => refused += 1,
                }
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        toggler.join().unwrap();
        eprintln!("size race: {opened} opened, {refused} refused");
        assert!(opened + refused > 0);
    }

    #[test]
    fn a_mapped_file_cannot_be_truncated_under_a_reader() {
        // The guarantee `MappedFile` relies on: Windows refuses to cut a file below the size of a section that is
        // mapped from it (ERROR_USER_MAPPED_FILE), whatever the access of the mapping.
        for access in [Access::ReadOnly, Access::ReadWrite] {
            let path = file_with_three_frames(&format!("truncate-{access:?}.bin"));
            let m = MappedFile::open(&path, access).unwrap();
            let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            let err = f
                .set_len(8192)
                .expect_err("truncating a mapped file must fail");
            assert_eq!(err.raw_os_error(), Some(1224), "{access:?}: {err}");
            assert_eq!(std::fs::metadata(&path).unwrap().len(), FILE_SIZE);
            read_like_the_dll(&m);
            // once the mapping is gone the file can shrink again
            drop(m);
            f.set_len(8192).unwrap();
        }
    }

    #[test]
    fn time_conversions_saturate_instead_of_wrapping() {
        // a QPC value from a hostile file: u64::MAX ticks must not wrap into a negative time
        assert!(qpc_to_hns(u64::MAX) > 0);
        assert!(qpc_to_hns(u64::MAX) >= qpc_to_hns(u64::MAX / 2));
        assert!(hns_to_qpc(i64::MAX) >= hns_to_qpc(i64::MAX / 2));
        assert_eq!(hns_to_qpc(i64::MIN), 0);
        assert_eq!(qpc_to_hns(0), 0);
    }

    #[test]
    fn time_conversions_round_trip() {
        let q = qpc_now();
        let back = hns_to_qpc(qpc_to_hns(q));
        assert!(q.abs_diff(back) <= qpc_frequency() / 1_000_000 + 1);
    }
}
