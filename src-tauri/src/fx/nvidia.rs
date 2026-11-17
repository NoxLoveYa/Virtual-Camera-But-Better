//! NVIDIA Video Effects (plan 4.6): is the runtime usable on this PC, and the installer for it. The effects themselves
//! (background removal, background blur) are built on [`Runtime`] by their own filter files.
//!
//! * Detection loads nothing heavy: the GPU comes from `nvidia-smi` ([`detect_gpu`]), the runtime folder is looked at
//!   on disk ([`inspect`]), and only the version needs the DLL ([`probe`] loads it for a moment and lets it go).
//! * [`Runtime`] is the loaded runtime: `NVCVImage.dll` and `NVVideoEffects.dll` from the runtime folder, found the way
//!   OBS finds them (`nvvfx-load.h`). [`Runtime::open`] hands out one shared copy; it stays loaded as long as somebody
//!   holds the `Arc`, and unloads when the last holder drops it. Function pointers taken from it are valid only while
//!   the `Arc` they came from is alive: keep both together.
//! * [`run_install`] downloads NVIDIA's redistributable, accepts it by a valid NVIDIA signature or by its pinned
//!   SHA-256 ([`pins`]) and runs it (`install_nvidia.ps1`).

use libloading::os::windows::{Library, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR};
use serde::Serialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// The oldest runtime Better Cam accepts, packed like OBS packs versions (`MIN_VFX_SDK_VERSION`): major, minor,
/// build, revision, one byte each. Equal to 0.7.6.
pub const MIN_VERSION: u32 = (7 << 16) | (6 << 8);

/// What the installer table below downloads (NVIDIA's resources page lists this as the newest for RTX 20 to 50).
pub const INSTALLER_VERSION: &str = "0.7.6";

/// Files the runtime folder must hold.
const VIDEOFX_DLL: &str = "NVVideoEffects.dll";
const CVIMAGE_DLL: &str = "NVCVImage.dll";

/// The model files (in `models\`) the background effects run on: NVIDIA's "AIGS" segmentation network, one engine per
/// mode (`m0` to `m3`), built for one GPU generation. The generation is in the name as its SM number (7.5 → 75), so the
/// redistributable of another generation brings other files; the `_1_4_8` batch variants are not needed.
pub fn required_models(arch: &str) -> Vec<String> {
    let sm = match arch {
        "turing" => "75",
        "ampere" => "86",
        "ada" => "89",
        "blackwell" => "100",
        _ => return Vec::new(),
    };
    (0..4).map(|m| format!("AIGS_288x512_{sm}_m{m}.engine.trtpkg")).collect()
}

/// `NvCV_Status` of a call that worked.
pub const NVCV_SUCCESS: i32 = 0;

// ---- version -----------------------------------------------------------------------------------------------------

/// `0.7.6`, or `0.7.6.1` when the revision is not zero.
pub fn format_version(v: u32) -> String {
    let (major, minor, build, rev) = (v >> 24, (v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff);
    if rev == 0 {
        format!("{major}.{minor}.{build}")
    } else {
        format!("{major}.{minor}.{build}.{rev}")
    }
}

/// Packed versions compare as numbers.
pub fn version_ok(v: u32) -> bool {
    v >= MIN_VERSION
}

// ---- GPU ---------------------------------------------------------------------------------------------------------

/// The installed NVIDIA GPU that can run the effects.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Gpu {
    pub name: String,
    /// Which redistributable build fits this GPU: turing, ampere, ada or blackwell.
    pub arch: &'static str,
}

/// The redistributable build for a GPU: an RTX card (tensor cores) of compute capability 7.5 (Turing), 8.x (Ampere,
/// 8.9 is Ada) or 10.x and 12.x (Blackwell). A GTX 16 series card is also 7.5 but has no tensor cores; its name has
/// no "RTX" and NVIDIA's runtime does not support it.
pub fn arch_for(name: &str, compute_cap: &str) -> Option<&'static str> {
    if !name.to_ascii_uppercase().contains("RTX") {
        return None;
    }
    let (major, minor) = compute_cap.trim().split_once('.')?;
    match (major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?) {
        (7, 5) => Some("turing"),
        (8, 9) => Some("ada"),
        (8, _) => Some("ampere"),
        (10 | 12, _) => Some("blackwell"),
        _ => None,
    }
}

/// The first usable GPU in the output of `nvidia-smi --query-gpu=name,compute_cap --format=csv,noheader`.
pub fn parse_smi(output: &str) -> Option<Gpu> {
    output.lines().find_map(|l| {
        let (name, cap) = l.rsplit_once(',')?;
        Some(Gpu { name: name.trim().into(), arch: arch_for(name, cap)? })
    })
}

/// Asks `nvidia-smi`, which can take a second: call it off the UI thread. A GPU found once is remembered.
pub fn detect_gpu() -> Option<Gpu> {
    use std::os::windows::process::CommandExt;
    static FOUND: OnceLock<Gpu> = OnceLock::new();
    if let Some(g) = FOUND.get() {
        return Some(g.clone());
    }
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=name,compute_cap", "--format=csv,noheader"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .ok()?;
    let gpu = parse_smi(&String::from_utf8_lossy(&out.stdout))?;
    Some(FOUND.get_or_init(|| gpu).clone())
}

// ---- where the runtime is ----------------------------------------------------------------------------------------

/// `%NV_VIDEO_EFFECTS_PATH%` when set, else `%ProgramFiles%\NVIDIA Corporation\NVIDIA Video Effects` (OBS's rule).
pub fn runtime_dir_from(env_path: Option<OsString>, program_files: Option<OsString>) -> PathBuf {
    match env_path.filter(|p| !p.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(program_files.filter(|p| !p.is_empty()).unwrap_or_else(|| r"C:\Program Files".into()))
            .join(r"NVIDIA Corporation\NVIDIA Video Effects"),
    }
}

pub fn runtime_dir() -> PathBuf {
    runtime_dir_from(std::env::var_os("NV_VIDEO_EFFECTS_PATH"), std::env::var_os("ProgramFiles"))
}

/// What is on disk in a runtime folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layout {
    pub dir: PathBuf,
    pub dir_exists: bool,
    pub videofx: bool,
    pub cvimage: bool,
    /// The folder `models` exists and holds at least one file.
    pub has_models: bool,
    /// Required model files that are not there ([`required_models`] of the GPU's generation).
    pub missing_models: Vec<String>,
}

impl Layout {
    pub fn complete(&self) -> bool {
        self.dir_exists && self.videofx && self.cvimage && self.has_models && self.missing_models.is_empty()
    }
}

/// Looks at `dir`. `arch` is the GPU's generation, which decides the model files that must be there; without a GPU
/// only a non-empty `models` folder is asked for.
pub fn inspect(dir: &Path, arch: Option<&str>) -> Layout {
    let models = dir.join("models");
    let has_models = std::fs::read_dir(&models).map(|mut rd| rd.any(|e| e.is_ok_and(|e| e.path().is_file()))).unwrap_or(false);
    Layout {
        dir: dir.to_path_buf(),
        dir_exists: dir.is_dir(),
        videofx: dir.join(VIDEOFX_DLL).is_file(),
        cvimage: dir.join(CVIMAGE_DLL).is_file(),
        has_models,
        missing_models: arch.map(required_models).unwrap_or_default().into_iter().filter(|m| !models.join(m).is_file()).collect(),
    }
}

// ---- status ------------------------------------------------------------------------------------------------------

/// `nvidia_status`: `available` is true only when an RTX GPU, the runtime files, a good version and the models are all
/// there. `detail` says in plain words what is missing; `version` is the runtime's own, when it could be read.
#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct Status {
    pub available: bool,
    pub detail: String,
    pub version: Option<String>,
}

/// The verdict from facts that were already collected. `version` is the result of reading it from the DLL, `None`
/// when that was not tried (no GPU or an incomplete folder).
pub fn status_from(gpu: Option<&Gpu>, layout: &Layout, version: Option<&Result<u32, String>>) -> Status {
    let no = |detail: String, version: Option<u32>| Status { available: false, detail, version: version.map(format_version) };
    if gpu.is_none() {
        return no("Needs an NVIDIA RTX GPU (GeForce RTX 20 series or newer).".into(), None);
    }
    let dir = layout.dir.display();
    if !layout.dir_exists {
        return no(
            format!("The NVIDIA Video Effects runtime is not installed (looked in {dir}). Install it to use NVIDIA's video effects."),
            None,
        );
    }
    for (present, file) in [(layout.videofx, VIDEOFX_DLL), (layout.cvimage, CVIMAGE_DLL)] {
        if !present {
            return no(format!("Found {dir} but {file} is missing. Reinstall the NVIDIA Video Effects runtime."), None);
        }
    }
    if !layout.has_models {
        return no(format!("Found {dir} but its models folder is missing or empty. Reinstall the NVIDIA Video Effects runtime."), None);
    }
    if !layout.missing_models.is_empty() {
        return no(
            format!("Found {dir} but these model files are missing: {}. Reinstall the NVIDIA Video Effects runtime.", layout.missing_models.join(", ")),
            None,
        );
    }
    match version {
        None => no("The NVIDIA Video Effects version was not checked.".into(), None),
        Some(Err(e)) => no(format!("Found {dir} but could not load it: {e}. Reinstall the NVIDIA Video Effects runtime."), None),
        Some(Ok(v)) if !version_ok(*v) => no(
            format!(
                "NVIDIA Video Effects {} is installed; Better Cam needs {} or newer. Install the current runtime.",
                format_version(*v),
                format_version(MIN_VERSION)
            ),
            Some(*v),
        ),
        Some(Ok(v)) => Status { available: true, detail: format!("NVIDIA Video Effects {} found at {dir}", format_version(*v)), version: Some(format_version(*v)) },
    }
}

/// Reads the facts and gives the verdict. Loads the runtime DLL for a moment (and caches what it said until the file
/// changes), so call it off the UI thread.
pub fn probe() -> Status {
    let gpu = detect_gpu();
    let layout = inspect(&runtime_dir(), gpu.as_ref().map(|g| g.arch));
    let version = (gpu.is_some() && layout.complete()).then(|| read_version(&layout.dir));
    status_from(gpu.as_ref(), &layout, version.as_ref())
}

/// What `NvVFX_GetVersion` said for a DLL, remembered with the file's size and time so a reinstall is noticed.
static VERSION_CACHE: Mutex<Option<(DllKey, u32)>> = Mutex::new(None);

/// A DLL's path, size and modification time.
type DllKey = (PathBuf, u64, Option<std::time::SystemTime>);

fn read_version(dir: &Path) -> Result<u32, String> {
    let dll = dir.join(VIDEOFX_DLL);
    let meta = std::fs::metadata(&dll).map_err(|e| format!("{VIDEOFX_DLL}: {e}"))?;
    let key = (dll, meta.len(), meta.modified().ok());
    if let Some((k, v)) = &*VERSION_CACHE.lock().unwrap() {
        if *k == key {
            return Ok(*v);
        }
    }
    // The runtime unloads again when this copy is dropped, unless a filter holds it.
    let version = Runtime::shared(dir)?.version();
    *VERSION_CACHE.lock().unwrap() = Some((key, version));
    Ok(version)
}

// ---- the loaded runtime ------------------------------------------------------------------------------------------

type GetVersionFn = unsafe extern "C" fn(*mut u32) -> i32;

/// `NVCVImage.dll` and `NVVideoEffects.dll`, loaded from one runtime folder.
pub struct Runtime {
    dir: PathBuf,
    version: u32,
    // `videofx` depends on `cvimage`: fields drop in this order.
    videofx: Library,
    cvimage: Library,
}

fn describe(e: &libloading::Error) -> String {
    match std::error::Error::source(e) {
        Some(src) => format!("{e}: {src}"),
        None => e.to_string(),
    }
}

impl Runtime {
    /// Loads the DLLs of `dir` with OBS's search flags (the folder of the DLL and the default directories, so the
    /// runtime's own dependencies next to it are found and nothing from the current directory is) and reads the
    /// version. A runtime that is too old still loads: [`Runtime::version`] says which it is.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let flags = LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS;
        let open = |name: &str| unsafe { Library::load_with_flags(dir.join(name), flags) }.map_err(|e| format!("{name}: {}", describe(&e)));
        let cvimage = open(CVIMAGE_DLL)?;
        let videofx = open(VIDEOFX_DLL)?;
        let get_version = unsafe { videofx.get::<GetVersionFn>(b"NvVFX_GetVersion\0") }
            .map_err(|e| format!("{VIDEOFX_DLL}: NvVFX_GetVersion: {}", describe(&e)))?;
        let mut version = 0u32;
        let rc = unsafe { get_version(&mut version) };
        if rc != NVCV_SUCCESS {
            return Err(format!("NvVFX_GetVersion failed (status {rc})"));
        }
        Ok(Self { dir: dir.to_path_buf(), version, videofx, cvimage })
    }

    /// The runtime of `dir`, shared: the copy that is loaded now when somebody holds one, else a new load.
    pub fn shared(dir: &Path) -> Result<Arc<Self>, String> {
        static LOADED: Mutex<Weak<Runtime>> = Mutex::new(Weak::new());
        let mut slot = LOADED.lock().unwrap();
        if let Some(rt) = slot.upgrade().filter(|rt| rt.dir == dir) {
            return Ok(rt);
        }
        let rt = Arc::new(Self::load(dir)?);
        *slot = Arc::downgrade(&rt);
        Ok(rt)
    }

    /// The shared runtime of the folder [`runtime_dir`] names, if it is complete and new enough. The error is a
    /// sentence for the card.
    pub fn open() -> Result<Arc<Self>, String> {
        let dir = runtime_dir();
        let gpu = detect_gpu();
        let layout = inspect(&dir, gpu.as_ref().map(|g| g.arch));
        if !layout.complete() || gpu.is_none() {
            return Err(status_from(gpu.as_ref(), &layout, None).detail);
        }
        let rt = Self::shared(&dir)?;
        if !version_ok(rt.version) {
            return Err(status_from(gpu.as_ref(), &layout, Some(&Ok(rt.version))).detail);
        }
        Ok(rt)
    }

    /// Packed like [`MIN_VERSION`]; [`format_version`] makes the text.
    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The folder to give `NvVFX_SetString(NVVFX_MODEL_DIRECTORY)`.
    pub fn models_dir(&self) -> PathBuf {
        self.dir.join("models")
    }

    /// An export of `NVVideoEffects.dll` (`NvVFX_CreateEffect`, ...), as the function pointer type `T`.
    ///
    /// # Safety
    /// `T` must be the exported function's real signature, and the pointer must not be called after the last `Arc` of
    /// this runtime is dropped.
    pub unsafe fn videofx_fn<T: Copy>(&self, name: &str) -> Result<T, String> {
        Self::symbol(&self.videofx, VIDEOFX_DLL, name)
    }

    /// An export of `NVCVImage.dll` (`NvCVImage_Init`, `NvCVImage_Transfer`, ...). Same rules as [`Self::videofx_fn`].
    ///
    /// # Safety
    /// See [`Self::videofx_fn`].
    pub unsafe fn cvimage_fn<T: Copy>(&self, name: &str) -> Result<T, String> {
        Self::symbol(&self.cvimage, CVIMAGE_DLL, name)
    }

    unsafe fn symbol<T: Copy>(lib: &Library, file: &str, name: &str) -> Result<T, String> {
        let mut z = name.as_bytes().to_vec();
        z.push(0);
        lib.get::<T>(&z[..]).map(|s| *s).map_err(|e| format!("{file}: {name}: {}", describe(&e)))
    }
}

// ---- installer ---------------------------------------------------------------------------------------------------

/// NVIDIA's own download of the Video Effects redistributable for a GPU generation (the host and the file names are
/// the ones on NVIDIA's Broadcast SDK resources page).
pub fn installer_url(arch: &str) -> Option<String> {
    matches!(arch, "turing" | "ampere" | "ada" | "blackwell").then(|| {
        format!("https://international.download.nvidia.com/Windows/broadcast/sdk/VFX/nvidia_video_effects_sdk_installer_v{INSTALLER_VERSION}_{arch}.exe")
    })
}

/// A file Better Cam accepts at one address by its fingerprint: the size and the SHA-256 (upper case hex).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

/// Why pins exist: NVIDIA's own signature on these four installers does not chain to a root Windows trusts (the
/// signer is the internal `CN=NVIDIA DVS Dev Signing`, issued by `NVIDIA DVS Dev Intermediate`, expired 2026-03-27,
/// thumbprint F3F62BE053B6812E2E1AEC4EA8EB7FB4B955B5AB; `Get-AuthenticodeSignature` says `UnknownError`), so the
/// signature check alone would refuse the genuine files. The script therefore also accepts exactly these files,
/// whose size and SHA-256 were taken from NVIDIA's download host over HTTPS on 2026-11-17 (and checked a second time
/// the same day). A properly signed future file is accepted without a pin. Per generation: (arch, size, SHA-256).
const PINNED: [(&str, u64, &str); 4] = [
    ("turing", 759_745_272, "D51D8789F96A82375B04BCCA6914EEE301AD5B6CC45137D7B86FC45DDB205F2D"),
    ("ampere", 775_486_112, "93BDF4FFEC7C393037936349678D1864897FEF45D7BA2AB81D4B1563E1328512"),
    ("ada", 767_555_072, "4761FE1579E82DE6D45EFEC8DC09492CF2CC601867269955E129707AC8EA8546"),
    ("blackwell", 783_058_432, "CEF45592E16D0EA91DBFF12A828B31874AB83829ADF9A431466DFB88B714618D"),
];

/// The built-in pins, each tied to the address of its own generation: a hash never validates another address.
pub fn pins() -> Vec<Pin> {
    PINNED
        .iter()
        .filter_map(|(arch, size, sha)| Some(Pin { url: installer_url(arch)?, size: *size, sha256: (*sha).into() }))
        .collect()
}

/// A test hook of debug builds: `BETTER_CAM_TEST_NVIDIA_URL` replaces the download address, but only with a loopback
/// address (never a real host). Release builds have no such thing.
#[cfg(debug_assertions)]
pub fn test_url_override() -> Option<String> {
    std::env::var("BETTER_CAM_TEST_NVIDIA_URL").ok().filter(|u| u.starts_with("http://127.0.0.1:"))
}

/// Debug builds only: `BETTER_CAM_TEST_NVIDIA_PIN=<size>:<sha256>` is the pin of the loopback test address. It
/// replaces the built-in table, so a test file can be accepted without touching the real pins.
#[cfg(debug_assertions)]
pub fn test_pins(url: &str) -> Option<Vec<Pin>> {
    if !url.starts_with("http://127.0.0.1:") {
        return None;
    }
    let v = std::env::var("BETTER_CAM_TEST_NVIDIA_PIN").ok()?;
    let (size, sha) = v.split_once(':')?;
    Some(vec![Pin { url: url.into(), size: size.trim().parse().ok()?, sha256: sha.trim().to_ascii_uppercase() }])
}

/// What the `nvidia-install` event carries: `download` with bytes done and total, or a stage (`verify`, `install`)
/// with zeros.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    pub stage: String,
    pub done: u64,
    pub total: u64,
}

/// One line of what `install_nvidia.ps1` prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptLine {
    Progress { done: u64, total: u64 },
    Stage(String),
    /// What the signature check found: PowerShell's status (`Valid`, `NotSigned`, ...) and the signer's name.
    Signature { status: String, signer: String },
    /// The file passed the check, by `signature` (valid, NVIDIA Corporation) or by `pin` (the recorded fingerprint);
    /// `detail` is the signer or the computed SHA-256.
    Accepted { rule: String, detail: String },
    Error(String),
}

pub fn parse_line(line: &str) -> Option<ScriptLine> {
    let (word, rest) = line.trim().split_once(' ')?;
    match word {
        "PROGRESS" => {
            let mut n = rest.split(' ').filter_map(|x| x.parse::<u64>().ok());
            Some(ScriptLine::Progress { done: n.next().unwrap_or(0), total: n.next().unwrap_or(0) })
        }
        "STAGE" => Some(ScriptLine::Stage(rest.trim().into())),
        "SIGNATURE" => {
            let (status, signer) = rest.split_once(' ').unwrap_or((rest, "-"));
            Some(ScriptLine::Signature { status: status.into(), signer: signer.trim().into() })
        }
        "ACCEPTED" => {
            let (rule, detail) = rest.split_once(' ').unwrap_or((rest, ""));
            Some(ScriptLine::Accepted { rule: rule.into(), detail: detail.trim().into() })
        }
        "ERROR" => Some(ScriptLine::Error(rest.trim().into())),
        _ => None,
    }
}

impl ScriptLine {
    /// The event for the window, for the lines that are progress.
    pub fn progress(&self) -> Option<Progress> {
        match self {
            Self::Progress { done, total } => Some(Progress { stage: "download".into(), done: *done, total: *total }),
            Self::Stage(s) => Some(Progress { stage: s.clone(), done: 0, total: 0 }),
            _ => None,
        }
    }
}

const INSTALL_SCRIPT: &str = include_str!("../install_nvidia.ps1");

/// Downloads `url`, accepts the file only when it is validly signed by NVIDIA Corporation or when it is the file the
/// entry of `pins` for this very `url` describes (size and SHA-256), and runs it elevated; waits until the installer
/// closes. Every line the script prints goes to `on_line`. `temp` replaces `%TEMP%` for the script (tests). The file is
/// deleted at the end, whatever happened, and a file that passes neither check is never run.
pub fn run_install(url: &str, pins: &[Pin], temp: Option<&Path>, mut on_line: impl FnMut(&ScriptLine)) -> Result<(), String> {
    use std::io::{BufRead, BufReader};
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    if !url.is_ascii() || url.contains(|c: char| c == '\'' || c.is_whitespace() || c.is_control()) {
        return Err("The download address is not valid.".into());
    }
    let (size, sha) = match pins.iter().find(|p| p.url == url) {
        Some(p) if p.size > 0 && p.sha256.len() == 64 && p.sha256.chars().all(|c| c.is_ascii_hexdigit()) => (p.size, p.sha256.to_ascii_uppercase()),
        Some(_) => return Err("The recorded fingerprint for this download is not valid.".into()),
        None => (0, String::new()),
    };
    let script = INSTALL_SCRIPT.replace("{SIZE}", &format!("{size}L")).replace("{SHA256}", &sha).replace("{URL}", url);
    let mut cmd = Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(t) = temp {
        cmd.env("TEMP", t).env("TMP", t);
    }
    let mut child = cmd.spawn().map_err(|e| format!("Could not start PowerShell: {e}"))?;
    let mut error = None;
    for line in BufReader::new(child.stdout.take().ok_or("no installer output")?).lines().map_while(Result::ok) {
        if let Some(l) = parse_line(&line) {
            if let ScriptLine::Error(m) = &l {
                error = Some(m.clone());
            }
            on_line(&l);
        }
    }
    if child.wait().map_err(|e| e.to_string())?.success() {
        Ok(())
    } else {
        Err(error.unwrap_or_else(|| "The NVIDIA installer failed.".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn gpu(arch: &'static str) -> Gpu {
        Gpu { name: "NVIDIA GeForce RTX 5080".into(), arch }
    }

    fn complete(dir: &str) -> Layout {
        Layout { dir: dir.into(), dir_exists: true, videofx: true, cvimage: true, has_models: true, missing_models: vec![] }
    }

    #[test]
    fn nvidia_arch_follows_the_gpu_series() {
        assert_eq!(arch_for("NVIDIA GeForce RTX 2070", "7.5"), Some("turing"));
        assert_eq!(arch_for("NVIDIA GeForce RTX 3060", "8.6"), Some("ampere"));
        assert_eq!(arch_for("NVIDIA RTX A4000", "8.6"), Some("ampere"));
        assert_eq!(arch_for("NVIDIA GeForce RTX 4070", "8.9"), Some("ada"));
        assert_eq!(arch_for("NVIDIA GeForce RTX 5080", "12.0"), Some("blackwell"));
        assert_eq!(arch_for("NVIDIA RTX PRO 6000 Blackwell", "12.0"), Some("blackwell"));
        assert_eq!(arch_for("NVIDIA TITAN RTX", " 7.5 "), Some("turing"));
        // Turing without tensor cores, older cards, other makers, data centre cards without RTX in the name.
        assert_eq!(arch_for("NVIDIA GeForce GTX 1660 SUPER", "7.5"), None);
        assert_eq!(arch_for("NVIDIA GeForce GTX 1080", "6.1"), None);
        assert_eq!(arch_for("NVIDIA GeForce RTX 9999", "[N/A]"), None);
        assert_eq!(arch_for("NVIDIA GeForce RTX 9999", ""), None);
        assert_eq!(arch_for("NVIDIA H100", "9.0"), None);
        assert_eq!(arch_for("AMD Radeon RX 7900 XTX", "12.0"), None);
        assert_eq!(arch_for("Intel(R) Arc(TM) A770", "8.9"), None);
    }

    #[test]
    fn nvidia_smi_output_gives_the_first_usable_gpu() {
        assert_eq!(parse_smi("NVIDIA GeForce RTX 5080, 12.0\r\n"), Some(gpu("blackwell")));
        let two = "NVIDIA GeForce GTX 1060 6GB, 6.1\nNVIDIA GeForce RTX 3060, 8.6\n";
        assert_eq!(parse_smi(two), Some(Gpu { name: "NVIDIA GeForce RTX 3060".into(), arch: "ampere" }));
        assert_eq!(parse_smi("NVIDIA GeForce GTX 1660 Ti, 7.5\n"), None);
        assert_eq!(parse_smi("NVIDIA-SMI has failed because it couldn't communicate with the NVIDIA driver."), None);
        assert_eq!(parse_smi(""), None);
    }

    #[test]
    fn nvidia_runtime_folder_follows_the_environment() {
        let pf = Some(OsString::from(r"D:\Programs"));
        assert_eq!(runtime_dir_from(Some(r"E:\vfx".into()), pf.clone()), PathBuf::from(r"E:\vfx"));
        assert_eq!(runtime_dir_from(None, pf.clone()), PathBuf::from(r"D:\Programs\NVIDIA Corporation\NVIDIA Video Effects"));
        assert_eq!(runtime_dir_from(Some("".into()), pf), PathBuf::from(r"D:\Programs\NVIDIA Corporation\NVIDIA Video Effects"));
        assert_eq!(runtime_dir_from(None, None), PathBuf::from(r"C:\Program Files\NVIDIA Corporation\NVIDIA Video Effects"));
    }

    #[test]
    fn nvidia_versions_compare_and_print() {
        assert_eq!(MIN_VERSION, 0x0007_0600);
        assert_eq!(format_version(MIN_VERSION), "0.7.6");
        assert_eq!(format_version((1 << 24) | (3 << 16)), "1.3.0");
        assert_eq!(format_version((7 << 16) | (6 << 8) | 2), "0.7.6.2");
        assert!(version_ok(MIN_VERSION));
        assert!(version_ok((7 << 16) | (6 << 8) | 1));
        assert!(version_ok(1 << 24));
        assert!(!version_ok((7 << 16) | (5 << 8) | 255));
        assert!(!version_ok((6 << 16) | (5 << 8)));
        assert!(!version_ok(0));
    }

    #[test]
    fn nvidia_status_names_what_is_missing() {
        let g = gpu("blackwell");
        let ok = Ok(MIN_VERSION);

        let s = status_from(None, &complete("X"), Some(&ok));
        assert!(!s.available && s.detail.contains("RTX GPU") && s.version.is_none());

        let none = Layout { dir: "X".into(), ..Layout::default() };
        let s = status_from(Some(&g), &none, None);
        assert!(!s.available && s.detail.contains("not installed") && s.detail.contains("X"), "{}", s.detail);

        let no_dll = Layout { videofx: false, ..complete("X") };
        let s = status_from(Some(&g), &no_dll, None);
        assert!(!s.available && s.detail.contains("NVVideoEffects.dll is missing"), "{}", s.detail);

        let no_cv = Layout { cvimage: false, ..complete("X") };
        assert!(status_from(Some(&g), &no_cv, None).detail.contains("NVCVImage.dll is missing"));

        let no_models = Layout { has_models: false, ..complete("X") };
        let s = status_from(Some(&g), &no_models, None);
        assert!(!s.available && s.detail.contains("models folder"), "{}", s.detail);

        let some_models = Layout { missing_models: vec!["a.trtpkg".into(), "b.trtpkg".into()], ..complete("X") };
        let s = status_from(Some(&g), &some_models, None);
        assert!(!s.available && s.detail.contains("a.trtpkg, b.trtpkg"), "{}", s.detail);

        let old = Ok((7 << 16) | (1 << 8));
        let s = status_from(Some(&g), &complete("X"), Some(&old));
        assert!(!s.available && s.detail.contains("0.7.1") && s.detail.contains("0.7.6"), "{}", s.detail);
        assert_eq!(s.version.as_deref(), Some("0.7.1"));

        let broken: Result<u32, String> = Err("NVVideoEffects.dll: The specified module could not be found".into());
        let s = status_from(Some(&g), &complete("X"), Some(&broken));
        assert!(!s.available && s.detail.contains("could not load") && s.detail.contains("module could not be found"), "{}", s.detail);

        let s = status_from(Some(&g), &complete("X"), Some(&ok));
        assert!(s.available && s.version.as_deref() == Some("0.7.6") && s.detail.contains("0.7.6"), "{}", s.detail);
        let newer = Ok((1 << 24) | (3 << 16));
        assert!(status_from(Some(&g), &complete("X"), Some(&newer)).available);
    }

    #[test]
    fn nvidia_inspect_reads_the_folder() {
        let dir = std::env::temp_dir().join(format!("bc-nv-inspect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!inspect(&dir, None).dir_exists);
        std::fs::create_dir_all(dir.join("models")).unwrap();
        let l = inspect(&dir, None);
        assert!(l.dir_exists && !l.videofx && !l.cvimage && !l.has_models && !l.complete());
        std::fs::write(dir.join(VIDEOFX_DLL), b"x").unwrap();
        std::fs::write(dir.join(CVIMAGE_DLL), b"x").unwrap();
        assert!(!inspect(&dir, None).complete(), "an empty models folder is not enough");

        // Any model file does for a PC without a GPU; a GPU asks for the files of its generation.
        std::fs::write(dir.join("models").join("AIGS_288x512_75_m0.engine.trtpkg"), b"x").unwrap();
        let l = inspect(&dir, None);
        assert!(l.videofx && l.cvimage && l.has_models && l.complete());
        let l = inspect(&dir, Some("turing"));
        assert!(!l.complete());
        assert_eq!(l.missing_models, ["AIGS_288x512_75_m1.engine.trtpkg", "AIGS_288x512_75_m2.engine.trtpkg", "AIGS_288x512_75_m3.engine.trtpkg"]);
        for m in 1..4 {
            std::fs::write(dir.join("models").join(format!("AIGS_288x512_75_m{m}.engine.trtpkg")), b"x").unwrap();
        }
        assert!(inspect(&dir, Some("turing")).complete());
        // The files of another generation do not help.
        assert_eq!(inspect(&dir, Some("blackwell")).missing_models.len(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nvidia_a_runtime_that_cannot_load_gives_a_readable_reason() {
        let dir = std::env::temp_dir().join(format!("bc-nv-junk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Not a DLL at all, and a folder with no DLLs.
        std::fs::write(dir.join(CVIMAGE_DLL), b"this is not a dll").unwrap();
        std::fs::write(dir.join(VIDEOFX_DLL), b"this is not a dll").unwrap();
        let e = Runtime::load(&dir).err().expect("junk must not load");
        assert!(e.starts_with("NVCVImage.dll: "), "{e}");
        let e = read_version(&dir).unwrap_err();
        assert!(e.contains("NVCVImage.dll"), "{e}");
        let status = status_from(Some(&gpu("ada")), &complete(&dir.to_string_lossy()), Some(&Err(e)));
        assert!(!status.available && status.detail.contains("could not load") && status.detail.contains("Reinstall"), "{}", status.detail);

        let empty = dir.join("nothing-here");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(Runtime::load(&empty).err().is_some_and(|e| e.contains("NVCVImage.dll")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nvidia_models_follow_the_gpu_generation() {
        assert_eq!(
            required_models("blackwell"),
            ["AIGS_288x512_100_m0.engine.trtpkg", "AIGS_288x512_100_m1.engine.trtpkg", "AIGS_288x512_100_m2.engine.trtpkg", "AIGS_288x512_100_m3.engine.trtpkg"]
        );
        assert_eq!(required_models("turing")[0], "AIGS_288x512_75_m0.engine.trtpkg");
        assert_eq!(required_models("ampere")[3], "AIGS_288x512_86_m3.engine.trtpkg");
        assert_eq!(required_models("ada")[1], "AIGS_288x512_89_m1.engine.trtpkg");
        assert!(required_models("hopper").is_empty());
    }

    #[test]
    fn nvidia_installer_url_per_generation() {
        let base = "https://international.download.nvidia.com/Windows/broadcast/sdk/VFX/nvidia_video_effects_sdk_installer_v0.7.6_";
        for arch in ["turing", "ampere", "ada", "blackwell"] {
            assert_eq!(installer_url(arch), Some(format!("{base}{arch}.exe")));
        }
        assert_eq!(installer_url("hopper"), None);
        assert_eq!(installer_url(""), None);
        assert_eq!(installer_url("ada.exe?x=1"), None);
    }

    #[test]
    fn nvidia_script_lines_parse() {
        assert_eq!(parse_line("PROGRESS 4194304 783058432"), Some(ScriptLine::Progress { done: 4194304, total: 783058432 }));
        assert_eq!(parse_line("PROGRESS 10 0\r"), Some(ScriptLine::Progress { done: 10, total: 0 }));
        assert_eq!(parse_line("STAGE verify"), Some(ScriptLine::Stage("verify".into())));
        assert_eq!(
            parse_line("SIGNATURE Valid NVIDIA Corporation"),
            Some(ScriptLine::Signature { status: "Valid".into(), signer: "NVIDIA Corporation".into() })
        );
        assert_eq!(parse_line("SIGNATURE NotSigned -"), Some(ScriptLine::Signature { status: "NotSigned".into(), signer: "-".into() }));
        assert_eq!(parse_line("ERROR The download server answered 404 Not Found."), Some(ScriptLine::Error("The download server answered 404 Not Found.".into())));
        assert_eq!(parse_line("something else"), None);
        assert_eq!(parse_line(""), None);

        let p = parse_line("PROGRESS 5 10").unwrap().progress().unwrap();
        assert_eq!(p, Progress { stage: "download".into(), done: 5, total: 10 });
        let p = parse_line("STAGE install").unwrap().progress().unwrap();
        assert_eq!(p, Progress { stage: "install".into(), done: 0, total: 0 });
        assert!(parse_line("ERROR x").unwrap().progress().is_none());
        assert_eq!(parse_line("ACCEPTED pin D51D"), Some(ScriptLine::Accepted { rule: "pin".into(), detail: "D51D".into() }));
        assert_eq!(
            parse_line("ACCEPTED signature NVIDIA Corporation"),
            Some(ScriptLine::Accepted { rule: "signature".into(), detail: "NVIDIA Corporation".into() })
        );
        assert!(parse_line("ACCEPTED pin D51D").unwrap().progress().is_none());
    }

    // ---- the script, against a server on this PC ---------------------------------------------------------------

    /// Serves `/file` with `body` and anything else with 404, for as many requests as come until the test ends.
    fn serve(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let body = body.clone();
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let mut head = Vec::new();
                    let mut b = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") && stream.read(&mut b).unwrap_or(0) == 1 {
                        head.push(b[0]);
                    }
                    let ok = String::from_utf8_lossy(&head).starts_with("GET /file ");
                    let (status, payload) = if ok { ("200 OK", &body[..]) } else { ("404 Not Found", &b""[..]) };
                    let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", payload.len());
                    let _ = stream.write_all(payload);
                });
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    /// Runs the script with its own `%TEMP%`; returns the lines it printed, its result, and whether the temp folder
    /// still holds the download folder afterwards.
    fn run_script(url: &str, name: &str) -> (Vec<ScriptLine>, Result<(), String>, bool) {
        run_script_pinned(url, &[], name)
    }

    fn run_script_pinned(url: &str, pins: &[Pin], name: &str) -> (Vec<ScriptLine>, Result<(), String>, bool) {
        let temp = std::env::temp_dir().join(format!("bc-nv-script-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let mut lines = Vec::new();
        let result = run_install(url, pins, Some(&temp), |l| lines.push(l.clone()));
        let leftover = temp.join("better-cam-nvidia").exists();
        let _ = std::fs::remove_dir_all(&temp);
        (lines, result, leftover)
    }

    #[test]
    fn nvidia_a_download_that_answers_404_ends_with_a_sentence_and_no_file() {
        let base = serve(Vec::new());
        let (lines, result, leftover) = run_script(&format!("{base}/missing"), "404");
        assert_eq!(result, Err("The download server answered 404 Not Found.".into()));
        assert!(!lines.contains(&ScriptLine::Stage("verify".into())) && !lines.contains(&ScriptLine::Stage("install".into())));
        assert!(!leftover, "the download folder must be gone");
    }

    #[test]
    fn nvidia_an_unsigned_installer_is_refused_deleted_and_never_run() {
        // The test program itself: a real executable that has no signature.
        let base = serve(std::fs::read(std::env::current_exe().unwrap()).unwrap());
        let (lines, result, leftover) = run_script(&format!("{base}/file"), "unsigned");
        assert_eq!(result, Err("The downloaded installer failed the signature check (NotSigned, signer -), so it was not run.".into()), "{lines:?}");
        assert!(lines.contains(&ScriptLine::Stage("verify".into())));
        assert!(lines.contains(&ScriptLine::Signature { status: "NotSigned".into(), signer: "-".into() }), "{lines:?}");
        assert!(!lines.contains(&ScriptLine::Stage("install".into())), "the installer must not start: {lines:?}");
        assert!(!leftover, "the download folder must be gone");
    }

    #[test]
    fn nvidia_a_valid_signature_of_another_company_is_refused_too() {
        let whoami = Path::new(&std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())).join(r"System32\whoami.exe");
        let base = serve(std::fs::read(whoami).unwrap());
        let (lines, result, leftover) = run_script(&format!("{base}/file"), "other-signer");
        let err = result.clone().unwrap_err();
        assert!(err.starts_with("The downloaded installer is signed by ") && err.ends_with(", not by NVIDIA Corporation, so it was not run."), "{err}");
        assert!(lines.iter().any(|l| matches!(l, ScriptLine::Signature { status, signer } if status == "Valid" && signer != "NVIDIA Corporation")), "{lines:?}");
        assert!(!lines.contains(&ScriptLine::Stage("install".into())), "the installer must not start: {lines:?}");
        assert!(!leftover);
    }

    #[test]
    fn nvidia_a_bad_download_address_is_not_passed_to_the_script() {
        assert!(run_install("http://127.0.0.1:1/x' ; calc ; '", &[], None, |_| {}).is_err());
        assert!(run_install("http://127.0.0.1:1/a b", &[], None, |_| {}).is_err());
    }

    // ---- pins ----------------------------------------------------------------------------------------------------

    /// SHA-256 of `bytes` by Windows' own tool, not by the script under test.
    fn sha256_of(bytes: &[u8]) -> String {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let f = std::env::temp_dir().join(format!("bc-nv-hash-{}-{}.bin", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
        std::fs::write(&f, bytes).unwrap();
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &format!("(Get-FileHash -Algorithm SHA256 -LiteralPath '{}').Hash", f.display())])
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&f);
        String::from_utf8_lossy(&out.stdout).trim().to_ascii_uppercase()
    }

    /// A harmless program that Windows signs (so the signature rule cannot be what accepts it: its signer is not
    /// NVIDIA). The "installer" of the pin tests: it prints the user name and exits.
    fn harmless() -> Vec<u8> {
        std::fs::read(Path::new(&std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())).join(r"System32\whoami.exe")).unwrap()
    }

    fn pin(url: &str, bytes: &[u8]) -> Pin {
        Pin { url: url.into(), size: bytes.len() as u64, sha256: sha256_of(bytes) }
    }

    #[test]
    fn nvidia_the_built_in_pins_match_the_url_table() {
        let all = pins();
        assert_eq!(all.len(), 4);
        for (arch, p) in ["turing", "ampere", "ada", "blackwell"].iter().zip(&all) {
            assert_eq!(Some(&p.url), installer_url(arch).as_ref());
            assert!(p.sha256.len() == 64 && p.sha256.chars().all(|c| matches!(c, '0'..='9' | 'A'..='F')), "{}", p.sha256);
            assert!(p.size > 700_000_000 && p.size < 800_000_000);
        }
        let mut hashes: Vec<_> = all.iter().map(|p| &p.sha256).collect();
        hashes.sort();
        hashes.dedup();
        assert_eq!(hashes.len(), 4, "every generation has its own file");
    }

    #[test]
    fn nvidia_a_file_that_matches_its_pin_is_accepted_and_run() {
        let bytes = harmless();
        let base = serve(bytes.clone());
        let url = format!("{base}/file");
        let p = pin(&url, &bytes);
        let (lines, result, leftover) = run_script_pinned(&url, std::slice::from_ref(&p), "pin-ok");
        assert_eq!(result, Ok(()), "{lines:?}");
        assert!(lines.contains(&ScriptLine::Accepted { rule: "pin".into(), detail: p.sha256.clone() }), "{lines:?}");
        assert!(lines.contains(&ScriptLine::Stage("install".into())), "{lines:?}");
        assert!(!leftover, "the download folder must be gone");
    }

    #[test]
    fn nvidia_a_file_with_one_byte_changed_is_refused_and_deleted() {
        let good = harmless();
        let mut bad = good.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        let base = serve(bad);
        let url = format!("{base}/file");
        let (lines, result, leftover) = run_script_pinned(&url, &[pin(&url, &good)], "pin-byte");
        let err = result.unwrap_err();
        assert!(err.contains("not the file Better Cam has on record") && err.contains("SHA-256") && err.ends_with("so it was not run."), "{err}");
        assert!(!lines.iter().any(|l| matches!(l, ScriptLine::Accepted { .. })) && !lines.contains(&ScriptLine::Stage("install".into())), "{lines:?}");
        assert!(!leftover);
    }

    #[test]
    fn nvidia_a_right_hash_with_a_wrong_size_is_refused() {
        let bytes = harmless();
        let base = serve(bytes.clone());
        let url = format!("{base}/file");
        let mut p = pin(&url, &bytes);
        p.size += 1;
        let (lines, result, leftover) = run_script_pinned(&url, &[p], "pin-size");
        let err = result.unwrap_err();
        assert!(err.contains("its size is") && err.contains("expected"), "{err}");
        assert!(!lines.contains(&ScriptLine::Stage("install".into())), "{lines:?}");
        assert!(!leftover);
    }

    #[test]
    fn nvidia_a_pin_for_another_address_does_not_validate_the_file() {
        let bytes = harmless();
        let base = serve(bytes.clone());
        let url = format!("{base}/file");
        // The pin is exactly right for these bytes, but belongs to another generation's address.
        let other = pin(&format!("{base}/other"), &bytes);
        let (lines, result, leftover) = run_script_pinned(&url, &[other], "pin-other-url");
        let err = result.unwrap_err();
        assert!(err.starts_with("The downloaded installer is signed by ") && err.ends_with("so it was not run."), "{err}");
        assert!(!lines.iter().any(|l| matches!(l, ScriptLine::Accepted { .. })) && !lines.contains(&ScriptLine::Stage("install".into())), "{lines:?}");
        assert!(!leftover);
        // And the built-in table knows nothing about a loopback address.
        let (lines, result, _) = run_script_pinned(&url, &pins(), "pin-builtin");
        assert!(result.is_err() && !lines.contains(&ScriptLine::Stage("install".into())), "{lines:?}");
    }

    #[test]
    fn nvidia_a_file_without_pin_or_nvidia_signature_is_refused() {
        // Unsigned and no pin entry at all, with the pin table of other files around.
        let base = serve(std::fs::read(std::env::current_exe().unwrap()).unwrap());
        let url = format!("{base}/file");
        let (lines, result, leftover) = run_script_pinned(&url, &[pin(&format!("{base}/x"), b"x")], "pin-none");
        assert_eq!(result, Err("The downloaded installer failed the signature check (NotSigned, signer -), so it was not run.".into()), "{lines:?}");
        assert!(!lines.contains(&ScriptLine::Stage("install".into())));
        assert!(!leftover);
    }

    #[test]
    fn nvidia_a_malformed_pin_is_refused_before_the_script_runs() {
        let url = "http://127.0.0.1:1/file";
        for sha in ["", "ABC", &"G".repeat(64), &format!("{}'; calc; '", "A".repeat(40))] {
            let p = Pin { url: url.into(), size: 10, sha256: sha.into() };
            assert_eq!(run_install(url, &[p], None, |_| {}), Err("The recorded fingerprint for this download is not valid.".into()));
        }
        let p = Pin { url: url.into(), size: 0, sha256: "A".repeat(64) };
        assert!(run_install(url, &[p], None, |_| {}).is_err());
    }

    /// Runs against the real runtime; skipped on a PC where it is not installed.
    #[test]
    fn nvidia_real_runtime_loads_and_reports_its_version() {
        let layout = inspect(&runtime_dir(), detect_gpu().as_ref().map(|g| g.arch));
        if !layout.complete() {
            eprintln!("skipped: NVIDIA Video Effects runtime not installed at {}", layout.dir.display());
            return;
        }
        let t = std::time::Instant::now();
        let rt = Runtime::shared(&layout.dir).expect("load the runtime");
        eprintln!("runtime {} (raw {:#010x}) loaded in {:?} from {}", format_version(rt.version()), rt.version(), t.elapsed(), rt.dir().display());
        assert!(version_ok(rt.version()));
        // `open` is what the filters call: same folder, same shared copy.
        let opened = Runtime::open().expect("Runtime::open");
        assert!(Arc::ptr_eq(&rt, &opened));
        // The same copy is handed out while somebody holds it.
        assert!(Arc::ptr_eq(&rt, &Runtime::shared(&layout.dir).unwrap()));
        let create: Result<unsafe extern "C" fn(*const std::ffi::c_char, *mut *mut std::ffi::c_void) -> i32, String> =
            unsafe { rt.videofx_fn("NvVFX_CreateEffect") };
        assert!(create.is_ok());
        assert!(unsafe { rt.videofx_fn::<unsafe extern "C" fn()>("NoSuchExport") }.is_err());
        assert!(probe().detail.contains(&format_version(rt.version())) || detect_gpu().is_none());
    }
}
