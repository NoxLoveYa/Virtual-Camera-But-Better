//! Windows side of the transport: map `frames.bin`, QPC helpers, and the [`Writer`] the app uses.

use std::io;
use std::os::windows::io::IntoRawHandle;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    GetFileSizeEx, SetEndOfFile, SetFilePointerEx, FILE_ATTRIBUTE_TEMPORARY, FILE_BEGIN,
};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, VirtualQuery, FILE_MAP_ALL_ACCESS,
    FILE_MAP_READ, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READONLY,
    PAGE_READWRITE,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::header::{
    Format, RawHeader, SlotMeta, WriterState, FILE_SIZE, HEADER_SIZE, MAX_FILE_SIZE,
};
use crate::secure;
use crate::seqlock::{PublishError, Shared, SlotFormat};
use crate::signal::FrameSignal;

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

#[cfg(test)]
fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn win_err(e: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(e.code().0 & 0xFFFF)
}

/// True when `path` names the production frame file (compared without case).
fn is_default_path(path: &Path) -> bool {
    path.to_string_lossy()
        .eq_ignore_ascii_case(&frames_path().to_string_lossy())
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
        let raw = RawHeader::from_bytes(&bytes)?;
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
/// Dropping a writer does **not** touch the file: the owner says when the camera is gone ([`Writer::close`]). A drop
/// that closed the file would let a writer that is left behind (a render thread that a stop could not wait for) mark
/// `Closed` the file of the *next* writer on the same path.
///
/// # One writer at a time: leases
///
/// The seqlock assumes a single writer, but a thread that was left behind keeps its handle to the writer until it ends
/// (it may be stuck in a driver call for minutes). So a session does not write through the engine's writer but
/// through its own [`Writer::lease`]: the same mapping with its own write gate. [`Writer::retire`] shuts that gate;
/// once it has returned, no write of the lease reaches the file (a write that is running is waited for), however
/// long the thread lives on. The gate is a lock, not a flag that is looked at before a write, so there is no gap
/// between the look and the write.
///
/// # The frame event
///
/// A writer also makes the pair of frame events of [`crate::signal`] and signals them after every publish and every
/// change of state, so that the virtual camera's pump can sleep until there is something to look at. The events are a
/// hint on top of the file: when they cannot be made (or are switched off, [`WriterOptions`]) the writer works exactly
/// as it did without them, and the reason is in [`Writer::frame_event_status`]. A lease shares its owner's events, and
/// the gate above covers the signal too: a retired writer does not signal.
pub struct Writer {
    map: Arc<MappedFile>,
    /// The path the file was opened by: the virtual camera opens the file by this path, so it is what must still lead
    /// to the mapped file ([`Writer::backing_file`]).
    path: PathBuf,
    /// The write gate: `true` once the writer is retired. Every write to the file holds it for reading, so a writer
    /// that retires waits for the writes under way, and no later one starts.
    retired: RwLock<bool>,
    /// The frame events, shared with the leases; `None` when there are none (see `event_status`).
    signal: Option<Arc<FrameSignal>>,
    event_status: FrameEventStatus,
}

/// How a [`Writer`] is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriterOptions {
    /// Make and signal the frame events (default). Off for a writer that stands in for an old app (`vcam-tool feed
    /// --no-signal`, tests of a reader against a writer without events).
    pub signal: bool,
}

impl Default for WriterOptions {
    fn default() -> Self {
        WriterOptions { signal: true }
    }
}

/// Whether a [`Writer`] has frame events, and if not, why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameEventStatus {
    /// The events exist and are signalled.
    On,
    /// Switched off by [`WriterOptions::signal`].
    Disabled,
    /// They could not be made (the text says why); the writer works without them.
    Failed(String),
}

/// Whether the path of a [`Writer`]'s file still leads to the file it has mapped ([`Writer::backing_file`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackingFile {
    /// The path names the mapped file.
    Same,
    /// The path names another file, a link, or nothing (deleted, or deleted and pending): the virtual camera reads
    /// something else than the writer writes.
    Replaced,
    /// Could not tell (the open was refused for now, an opportunistic lock; the open failed for another reason such as
    /// "access denied" or a sharing violation; the file system has no file ids): nothing is known, look again later.
    Unknown,
}

impl Writer {
    /// Open the frame file at `path`, size it to [`FILE_SIZE`], map it and write a fresh header (state `closed`, no
    /// advertised types).
    ///
    /// `%ProgramData%\BetterCam\frames.bin` is made by the installer and the app has only read and write access to
    /// it (no right to create or delete anything in that folder), so for that path this opens the existing file and
    /// never creates it or its folder: a missing file is `NotFound` (the virtual camera is not installed). Any other
    /// `path` (tests, a scratch folder) is created when missing, with its folders. Either way a link, a directory or
    /// a file with more than one hard link is refused ([`secure`]), and for the default path the file must really be
    /// below `%ProgramData%`.
    ///
    /// An existing file is never truncated: a running DLL may have it mapped, and Windows refuses to shrink a mapped
    /// file. A larger existing file is used as is.
    pub fn create(path: &Path) -> io::Result<Writer> {
        Self::create_with(path, &WriterOptions::default())
    }

    /// [`Writer::create`] with options.
    pub fn create_with(path: &Path, opts: &WriterOptions) -> io::Result<Writer> {
        if is_default_path(path) {
            return Self::open_precreated_with(path, Some(&secure::program_data_dir()), opts);
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Self::open_file(path, secure::DISP_OPEN_ALWAYS, None, opts)
    }

    /// Open the file the installer made at `path` (it must exist), as [`Writer::create`] does for the default path.
    /// With `pin = Some(root)` the file must really be `path` below the canonical folder `root`.
    pub fn open_precreated(path: &Path, pin: Option<&Path>) -> io::Result<Writer> {
        Self::open_precreated_with(path, pin, &WriterOptions::default())
    }

    /// [`Writer::open_precreated`] with options.
    pub fn open_precreated_with(
        path: &Path,
        pin: Option<&Path>,
        opts: &WriterOptions,
    ) -> io::Result<Writer> {
        Self::open_file(path, secure::DISP_OPEN_EXISTING, pin, opts).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "{} does not exist: the virtual camera is not installed (install or repair it)",
                        path.display()
                    ),
                )
            } else {
                e
            }
        })
    }

    fn open_file(
        path: &Path,
        disposition: windows::Win32::Storage::FileSystem::FILE_CREATION_DISPOSITION,
        pin: Option<&Path>,
        opts: &WriterOptions,
    ) -> io::Result<Writer> {
        let expect = match pin {
            Some(root) => Some(secure::expected_path(root, path)?),
            None => None,
        };
        // exactly the rights a read-write mapping needs: the file does not give its users more (see `secure`)
        let h = secure::open_with(
            path,
            secure::ACCESS_READ_WRITE,
            secure::SHARE_ALL,
            disposition,
            false,
            FILE_ATTRIBUTE_TEMPORARY.0,
        )?;
        secure::verify_file(secure::raw(&h), expect.as_deref())?;
        let file = HANDLE(h.into_raw_handle());
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
        // The events come after `init_writer` (which zeroes the advert) and never stop the writer from opening: any
        // failure leaves a writer without events, and a reader that finds no advert polls.
        let (signal, event_status) = if opts.signal {
            match FrameSignal::create() {
                Ok(s) => {
                    s.advertise(map.shared());
                    (Some(Arc::new(s)), FrameEventStatus::On)
                }
                Err(e) => (None, FrameEventStatus::Failed(e.to_string())),
            }
        } else {
            (None, FrameEventStatus::Disabled)
        };
        Ok(Writer {
            map: Arc::new(map),
            path: path.to_path_buf(),
            retired: RwLock::new(false),
            signal,
            event_status,
        })
    }

    /// A writer on the same mapped file with a write gate of its own (see the type's notes). It shares the mapping and
    /// the frame events, so the file stays mapped (and the events open) until the last lease is gone; nothing is
    /// written or reset by making it.
    pub fn lease(&self) -> Writer {
        Writer {
            map: self.map.clone(),
            path: self.path.clone(),
            retired: RwLock::new(false),
            signal: self.signal.clone(),
            event_status: self.event_status.clone(),
        }
    }

    /// Whether this writer has frame events: [`FrameEventStatus::On`], switched off, or failed (with the reason). A
    /// failure is not an error of the writer; it just has no events.
    pub fn frame_event_status(&self) -> &FrameEventStatus {
        &self.event_status
    }

    /// The nonce the frame events are named by, when there are events.
    pub fn frame_event_nonce(&self) -> Option<u128> {
        self.signal.as_ref().map(|s| s.nonce())
    }

    /// One signal for a reader that waits, if there are events. Called with the write gate held.
    fn notify(&self) {
        if let Some(s) = &self.signal {
            s.notify(self.shared());
        }
    }

    /// Shut the write gate: from here on this writer's writes ([`Writer::set_state`], [`Writer::heartbeat`],
    /// [`Writer::set_advertised`], [`Writer::publish`], [`Writer::clear_latest`], [`Writer::close`]) do nothing, and
    /// when this returns none is still running. Reads keep working. Final.
    pub fn retire(&self) {
        *self.retired.write().unwrap_or_else(|e| e.into_inner()) = true;
    }

    /// Whether [`Writer::retire`] was called.
    pub fn is_retired(&self) -> bool {
        *self.retired.read().unwrap_or_else(|e| e.into_inner())
    }

    /// The gate held open for one write: `None` when the writer is retired. The write must be done before the guard
    /// is dropped.
    fn open_gate(&self) -> Option<RwLockReadGuard<'_, bool>> {
        let g = self.retired.read().unwrap_or_else(|e| e.into_inner());
        (!*g).then_some(g)
    }

    /// [`Writer::create`] at `%ProgramData%\BetterCam\frames.bin`: opens the file the installer made.
    pub fn create_default() -> io::Result<Writer> {
        Writer::create(&frames_path())
    }

    /// The writable view of the mapping. A write through it bypasses the write gate ([`Writer::retire`]), so it is
    /// not public: other crates read with [`Writer::header`] and [`Writer::slot_meta`] and write with the writer's own
    /// methods. Only the tests of other crates, which set up a file's contents directly (`poke`, a consumer's fields),
    /// get it, with the `test-support` feature. The ship builds (`npm run vcam:build`, `npm run tauri build`) never
    /// enable that feature; a `cargo test` build can (other crates ask for it in their `dev-dependencies`).
    #[cfg(not(feature = "test-support"))]
    pub(crate) fn shared(&self) -> &Shared {
        self.map.shared()
    }

    /// See the notes of the `pub(crate)` form: the same view, public for tests.
    #[cfg(feature = "test-support")]
    pub fn shared(&self) -> &Shared {
        self.map.shared()
    }

    /// A copy of the file's header (read only).
    pub fn header(&self) -> RawHeader {
        self.shared().header()
    }

    /// A copy of the metadata of slot `i` (read only).
    pub fn slot_meta(&self, i: usize) -> SlotMeta {
        self.shared().slot_meta(i)
    }

    /// Set the media types the camera advertises (read by the DLL at activation).
    pub fn set_advertised(&self, formats: &[Format]) {
        if let Some(_gate) = self.open_gate() {
            self.shared().set_advertised(formats);
        }
    }

    /// Set `writer_state` and refresh the heartbeat, then signal the frame event: a reader that waits sees the change
    /// at once.
    pub fn set_state(&self, state: WriterState) {
        if let Some(_gate) = self.open_gate() {
            self.shared().set_writer_state(state);
            self.shared().set_writer_heartbeat(qpc_now());
            self.notify();
        }
    }

    /// Refresh `writer_heartbeat`. Call it every frame and at least every 200 ms when idle. It does not signal: a
    /// reader that waits needs no wake-up while nothing changes.
    pub fn heartbeat(&self) {
        if let Some(_gate) = self.open_gate() {
            self.shared().set_writer_heartbeat(qpc_now());
        }
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

    /// Publish a frame (see [`Shared::publish`]) and signal the frame event once it is in place. `capture_qpc` is when
    /// the frame was captured (QPC ticks); the heartbeat is refreshed too. A publish that fails signals nothing.
    pub fn publish(
        &self,
        f: SlotFormat,
        capture_qpc: u64,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<u64, PublishError> {
        // The gate stays open for the whole publish, the copy of the frame included, and for the signal after it.
        let Some(_gate) = self.open_gate() else {
            return Err(PublishError::Retired);
        };
        let seq = self.shared().publish(f, capture_qpc, qpc_now(), fill)?;
        self.notify();
        Ok(seq)
    }

    /// Whether the path the file was opened by still leads to the file this writer has mapped: the file ids (volume
    /// and 128-bit id) of the path, opened without following a link, and of the mapped handle are compared. A deleted,
    /// replaced or relinked file is [`BackingFile::Replaced`]: the writer then writes into a file the virtual camera
    /// does not read. One open and two queries; the file is not touched.
    ///
    /// The path is opened with `FILE_READ_ATTRIBUTES` only (no read access to the data: an open that reads is what a
    /// virus scanner that scans on open wakes up for). Only an answer that says the file is gone counts as
    /// [`BackingFile::Replaced`]: the path does not exist (`NotFound`, `PathNotFound`), the mapped file is marked for
    /// deletion (an open of such a file says "access denied", so its own handle is asked), or the path names another
    /// file. Any other failure to open (access denied, a sharing violation, a lock held back) is
    /// [`BackingFile::Unknown`]: it says nothing about the file.
    pub fn backing_file(&self) -> BackingFile {
        let mine = match secure::file_identity(self.map.file) {
            Ok(id) => id,
            Err(_) => return BackingFile::Unknown,
        };
        // Delete pending: the name is gone or will go with the last handle, whatever the path opens to.
        if matches!(secure::delete_pending(self.map.file), Ok(true)) {
            return BackingFile::Replaced;
        }
        let h = match secure::open_to_inspect(&self.path) {
            Ok(h) => h,
            Err(e) => return open_failure_verdict(&e),
        };
        match secure::file_identity(secure::raw(&h)) {
            Ok(theirs) if theirs == mine => BackingFile::Same,
            Ok(_) => BackingFile::Replaced,
            Err(_) => BackingFile::Unknown,
        }
    }

    /// Forget the newest frame (see [`Shared::clear_latest`]): a reader that attaches gets no old picture, only the
    /// next one published. The state and the heartbeat are not touched, and nothing is signalled: it removes a frame
    /// and needs no wake-up.
    pub fn clear_latest(&self) {
        if let Some(_gate) = self.open_gate() {
            self.shared().clear_latest();
        }
    }

    /// Mark the file closed (the DLL shows "not running" immediately). Nothing else does that: dropping a writer
    /// leaves the file as it is.
    ///
    /// The advert of the frame events is withdrawn first (`features = 0`), then the state goes `Closed`, then the
    /// frame event is signalled so that a reader that waits sees the close at once. The withdrawal is final for this
    /// writer: a state set after a close is signalled, but readers poll for the rest of its life.
    pub fn close(&self) {
        if let Some(_gate) = self.open_gate() {
            if let Some(s) = &self.signal {
                s.withdraw(self.shared());
            }
            self.shared().set_writer_state(WriterState::Closed);
            self.shared().set_writer_heartbeat(qpc_now());
            self.notify();
        }
    }
}

/// What a failed open of the frame file's path says about the file ([`Writer::backing_file`]): only "there is nothing
/// at that path" (`ERROR_FILE_NOT_FOUND`, `ERROR_PATH_NOT_FOUND`) means the file is gone.
fn open_failure_verdict(e: &io::Error) -> BackingFile {
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PATH_NOT_FOUND: i32 = 3;
    if secure::is_refused(e) {
        // held back for now (an opportunistic lock), or refused for what the file is: nothing is known
        return BackingFile::Unknown;
    }
    match e.raw_os_error() {
        Some(ERROR_FILE_NOT_FOUND) | Some(ERROR_PATH_NOT_FOUND) => BackingFile::Replaced,
        _ => BackingFile::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{SUBTYPE_NV12, SUBTYPE_RGB32};
    use crate::seqlock::{Disposition, Read};
    use crate::validate::validate_header;
    use windows::Win32::Storage::FileSystem::GetFileAttributesW;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bettercam-proto-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// A fresh folder for one test, with the temp dir below it as the trusted root of a pin.
    fn sandbox(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("bettercam-writer-{}", std::process::id()));
        let dir = root.join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (root, dir)
    }

    fn junction(link: &Path, target: &Path) {
        let out = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(out.status.success(), "mklink /J failed");
    }

    #[test]
    fn the_writer_notices_a_frame_file_that_was_deleted_or_replaced() {
        // untouched
        let path = temp_file("backing-same.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        assert_eq!(w.backing_file(), BackingFile::Same);
        // writing and mapping leave it the same file
        w.set_state(WriterState::Live);
        assert_eq!(w.backing_file(), BackingFile::Same);

        // deleted: the mapping lives on (the handle allows delete), the path leads nowhere
        std::fs::remove_file(&path).unwrap();
        assert_eq!(w.backing_file(), BackingFile::Replaced);
        // and a new file at the path is another file
        let other = Writer::create(&path).unwrap();
        assert_eq!(other.backing_file(), BackingFile::Same);
        assert_eq!(w.backing_file(), BackingFile::Replaced);
        drop(w);

        // renamed over by another file
        let spare = temp_file("backing-spare.bin");
        let _ = std::fs::remove_file(&spare);
        std::fs::write(&spare, vec![0u8; FILE_SIZE as usize]).unwrap();
        assert_eq!(other.backing_file(), BackingFile::Same);
        std::fs::rename(&spare, &path).unwrap();
        assert_eq!(other.backing_file(), BackingFile::Replaced);
    }

    #[test]
    fn the_writer_can_forget_its_newest_frame_and_a_reader_gets_the_next_one() {
        let path = temp_file("clear-latest.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let fmt = SlotFormat {
            width: 16,
            height: 16,
            stride: 16,
            subtype: SUBTYPE_NV12,
        };
        w.publish(fmt, 111, |d| d.fill(1)).unwrap();
        // a second process's view of the same file, as the DLL has it
        let m = MappedFile::open(&path, Access::ReadWrite).unwrap();
        let v = validate_header(&m.shared().header(), m.len() as u64, None, None).unwrap();
        let first = |m: &MappedFile| {
            let mut got = None;
            let _ = m.shared().read_latest(&v, |meta, _| {
                got = Some(meta.capture_qpc);
                Disposition::Taken
            });
            got
        };
        assert_eq!(first(&m), Some(111));
        w.clear_latest();
        assert_eq!(m.shared().header().latest_slot, crate::NO_SLOT);
        assert_eq!(
            first(&m),
            None,
            "an attaching reader is offered no old frame"
        );
        assert_eq!(
            m.shared().header().writer_state,
            WriterState::Live as u32,
            "the state is the caller's"
        );
        w.publish(fmt, 222, |d| d.fill(2)).unwrap();
        assert_eq!(
            first(&m),
            Some(222),
            "the next frame is the first one it can take"
        );
    }

    #[test]
    fn a_writer_opens_the_precreated_file_and_never_creates_one() {
        let (root, dir) = sandbox("precreated");
        let f = dir.join("frames.bin");
        // the installer's file: sized, no header
        {
            let file = std::fs::File::create(&f).unwrap();
            file.set_len(FILE_SIZE).unwrap();
        }
        let w = Writer::open_precreated(&f, Some(&root)).unwrap();
        assert_eq!(w.shared().header().latest_slot, crate::NO_SLOT);
        assert_eq!(w.shared().header().writer_state, WriterState::Closed as u32);
        w.set_state(WriterState::Live);
        drop(w);
        // a short pre-created file is grown through the handle
        std::fs::write(&f, b"").unwrap();
        let w = Writer::open_precreated(&f, Some(&root)).unwrap();
        assert_eq!(std::fs::metadata(&f).unwrap().len(), FILE_SIZE);
        drop(w);

        // a missing file is a clean NotFound that says what to do; nothing is created
        let missing = dir.join("nope").join("frames.bin");
        let e = Writer::open_precreated(&missing, Some(&root))
            .err()
            .expect("must fail");
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
        assert!(e.to_string().contains("not installed"), "{e}");
        assert!(
            !missing.parent().unwrap().exists(),
            "the folder was created"
        );
        let missing = dir.join("absent.bin");
        assert!(Writer::open_precreated(&missing, None).is_err());
        assert!(!missing.exists(), "the file was created");
    }

    #[test]
    fn only_the_production_path_takes_the_strict_route() {
        // `create` on this path opens the installer's file and never makes one; every other path (tests) is created
        assert!(is_default_path(&frames_path()));
        assert!(is_default_path(Path::new(
            &frames_path().to_string_lossy().to_uppercase()
        )));
        assert!(!is_default_path(&temp_file("frames.bin")));
        assert!(!is_default_path(&data_dir()));
    }

    #[test]
    fn a_writer_refuses_links_and_hard_links() {
        let (root, dir) = sandbox("links");
        // a hard-linked file
        let f = dir.join("frames.bin");
        std::fs::write(&f, vec![0u8; 8192]).unwrap();
        std::fs::hard_link(&f, dir.join("other.bin")).unwrap();
        let e = Writer::open_precreated(&f, Some(&root)).err().unwrap();
        assert!(secure::is_refused(&e), "{e}");
        let e = Writer::create(&f).err().unwrap();
        assert!(secure::is_refused(&e), "{e}");
        assert_eq!(
            std::fs::metadata(&f).unwrap().len(),
            8192,
            "must not be touched"
        );

        // a junction in the folder chain: refused with a pin (the production path), the target stays untouched
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("frames.bin"), vec![0u8; 8192]).unwrap();
        let link = dir.join("link");
        junction(&link, &real);
        let e = Writer::open_precreated(&link.join("frames.bin"), Some(&root))
            .err()
            .unwrap();
        assert!(secure::is_refused(&e), "{e}");
        assert_eq!(
            std::fs::metadata(real.join("frames.bin")).unwrap().len(),
            8192
        );
        let _ = std::fs::remove_dir(&link);

        // a symbolic link as the file (when the account may make one)
        let target = dir.join("target.bin");
        std::fs::write(&target, vec![0u8; 8192]).unwrap();
        let sym = dir.join("sym.bin");
        if std::os::windows::fs::symlink_file(&target, &sym).is_ok() {
            let e = Writer::open_precreated(&sym, Some(&root)).err().unwrap();
            assert!(secure::is_refused(&e), "{e}");
            let e = Writer::create(&sym).err().unwrap();
            assert!(secure::is_refused(&e), "{e}");
            assert_eq!(std::fs::metadata(&target).unwrap().len(), 8192);
        }
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
        // closing is the owner's act: the file is `Closed` after `close`, not after a drop
        w.close();
        assert_eq!(
            ro.shared().header().writer_state,
            WriterState::Closed as u32
        );
    }

    /// M-2: a writer that is dropped late (a render thread that was left behind, after the engine was rebuilt on the
    /// same path) must not mark the file of the new writer `Closed`.
    #[test]
    fn dropping_an_old_writer_leaves_the_new_writers_file_alone() {
        let path = temp_file("late-drop.bin");
        let _ = std::fs::remove_file(&path);
        let old = std::sync::Arc::new(Writer::create(&path).unwrap());
        let straggler = old.lease();
        old.set_state(WriterState::Live);
        // the engine is rebuilt: a new writer on the same path starts the header again
        let new = Writer::create(&path).unwrap();
        new.set_state(WriterState::Live);
        // the old handles go, the straggler last
        drop(old);
        drop(straggler);
        assert_eq!(
            new.shared().header().writer_state,
            WriterState::Live as u32,
            "the last drop of an old writer closed the file under the new one"
        );
    }

    #[test]
    fn a_retired_lease_writes_nothing_and_the_owner_still_does() {
        let path = temp_file("retire.bin");
        let _ = std::fs::remove_file(&path);
        let owner = Writer::create(&path).unwrap();
        let lease = owner.lease();
        let fmt = SlotFormat {
            width: 16,
            height: 16,
            stride: 16,
            subtype: SUBTYPE_NV12,
        };
        lease.set_state(WriterState::Live);
        lease.publish(fmt, 111, |d| d.fill(1)).unwrap();
        assert_eq!(owner.shared().header().writer_frames, 1);

        lease.retire();
        assert!(lease.is_retired() && !owner.is_retired());
        // every write is refused or ignored
        assert_eq!(
            lease.publish(fmt, 222, |_| panic!("the fill of a retired writer ran")),
            Err(PublishError::Retired)
        );
        lease.set_state(WriterState::Paused);
        lease.close();
        lease.heartbeat();
        lease.set_advertised(&[]);
        lease.clear_latest();
        let h = owner.shared().header();
        assert_eq!(h.writer_state, WriterState::Live as u32);
        assert_eq!(
            h.writer_frames, 1,
            "a retired writer's frame reached the file"
        );
        assert_ne!(
            h.latest_slot,
            crate::NO_SLOT,
            "a retired writer cleared the newest frame"
        );
        let beat = h.writer_heartbeat;
        lease.heartbeat();
        assert_eq!(owner.shared().header().writer_heartbeat, beat);
        // reads still work, and the owner is not affected
        assert_eq!(lease.backing_file(), BackingFile::Same);
        owner.publish(fmt, 333, |d| d.fill(3)).unwrap();
        assert_eq!(owner.shared().header().writer_frames, 2);
        owner.close();
        assert_eq!(
            owner.shared().header().writer_state,
            WriterState::Closed as u32
        );
    }

    /// The gate is a lock: a publish that is running finishes before `retire` returns, and none starts after it.
    #[test]
    fn retiring_waits_for_the_publish_that_is_running() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let path = temp_file("retire-wait.bin");
        let _ = std::fs::remove_file(&path);
        let owner = Writer::create(&path).unwrap();
        let lease = std::sync::Arc::new(owner.lease());
        let fmt = SlotFormat {
            width: 16,
            height: 16,
            stride: 16,
            subtype: SUBTYPE_NV12,
        };
        let (in_fill, filled) = (
            std::sync::Arc::new(AtomicBool::new(false)),
            std::sync::Arc::new(AtomicBool::new(false)),
        );
        let t = {
            let (lease, in_fill, filled) = (lease.clone(), in_fill.clone(), filled.clone());
            std::thread::spawn(move || {
                lease
                    .publish(fmt, 5, |d| {
                        in_fill.store(true, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(150));
                        d.fill(5);
                        filled.store(true, Ordering::SeqCst);
                    })
                    .unwrap();
            })
        };
        while !in_fill.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        lease.retire();
        assert!(
            filled.load(Ordering::SeqCst),
            "retire returned while a publish was still writing"
        );
        t.join().unwrap();
        assert_eq!(owner.shared().header().writer_frames, 1);
        assert!(lease.publish(fmt, 6, |_| {}).is_err());
    }

    #[test]
    fn only_a_missing_path_or_a_deleted_file_counts_as_replaced() {
        // the verdict on a failed open
        let denied = io::Error::from_raw_os_error(5);
        let sharing = io::Error::from_raw_os_error(32);
        let other = io::Error::other("something");
        assert_eq!(open_failure_verdict(&denied), BackingFile::Unknown);
        assert_eq!(open_failure_verdict(&sharing), BackingFile::Unknown);
        assert_eq!(open_failure_verdict(&other), BackingFile::Unknown);
        assert_eq!(
            open_failure_verdict(&secure::refused("held back")),
            BackingFile::Unknown
        );
        assert_eq!(
            open_failure_verdict(&io::Error::from_raw_os_error(2)),
            BackingFile::Replaced
        );
        assert_eq!(
            open_failure_verdict(&io::Error::from_raw_os_error(3)),
            BackingFile::Replaced
        );

        // a file that is marked for deletion but still open keeps its name, and an open of it says "access denied":
        // only the handle that is open knows (a mapped file cannot be marked the classic way, so a plain file shows it)
        let path = temp_file("backing-pending.bin");
        std::fs::write(&path, b"x").unwrap();
        let h = secure::open_with(
            &path,
            secure::ACCESS_READ,
            secure::SHARE_ALL,
            secure::DISP_OPEN_EXISTING,
            false,
            0,
        )
        .unwrap();
        assert!(!secure::delete_pending(secure::raw(&h)).unwrap());
        secure::mark_delete_pending_classic(&path).unwrap();
        assert!(secure::delete_pending(secure::raw(&h)).unwrap());
        let e = secure::open_to_inspect(&path).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(5), "{e}");
        assert_eq!(
            open_failure_verdict(&e),
            BackingFile::Unknown,
            "an open that is refused with 'access denied' says nothing"
        );
        drop(h);
    }

    /// The path is opened for its attributes only.
    #[test]
    fn the_frame_file_is_inspected_without_read_access() {
        let path = temp_file("backing-inspect.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        let h = secure::open_to_inspect(&path).unwrap();
        // no data access: a read through that handle is refused
        let mut f = std::fs::File::from(h);
        let mut b = [0u8; 4];
        assert!(
            std::io::Read::read(&mut f, &mut b).is_err(),
            "the inspect handle can read data"
        );
        assert_eq!(w.backing_file(), BackingFile::Same);
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

    // ---- the frame event ------------------------------------------------------------------------------------

    use crate::header::{wake_mode, FEATURE_FRAME_EVENT};
    use crate::signal::tests::{is_set, not_found, open_for, squat, with_nonces, What};
    use crate::signal::{open_frame_events, Step};

    const MODIFY_OR_WAIT: u32 = 0x0010_0000;

    fn fmt16() -> SlotFormat {
        SlotFormat {
            width: 16,
            height: 16,
            stride: 16,
            subtype: SUBTYPE_NV12,
        }
    }

    fn trace(w: &Writer) -> Vec<crate::signal::tests::Seen> {
        w.signal.as_ref().unwrap().trace.lock().unwrap().clone()
    }

    fn handles(w: &Writer) -> [windows::Win32::Foundation::HANDLE; 2] {
        let s = w.signal.as_ref().unwrap();
        [s.handle(0), s.handle(1)]
    }

    #[test]
    fn a_new_writer_advertises_its_events() {
        let path = temp_file("ev-advert.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        assert_eq!(w.frame_event_status(), &FrameEventStatus::On);
        let h = w.header();
        let nonce = w.frame_event_nonce().unwrap();
        assert_eq!(h.features, FEATURE_FRAME_EVENT);
        assert_eq!(
            ((h.event_nonce_hi as u128) << 64) | h.event_nonce_lo as u128,
            nonce
        );
        assert_eq!(h.signal_seq, 0, "nothing is signalled by opening");
        // a reader validates it as an advert
        let m = MappedFile::open(&path, Access::ReadOnly).unwrap();
        let v = validate_header(&m.shared().header(), m.len() as u64, None, None).unwrap();
        assert_eq!(v.wake.map(|w| (w.nonce, w.signal_seq)), Some((nonce, 0)));
        let ev = open_frame_events(nonce).unwrap();
        drop(ev);
        // the DLL's diagnostics start at zero
        assert_eq!((h.wake_mode, h.wake_events), (wake_mode::POLLING, 0));
    }

    #[test]
    fn the_writer_signals_after_publishes_and_state_changes_and_after_nothing_else() {
        let path = temp_file("ev-order.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        let seq = |w: &Writer| w.header().signal_seq;
        assert_eq!(seq(&w), 0);

        w.set_state(WriterState::Live);
        assert_eq!(seq(&w), 1, "a state change signals");
        w.publish(fmt16(), 5, |d| d.fill(1)).unwrap();
        assert_eq!(seq(&w), 2, "a publish signals");
        w.publish(fmt16(), 6, |d| d.fill(1)).unwrap();
        assert_eq!(seq(&w), 3);

        // no signal: the heartbeat, clearing the newest frame, a rejected frame, setting the advertised types
        w.heartbeat();
        w.clear_latest();
        let odd = SlotFormat {
            width: 15,
            ..fmt16()
        };
        assert!(w.publish(odd, 7, |_| panic!("no fill")).is_err());
        w.set_advertised(&crate::advertised_for_output(1280, 720, 30));
        assert_eq!(seq(&w), 3);

        w.set_state(WriterState::Paused);
        w.set_state(WriterState::Live);
        assert_eq!(seq(&w), 5);

        // the order of every signal: reset next, store, set; the publish was in place before the first step
        let t = trace(&w);
        assert_eq!(t.len(), 5 * 3);
        for (i, chunk) in t.chunks(3).enumerate() {
            let n = i as u64 + 1;
            let steps: Vec<What> = chunk.iter().map(|s| s.what).collect();
            assert_eq!(
                steps,
                vec![
                    What::Step(Step::Reset(((n + 1) & 1) as usize)),
                    What::Step(Step::Store(n)),
                    What::Step(Step::Set((n & 1) as usize)),
                ],
                "signal {n}"
            );
        }
        // signal 2 and 3 are the publishes: the frame (latest slot, count) was in the file before the reset
        assert_eq!(t[3].frames, 1);
        assert_ne!(t[3].latest, crate::NO_SLOT);
        assert_eq!(t[6].frames, 2);
        // signals 1, 4 and 5 are state changes: the state was already stored
        assert_eq!(t[0].state, WriterState::Live as u32);
        assert_eq!(t[9].state, WriterState::Paused as u32);
        assert_eq!(t[12].state, WriterState::Live as u32);
        // the last signal's event is the one that is set
        let e = handles(&w);
        assert!(is_set(e[1]) && !is_set(e[0]));
    }

    #[test]
    fn close_withdraws_the_advert_before_the_state_goes_closed_and_then_signals() {
        let path = temp_file("ev-close.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        w.publish(fmt16(), 1, |d| d.fill(1)).unwrap();
        assert_eq!(w.header().signal_seq, 2);
        w.close();
        let h = w.header();
        assert_eq!(h.features, 0);
        assert_eq!(h.writer_state, WriterState::Closed as u32);
        assert_eq!(h.signal_seq, 3, "the close is signalled");
        let t = trace(&w);
        let close_part = &t[6..];
        assert_eq!(close_part.len(), 4, "{close_part:?}");
        // the withdrawal came first, while the state was still Live and the advert still there
        assert_eq!(close_part[0].what, What::Withdraw);
        assert_eq!(close_part[0].features, FEATURE_FRAME_EVENT);
        assert_eq!(close_part[0].state, WriterState::Live as u32);
        // then the three steps of the signal, with the advert gone and the state stored
        for s in &close_part[1..] {
            assert_eq!(s.features, 0);
            assert_eq!(s.state, WriterState::Closed as u32);
        }
        assert_eq!(close_part[1].what, What::Step(Step::Reset(0)));
        assert_eq!(close_part[2].what, What::Step(Step::Store(3)));
        assert_eq!(close_part[3].what, What::Step(Step::Set(1)));
        // a reader no longer finds an advert
        let m = MappedFile::open(&path, Access::ReadOnly).unwrap();
        let v = validate_header(&m.shared().header(), m.len() as u64, None, None).unwrap();
        assert_eq!(v.wake, None);
        assert_eq!(v.state, WriterState::Closed);
    }

    #[test]
    fn setting_the_state_closed_keeps_the_advert_and_signals() {
        // the engine goes Closed and Live again on one writer; only `close` is final
        let path = temp_file("ev-state-closed.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        w.set_state(WriterState::Closed);
        assert_eq!(w.header().features, FEATURE_FRAME_EVENT);
        assert_eq!(w.header().signal_seq, 2);
        w.set_state(WriterState::Live);
        assert_eq!(w.header().signal_seq, 3);
    }

    #[test]
    fn a_retired_lease_signals_nothing_and_the_owner_still_does() {
        let path = temp_file("ev-retire.bin");
        let _ = std::fs::remove_file(&path);
        let owner = Writer::create(&path).unwrap();
        let lease = owner.lease();
        lease.set_state(WriterState::Live);
        lease.publish(fmt16(), 1, |d| d.fill(1)).unwrap();
        assert_eq!(
            owner.header().signal_seq,
            2,
            "a lease signals through the shared events"
        );
        let e = handles(&owner);
        let (e0, e1) = (is_set(e[0]), is_set(e[1]));

        lease.retire();
        let before = trace(&owner).len();
        assert!(lease.publish(fmt16(), 2, |_| panic!("no fill")).is_err());
        lease.set_state(WriterState::Paused);
        lease.close();
        lease.heartbeat();
        lease.clear_latest();
        let h = owner.header();
        assert_eq!(h.signal_seq, 2, "a retired lease signalled");
        assert_eq!(
            trace(&owner).len(),
            before,
            "a retired lease touched the events"
        );
        assert_eq!((is_set(e[0]), is_set(e[1])), (e0, e1));
        assert_eq!(
            h.features, FEATURE_FRAME_EVENT,
            "a retired close withdrew the advert"
        );
        assert_eq!(h.writer_state, WriterState::Live as u32);

        // the owner is not affected
        owner.publish(fmt16(), 3, |d| d.fill(3)).unwrap();
        assert_eq!(owner.header().signal_seq, 3);
        owner.set_state(WriterState::Paused);
        assert_eq!(owner.header().signal_seq, 4);
        // the counter is shared: the lease and the owner never reuse a number
        let lease2 = owner.lease();
        lease2.set_state(WriterState::Live);
        assert_eq!(owner.header().signal_seq, 5);
    }

    #[test]
    fn a_writer_whose_events_cannot_be_made_works_as_before() {
        let path = temp_file("ev-fail.bin");
        let _ = std::fs::remove_file(&path);
        // every nonce the generator offers has a taken name
        let nonces: Vec<u128> = (1..=3).map(|i| 0xDEAD_0000 + i).collect();
        let _squatters: Vec<_> = nonces.iter().map(|&n| squat(n, 0, None)).collect();
        let w = with_nonces(nonces.iter().map(|&n| Ok(n)).collect(), || {
            Writer::create(&path).unwrap()
        });
        match w.frame_event_status() {
            FrameEventStatus::Failed(why) => assert!(why.contains("taken"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(w.frame_event_nonce(), None);
        let h = w.header();
        assert_eq!(h.features, 0, "the feature bit is set without events");
        assert_eq!(
            (h.event_nonce_lo, h.event_nonce_hi, h.signal_seq),
            (0, 0, 0)
        );
        // nothing was left open under the names the writer tried
        for &n in &nonces {
            assert!(not_found(&open_for(n, 1, MODIFY_OR_WAIT)));
        }
        // and it publishes, changes state, leases and closes exactly as before, advertising nothing
        w.set_advertised(&crate::advertised_for_output(1280, 720, 30));
        w.set_state(WriterState::Live);
        w.publish(fmt16(), 9, |d| d.fill(7)).unwrap();
        let lease = w.lease();
        assert_eq!(lease.frame_event_status(), w.frame_event_status());
        lease.publish(fmt16(), 10, |d| d.fill(8)).unwrap();
        let m = MappedFile::open(&path, Access::ReadOnly).unwrap();
        let v = validate_header(&m.shared().header(), m.len() as u64, None, None).unwrap();
        assert_eq!(
            (v.state, v.writer_frames, v.wake),
            (WriterState::Live, 2, None)
        );
        assert_eq!(m.shared().header().signal_seq, 0);
        w.close();
        assert_eq!(m.shared().header().writer_state, WriterState::Closed as u32);
        assert_eq!(m.shared().header().features, 0);
    }

    #[test]
    fn a_writer_without_signals_is_the_old_writer() {
        let path = temp_file("ev-disabled.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create_with(&path, &WriterOptions { signal: false }).unwrap();
        assert_eq!(w.frame_event_status(), &FrameEventStatus::Disabled);
        w.set_state(WriterState::Live);
        w.publish(fmt16(), 1, |d| d.fill(1)).unwrap();
        w.close();
        let h = w.header();
        assert_eq!(
            (h.features, h.signal_seq, h.event_nonce_lo, h.event_nonce_hi),
            (0, 0, 0, 0)
        );
        assert_eq!(h.writer_frames, 1);
        assert_eq!(WriterOptions::default(), WriterOptions { signal: true });
    }

    #[test]
    fn a_writer_that_starts_over_clears_the_advert_of_the_one_before() {
        let path = temp_file("ev-restart.bin");
        let _ = std::fs::remove_file(&path);
        let first = Writer::create(&path).unwrap();
        first.set_state(WriterState::Live);
        first.publish(fmt16(), 1, |d| d.fill(1)).unwrap();
        let n1 = first.frame_event_nonce().unwrap();
        assert_eq!(first.header().signal_seq, 2);
        // an old app (no events) starts over the same file: its init zeroes the advert and the count
        let old = Writer::create_with(&path, &WriterOptions { signal: false }).unwrap();
        let h = old.header();
        assert_eq!(
            (h.features, h.event_nonce_lo, h.event_nonce_hi, h.signal_seq),
            (0, 0, 0, 0)
        );
        // a new app starts over it: a new nonce, the count from 0
        let second = Writer::create(&path).unwrap();
        let n2 = second.frame_event_nonce().unwrap();
        assert_ne!(n1, n2);
        let h = second.header();
        assert_eq!(h.features, FEATURE_FRAME_EVENT);
        assert_eq!(
            ((h.event_nonce_hi as u128) << 64) | h.event_nonce_lo as u128,
            n2
        );
        assert_eq!(h.signal_seq, 0);
        // both pairs exist, each named by its own nonce
        assert!(open_frame_events(n1).is_ok() && open_frame_events(n2).is_ok());
    }

    #[test]
    fn dropping_the_writer_and_its_leases_closes_the_event_handles() {
        let path = temp_file("ev-drop.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        let n = w.frame_event_nonce().unwrap();
        let lease = w.lease();
        // a reader's handles keep the events alive; the writer's do not
        let reader = open_frame_events(n).unwrap();
        drop(w);
        assert!(
            open_frame_events(n).is_ok(),
            "the lease still holds the events"
        );
        drop(lease);
        assert!(
            open_frame_events(n).is_ok(),
            "the reader still holds the events"
        );
        drop(reader);
        assert!(
            not_found(&open_for(n, 0, MODIFY_OR_WAIT))
                && not_found(&open_for(n, 1, MODIFY_OR_WAIT)),
            "an event handle of the writer leaked"
        );
        // 50 writers on one file, each dropped: no event of any of them is left
        let mut nonces = Vec::new();
        for _ in 0..50 {
            let w = Writer::create(&path).unwrap();
            nonces.push(w.frame_event_nonce().unwrap());
            w.set_state(WriterState::Live);
        }
        for n in nonces {
            assert!(not_found(&open_for(n, 0, MODIFY_OR_WAIT)));
        }
    }
}
