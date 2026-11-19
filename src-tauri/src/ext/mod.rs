//! Extensions (`docs/EXTENSIONS.md`): custom filters written in WGSL (or HLSL).
//!
//! * Spike E1: [`wgsl::translate`] turns WGSL into HLSL and the reflected uniform layouts, [`compile::compile_ps`]
//!   compiles the HLSL with FXC (`ps_5_0`), and [`layout`] packs parameter values into the layout `naga` reflected
//!   (`docs/SPIKE-EXT.md`).
//! * E3: [`manifest`] and the registry in this file (`ExtRegistry`), which finds the extension folders.
//! * E4: [`loader`] builds shaders off the render thread (cache by source hash, bounded workers), [`hlsl`] is the
//!   `.hlsl` path, [`params`] turns a card's settings into the `Params` bytes, and [`filter::ExtShaderFilter`] is the
//!   chain entry `ext:<id>` that `fx::make` hands out through [`make_filter`].

pub mod compile;
pub mod filter;
pub mod hlsl;
pub mod layout;
pub mod loader;
pub mod manifest;
pub mod params;
pub mod wgsl;

#[cfg(test)]
mod tests;

use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// Where a problem comes from, so the UI can word it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The author's shader source: syntax, types, a feature the convention does not allow.
    Source,
    /// The manifest (parameter list) does not fit the shader or WGSL.
    Manifest,
    /// FXC rejected (or could not finish) the generated HLSL.
    Fxc,
    /// A bug in the translation layer, a panic or a timeout. Not the author's fault.
    Internal,
}

/// A precise, user-readable error. `line` and `column` (1-based) count in the author's file, the generated prelude
/// excluded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtError {
    pub kind: ErrorKind,
    pub message: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

impl ExtError {
    pub fn source(message: impl Into<String>) -> ExtError {
        ExtError { kind: ErrorKind::Source, message: message.into(), line: None, column: None }
    }

    pub fn source_at(line: u32, column: u32, message: impl Into<String>) -> ExtError {
        ExtError { kind: ErrorKind::Source, message: message.into(), line: Some(line), column: Some(column) }
    }

    pub fn manifest(message: impl Into<String>) -> ExtError {
        ExtError { kind: ErrorKind::Manifest, message: message.into(), line: None, column: None }
    }

    pub fn fxc(message: impl Into<String>) -> ExtError {
        ExtError { kind: ErrorKind::Fxc, message: message.into(), line: None, column: None }
    }

    pub fn internal(message: impl Into<String>) -> ExtError {
        ExtError { kind: ErrorKind::Internal, message: message.into(), line: None, column: None }
    }

    /// `file:line:column: message`, the form `docs/EXTENSIONS.md` asks for.
    pub fn render(&self, file: &str) -> String {
        match (self.line, self.column) {
            (Some(l), Some(c)) => format!("{file}:{l}:{c}: {}", self.message),
            (Some(l), None) => format!("{file}:{l}: {}", self.message),
            _ => format!("{file}: {}", self.message),
        }
    }
}

impl fmt::Display for ExtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.line, self.column) {
            (Some(l), Some(c)) => write!(f, "{l}:{c}: {}", self.message),
            _ => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for ExtError {}

/// Address space reserved for the stack of a worker thread (Windows commits only the pages that are touched).
///
/// A stack overflow cannot be caught and ends the process, and `naga`'s WGSL parser recurses once per term of a chain
/// like `a+a+a+...`: measured (release build) it overflows 16 MiB between 20 000 and 40 000 terms and needs between
/// 64 and 128 MiB for the 128 000 terms a 256 KB source can hold. 512 MiB leaves 4x margin on that worst case.
const WORKER_STACK: usize = 512 * 1024 * 1024;

/// Workers that timed out and still run (FXC and `naga` cannot be cancelled). Past [`MAX_ABANDONED`] new work is
/// refused, so a hostile shader cannot pile up busy threads.
static ABANDONED: AtomicUsize = AtomicUsize::new(0);
const MAX_ABANDONED: usize = 2;

/// How many timed-out workers are still running.
pub fn stuck_workers() -> usize {
    ABANDONED.load(Ordering::SeqCst)
}

const RUNNING: u8 = 0;
const GIVEN_UP_ON: u8 = 1;
const FINISHED: u8 = 2;

/// Run `work` on its own thread and wait at most `timeout`. A panic becomes an [`ErrorKind::Internal`] error; so does
/// a timeout (the thread is left to finish on its own, nothing can stop FXC or `naga` from outside). Untrusted input
/// must only go through here.
pub(crate) fn run_bounded<T: Send + 'static>(
    what: &str,
    timeout: Duration,
    work: impl FnOnce() -> Result<T, ExtError> + Send + 'static,
) -> Result<T, ExtError> {
    if stuck_workers() >= MAX_ABANDONED {
        return Err(ExtError::internal(format!(
            "{what} refused: {} earlier shader jobs never finished and still use the CPU; restart Better Cam",
            stuck_workers()
        )));
    }
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(AtomicU8::new(RUNNING));
    let worker_state = state.clone();
    std::thread::Builder::new()
        .name(format!("ext {what}"))
        .stack_size(WORKER_STACK)
        .spawn(move || {
            let r = catch_unwind(AssertUnwindSafe(work));
            let _ = tx.send(r);
            if worker_state.swap(FINISHED, Ordering::SeqCst) == GIVEN_UP_ON {
                ABANDONED.fetch_sub(1, Ordering::SeqCst);
            }
        })
        .map_err(|e| ExtError::internal(format!("{what}: could not start a worker thread: {e}")))?;
    match rx.recv_timeout(timeout) {
        Ok(Ok(result)) => result,
        Ok(Err(panic)) => {
            let text = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            Err(ExtError::internal(format!("{what} failed inside the shader translator ({text}); the shader was not used")))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if state.compare_exchange(RUNNING, GIVEN_UP_ON, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                ABANDONED.fetch_add(1, Ordering::SeqCst);
            }
            Err(ExtError::internal(format!(
                "{what} did not finish within {} s; the shader was not used",
                timeout.as_secs()
            )))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(ExtError::internal(format!("{what}: the worker thread ended without a result")))
        }
    }
}

// ---- The registry (E3): finds extension folders, checks their manifests, keeps the list, tells the page ----

use manifest::{ActionDef, ExtKind, Manifest, ParamDef, RenderMode};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::SystemTime;

/// The kind of a chain entry that belongs to an extension: `ext:<id>`.
pub const KIND_PREFIX: &str = "ext:";

/// The registry the engine's filter chains ask for `ext:` kinds. `main` installs the app's; see [`registry`].
static GLOBAL: OnceLock<ExtRegistry> = OnceLock::new();

/// Make `registry` the one the filter chains use. The first call wins.
pub fn install_registry(registry: ExtRegistry) {
    let _ = GLOBAL.set(registry);
}

/// The registry the chains use: the installed one, else, in a process that installed none (`engine_script` and the other
/// examples), one on the folder named by the environment variable `BETTER_CAM_EXTENSIONS_DIR` (started on first use), else
/// none, and `ext:` cards are skipped as before.
pub fn registry() -> Option<ExtRegistry> {
    if let Some(r) = GLOBAL.get() {
        return Some(r.clone());
    }
    let dir = PathBuf::from(std::env::var_os("BETTER_CAM_EXTENSIONS_DIR").filter(|d| !d.is_empty())?);
    let reg = ExtRegistry::new();
    reg.start(dir.clone(), dir.join(".extensions-state.json"), None);
    install_registry(reg);
    GLOBAL.get().cloned()
}

/// A new filter for the chain entry `kind` (`ext:<id>`), whether or not the extension is installed and switched on right
/// now: the filter follows the registry, passes the picture through until its shader is built, and is a no-op while the
/// extension is switched off or gone. `None` when there is no registry or `kind` is not an extension kind.
pub fn make_filter(kind: &str) -> Option<Box<dyn crate::fx::GpuFilter>> {
    let id = kind.strip_prefix(KIND_PREFIX).filter(|id| !id.is_empty())?;
    Some(Box::new(filter::ExtShaderFilter::new(id, registry()?)))
}

/// How the page sees an extension (`docs/EXTENSIONS.md` 5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtState {
    /// The manifest and the files it names are valid and the user has not switched it off.
    Ready,
    /// Something about the extension is wrong; `error` says what.
    Error,
    /// Valid, but switched off in Settings → Extensions. Its cards stay in the chain and are bypassed.
    Disabled,
}

/// One extension as the page gets it: from `list_extensions` and the `extensions-changed` event.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ExtensionInfo {
    /// The folder name (equal to the manifest `id` when the manifest is valid).
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: Option<String>,
    pub license: Option<String>,
    pub hint: Option<String>,
    /// `null` when the manifest could not be read.
    pub kind: Option<ExtKind>,
    pub render: Option<RenderMode>,
    pub api_version: Option<u32>,
    /// The extension's folder, for the user (Settings → Extensions shows it).
    pub folder: String,
    /// The validated params, as `src/filters/schema.ts` `ParamDef`s. Empty when the manifest is invalid.
    pub params: Vec<ParamDef>,
    pub actions: Vec<ActionDef>,
    pub state: ExtState,
    pub enabled: bool,
    pub error: Option<String>,
}

/// What a poll compares: modification time and size of a file, `None` when it cannot be read.
type Stamp = Option<(SystemTime, u64)>;

fn stamp_of(path: &Path) -> Stamp {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

struct Loaded {
    info: ExtensionInfo,
    manifest: Option<Arc<Manifest>>,
    /// Every file the extension depends on, with its stamp when it was read.
    watch: Vec<(PathBuf, Stamp)>,
}

/// An immutable view of the registry, swapped whole on every change (the engine takes one per chain update).
#[derive(Debug, Default)]
pub struct Snapshot {
    /// Sorted by id.
    pub extensions: Vec<ExtensionInfo>,
    ready: BTreeMap<String, Arc<Manifest>>,
}

impl Snapshot {
    /// The manifest of an extension that is ready to run (valid and not switched off).
    pub fn ready(&self, id: &str) -> Option<&Arc<Manifest>> {
        self.ready.get(id)
    }

    pub fn info(&self, id: &str) -> Option<&ExtensionInfo> {
        self.extensions.iter().find(|e| e.id == id)
    }
}

/// Called with the whole list whenever it changed.
pub type Sink = Arc<dyn Fn(&[ExtensionInfo]) + Send + Sync>;

#[derive(Default)]
struct Config {
    root: Option<PathBuf>,
    state_file: Option<PathBuf>,
    sink: Option<Sink>,
}

#[derive(Default)]
struct Inner {
    loaded: BTreeMap<String, Loaded>,
    /// Ids the user switched off. Ids of extensions that are not installed stay in, so a reinstall stays off.
    disabled: BTreeSet<String>,
}

struct Shared {
    config: Mutex<Config>,
    inner: Mutex<Inner>,
    snapshot: Mutex<Arc<Snapshot>>,
    /// Held for the whole of a [`ExtRegistry::scan`], so that two scans (the poll and a Reload or switch from the page)
    /// never overlap: the sink is told in the order the snapshots were swapped, and the last list it got is the one
    /// `list` returns.
    scanning: Mutex<()>,
    stop: Mutex<bool>,
    wake: Condvar,
    /// Counts the scans that changed something an extension filter looks at (an extension reloaded, added, removed,
    /// switched on or off). The render thread reads it with one atomic load per frame.
    generation: AtomicU64,
}

/// Finds the extension folders under one root, validates their manifests and keeps the list.
///
/// Threads: [`ExtRegistry::start`] runs one thread (`ext registry`) that looks at the folder once a second and reloads
/// an extension whose manifest or named files changed (modification time or size), added or removed. Everything else
/// (`list`, `reload`, `set_enabled`) runs on the caller's thread and only holds the registry's locks for a moment: the
/// folder is read outside them. Nothing here is called from the render thread; the engine takes a [`Snapshot`].
#[derive(Clone)]
pub struct ExtRegistry {
    shared: Arc<Shared>,
}

impl Default for ExtRegistry {
    fn default() -> Self {
        ExtRegistry {
            shared: Arc::new(Shared {
                config: Mutex::new(Config::default()),
                inner: Mutex::new(Inner::default()),
                snapshot: Mutex::new(Arc::new(Snapshot::default())),
                scanning: Mutex::new(()),
                stop: Mutex::new(false),
                wake: Condvar::new(),
                generation: AtomicU64::new(0),
            }),
        }
    }
}

const POLL: Duration = Duration::from_secs(1);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ExtRegistry {
    pub fn new() -> ExtRegistry {
        ExtRegistry::default()
    }

    /// Point the registry at `root` (`%APPDATA%\dev.bettercam.app\extensions`, which need not exist), read which
    /// extensions the user switched off from `state_file` and scan once. No thread runs yet.
    pub fn configure(&self, root: PathBuf, state_file: PathBuf, sink: Option<Sink>) {
        let disabled = read_disabled(&state_file);
        {
            let mut cfg = lock(&self.shared.config);
            *cfg = Config { root: Some(root), state_file: Some(state_file), sink };
        }
        lock(&self.shared.inner).disabled = disabled;
        self.scan(true);
    }

    /// [`ExtRegistry::configure`], then the polling thread. Call it once per registry.
    pub fn start(&self, root: PathBuf, state_file: PathBuf, sink: Option<Sink>) {
        *lock(&self.shared.stop) = false;
        self.configure(root, state_file, sink);
        let shared = self.shared.clone();
        let me = ExtRegistry { shared: shared.clone() };
        let spawned = std::thread::Builder::new().name("ext registry".into()).spawn(move || loop {
            {
                let stop = lock(&shared.stop);
                let (stop, _) = shared.wake.wait_timeout(stop, POLL).unwrap_or_else(|e| e.into_inner());
                if *stop {
                    return;
                }
            }
            // A bug in one poll must not end the watcher.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                me.scan(false);
            }));
        });
        if let Err(e) = spawned {
            eprintln!("[ext] cannot start the registry thread: {e}");
        }
    }

    /// End the polling thread (it finishes within a moment).
    pub fn stop(&self) {
        *lock(&self.shared.stop) = true;
        self.shared.wake.notify_all();
    }

    /// The extensions folder, once the registry was configured.
    pub fn root(&self) -> Option<PathBuf> {
        lock(&self.shared.config).root.clone()
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        lock(&self.shared.snapshot).clone()
    }

    /// Changes whenever the snapshot got a different extension (see [`Shared::generation`]); equal values mean the
    /// manifests an engine looked at are still the current ones. Cheap enough for every frame.
    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Acquire)
    }

    pub fn list(&self) -> Vec<ExtensionInfo> {
        self.snapshot().extensions.clone()
    }

    /// Read everything again now (the Reload button), whether or not a file changed.
    pub fn reload(&self) -> Vec<ExtensionInfo> {
        self.scan(true)
    }

    /// Switch an installed extension on or off; remembered across restarts. An unknown id is an error.
    pub fn set_enabled(&self, id: &str, on: bool) -> Result<Vec<ExtensionInfo>, String> {
        {
            let mut inner = lock(&self.shared.inner);
            if !inner.loaded.contains_key(id) {
                return Err(format!("There is no extension “{id}”."));
            }
            let mut disabled = inner.disabled.clone();
            if on {
                disabled.remove(id);
            } else {
                disabled.insert(id.to_string());
            }
            if let Some(file) = lock(&self.shared.config).state_file.clone() {
                write_disabled(&file, &disabled)?;
            }
            inner.disabled = disabled;
        }
        Ok(self.scan(false))
    }

    /// Look at the folder; with `force` read every extension again, else only the ones whose files changed. Rebuilds
    /// the snapshot and, when the list differs from the previous one, calls the sink. Returns the list.
    pub(crate) fn scan(&self, force: bool) -> Vec<ExtensionInfo> {
        // Before anything is read: a scan that starts later also reads later, so its list is the newer one. The sink
        // runs under this lock and must not call back into the registry.
        let _one_at_a_time = lock(&self.shared.scanning);
        let Some(root) = lock(&self.shared.config).root.clone() else {
            return Vec::new();
        };
        let names = folder_names(&root);
        // Read outside the locks: a slow disk must not hold up `list`.
        let known: BTreeMap<String, Vec<(PathBuf, Stamp)>> =
            lock(&self.shared.inner).loaded.iter().map(|(k, v)| (k.clone(), v.watch.clone())).collect();
        let mut fresh: Vec<(String, Loaded)> = Vec::new();
        for name in &names {
            let unchanged = !force && known.get(name).is_some_and(|w| w.iter().all(|(p, s)| stamp_of(p) == *s));
            if !unchanged {
                fresh.push((name.clone(), load_one(&root, name)));
            }
        }

        let (snapshot, previous, sink) = {
            let mut inner = lock(&self.shared.inner);
            let before = inner.loaded.len();
            inner.loaded.retain(|id, _| names.contains(id));
            let removed = inner.loaded.len() != before;
            let reloaded = !fresh.is_empty();
            for (name, l) in fresh {
                inner.loaded.insert(name, l);
            }
            let snap = Arc::new(build_snapshot(&inner));
            let mut current = lock(&self.shared.snapshot);
            let previous = std::mem::replace(&mut *current, snap.clone());
            if removed || reloaded || previous.extensions != snap.extensions {
                self.shared.generation.fetch_add(1, Ordering::AcqRel);
            }
            (snap, previous, lock(&self.shared.config).sink.clone())
        };
        if previous.extensions != snapshot.extensions {
            if let Some(sink) = sink {
                sink(&snapshot.extensions);
            }
        }
        snapshot.extensions.clone()
    }
}

/// Folder names under `root` that can hold an extension: directories whose name does not start with ".".
/// (`.sdk` is the app's own folder.) Sorted.
fn folder_names(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| std::fs::metadata(e.path()).is_ok_and(|m| m.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names
}

fn build_snapshot(inner: &Inner) -> Snapshot {
    let mut snap = Snapshot::default();
    for (id, l) in &inner.loaded {
        let mut info = l.info.clone();
        if info.state != ExtState::Error {
            let off = inner.disabled.contains(id);
            info.enabled = !off;
            info.state = if off { ExtState::Disabled } else { ExtState::Ready };
            if !off {
                if let Some(m) = &l.manifest {
                    snap.ready.insert(id.clone(), m.clone());
                }
            }
        }
        snap.extensions.push(info);
    }
    snap
}

/// Read, parse and check one extension folder. Never fails: a problem becomes an entry in the `Error` state.
fn load_one(root: &Path, folder: &str) -> Loaded {
    let dir = root.join(folder);
    let manifest_path = dir.join("extension.json");
    let mut watch = vec![(manifest_path.clone(), stamp_of(&manifest_path))];
    let mut info = ExtensionInfo {
        id: folder.to_string(),
        name: folder.to_string(),
        version: String::new(),
        author: None,
        license: None,
        hint: None,
        kind: None,
        render: None,
        api_version: None,
        folder: dir.to_string_lossy().into_owned(),
        params: Vec::new(),
        actions: Vec::new(),
        state: ExtState::Ready,
        enabled: true,
        error: None,
    };
    let fail = |mut info: ExtensionInfo, message: String, watch: Vec<(PathBuf, Stamp)>| {
        info.state = ExtState::Error;
        info.error = Some(message);
        Loaded { info, manifest: None, watch }
    };

    let text = match read_manifest(&manifest_path) {
        Ok(t) => t,
        Err(message) => return fail(info, message, watch),
    };
    // Name and version are shown even when the rest is wrong.
    if let Ok(serde_json::Value::Object(o)) = serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}')) {
        for (key, slot) in [("name", &mut info.name), ("version", &mut info.version)] {
            if let Some(s) = o.get(key).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty() && s.chars().count() <= 48) {
                *slot = s.to_string();
            }
        }
    }
    let manifest = match Manifest::parse(&text, folder) {
        Ok(m) => m,
        Err(e) => return fail(info, format!("extension.json: {}", e.message), watch),
    };
    for file in manifest.files() {
        let p = dir.join(file);
        watch.push((p.clone(), stamp_of(&p)));
    }
    info.name = manifest.name.clone();
    info.version = manifest.version.clone();
    info.author = manifest.author.clone();
    info.license = manifest.license.clone();
    info.hint = manifest.hint.clone();
    info.kind = Some(manifest.kind);
    info.render = manifest.render;
    info.api_version = Some(manifest.api_version);
    if let Err(e) = manifest::check_files(&dir, &manifest) {
        return fail(info, e.message, watch);
    }
    info.params = manifest.params.clone();
    info.actions = manifest.actions.clone();
    Loaded { info, manifest: Some(Arc::new(manifest)), watch }
}

fn read_manifest(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "extension.json is missing: every extension folder needs one".to_string()
        } else {
            format!("extension.json cannot be read: {e}")
        }
    })?;
    if !meta.is_file() {
        return Err("extension.json is not a file".into());
    }
    if meta.len() > manifest::MAX_FILE_BYTES {
        return Err(format!("extension.json is {} KB; the limit is {} KB", meta.len().div_ceil(1024), manifest::MAX_FILE_BYTES / 1024));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("extension.json cannot be read: {e}"))?;
    String::from_utf8(bytes).map_err(|_| "extension.json is not UTF-8 text".to_string())
}

/// The ids in `{"disabled": [...]}`; a missing or damaged file means none.
fn read_disabled(file: &Path) -> BTreeSet<String> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return BTreeSet::new();
    };
    let Ok(serde_json::Value::Object(o)) = serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}')) else {
        return BTreeSet::new();
    };
    o.get("disabled")
        .and_then(|d| d.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn write_disabled(file: &Path, disabled: &BTreeSet<String>) -> Result<(), String> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("could not save the extension settings: {e}"))?;
    }
    let json = serde_json::json!({ "disabled": disabled });
    std::fs::write(file, serde_json::to_string_pretty(&json).unwrap_or_default())
        .map_err(|e| format!("could not save the extension settings: {e}"))
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A scratch folder that goes away with the test.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("better-cam-registry-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn root(&self) -> PathBuf {
            self.0.join("extensions")
        }
        fn state(&self) -> PathBuf {
            self.0.join("extensions-state.json")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const MANIFEST: &str = r#"{ "apiVersion": 1, "id": "ID", "name": "NAME", "version": "1.0.0", "kind": "filter", "shader": "a.wgsl",
        "params": [ { "type": "slider", "key": "amount", "label": "Amount", "min": 0, "max": 1, "step": 0.1, "def": 0.5 } ] }"#;

    fn install(s: &Scratch, id: &str, name: &str) {
        let dir = s.root().join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("extension.json"), MANIFEST.replace("ID", id).replace("NAME", name)).unwrap();
        std::fs::write(dir.join("a.wgsl"), "// shader").unwrap();
    }

    /// Counts the events the sink got.
    fn counting() -> (Sink, Arc<AtomicUsize>) {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        (Arc::new(move |_: &[ExtensionInfo]| { n2.fetch_add(1, Ordering::SeqCst); }), n)
    }

    #[test]
    fn a_missing_folder_is_an_empty_list() {
        let s = Scratch::new("missing");
        let reg = ExtRegistry::new();
        reg.configure(s.root(), s.state(), None);
        assert!(reg.list().is_empty());
        assert!(reg.reload().is_empty());
    }

    #[test]
    fn finds_lists_and_forgets_extensions_and_tells_the_sink_once_per_change() {
        let s = Scratch::new("find");
        let (sink, events) = counting();
        let reg = ExtRegistry::new();
        reg.configure(s.root(), s.state(), Some(sink));
        assert_eq!(events.load(Ordering::SeqCst), 0);

        install(&s, "vignette", "Vignette");
        std::fs::create_dir_all(s.root().join(".sdk")).unwrap();
        reg.scan(false);
        let list = reg.list();
        assert_eq!(list.len(), 1, ".sdk is not an extension");
        assert_eq!((list[0].id.as_str(), list[0].name.as_str(), list[0].state), ("vignette", "Vignette", ExtState::Ready));
        assert_eq!(list[0].kind, Some(ExtKind::Filter));
        assert_eq!(list[0].params.len(), 1);
        assert!(reg.snapshot().ready("vignette").is_some());
        assert_eq!(events.load(Ordering::SeqCst), 1);

        // Nothing changed: no event.
        reg.scan(false);
        reg.reload();
        assert_eq!(events.load(Ordering::SeqCst), 1);

        std::fs::remove_dir_all(s.root().join("vignette")).unwrap();
        assert!(reg.scan(false).is_empty());
        assert!(reg.snapshot().ready("vignette").is_none());
        assert_eq!(events.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_changed_manifest_or_shader_reloads_that_extension() {
        let s = Scratch::new("change");
        let reg = ExtRegistry::new();
        install(&s, "tint", "Tint");
        reg.configure(s.root(), s.state(), None);
        assert_eq!(reg.list()[0].name, "Tint");

        // A manifest rewritten with a different size is seen by the cheap poll.
        install(&s, "tint", "Tint, renamed");
        assert_eq!(reg.scan(false)[0].name, "Tint, renamed");

        // A shader that goes missing makes the extension an error, and coming back clears it.
        let shader = s.root().join("tint").join("a.wgsl");
        std::fs::remove_file(&shader).unwrap();
        let broken = reg.scan(false);
        assert_eq!(broken[0].state, ExtState::Error);
        assert!(broken[0].error.as_deref().unwrap().contains("shader: a.wgsl does not exist"), "{:?}", broken[0].error);
        assert!(reg.snapshot().ready("tint").is_none());
        std::fs::write(&shader, "// back").unwrap();
        assert_eq!(reg.scan(false)[0].state, ExtState::Ready);
    }

    #[test]
    fn a_bad_manifest_is_listed_with_its_error_and_the_others_still_load() {
        let s = Scratch::new("bad");
        install(&s, "good", "Good");
        let bad = s.root().join("broken");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("extension.json"), r#"{ "apiVersion": 1, "id": "broken", "name": "Broken", "version": "1.0", "kind": "filter" }"#).unwrap();
        let empty = s.root().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let future = s.root().join("future");
        std::fs::create_dir_all(&future).unwrap();
        std::fs::write(future.join("extension.json"), r#"{ "apiVersion": 2, "id": "future", "name": "Future", "version": "1.0.0" }"#).unwrap();
        let reg = ExtRegistry::new();
        reg.configure(s.root(), s.state(), None);
        let list = reg.list();
        let by = |id: &str| list.iter().find(|e| e.id == id).unwrap();
        assert_eq!(by("good").state, ExtState::Ready);
        assert_eq!(by("broken").state, ExtState::Error);
        assert_eq!(by("broken").name, "Broken", "name is read even from a bad manifest");
        assert!(by("broken").error.as_deref().unwrap().starts_with("extension.json: version:"));
        assert!(by("empty").error.as_deref().unwrap().contains("extension.json is missing"));
        assert!(by("future").error.as_deref().unwrap().contains("Needs a newer Better Cam (extension API 2)"));
        assert!(reg.snapshot().ready("good").is_some() && reg.snapshot().ready("broken").is_none());
    }

    #[test]
    fn disabling_is_remembered_across_registries_and_keeps_params() {
        let s = Scratch::new("disable");
        install(&s, "tint", "Tint");
        let reg = ExtRegistry::new();
        reg.configure(s.root(), s.state(), None);
        let list = reg.set_enabled("tint", false).unwrap();
        assert_eq!((list[0].state, list[0].enabled, list[0].params.len()), (ExtState::Disabled, false, 1));
        assert!(reg.snapshot().ready("tint").is_none());
        assert!(reg.set_enabled("nope", false).is_err());

        let again = ExtRegistry::new();
        again.configure(s.root(), s.state(), None);
        assert_eq!(again.list()[0].state, ExtState::Disabled);
        assert_eq!(again.set_enabled("tint", true).unwrap()[0].state, ExtState::Ready);
        assert!(read_disabled(&s.state()).is_empty());
    }

    /// Waits (at most 5 s) until the sink was called `n` times in total.
    fn wait_for_events(events: &AtomicUsize, n: usize) -> Duration {
        let started = std::time::Instant::now();
        while events.load(Ordering::SeqCst) < n && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
        started.elapsed()
    }

    #[test]
    fn the_polling_thread_survives_the_folder_being_deleted_and_made_again() {
        let s = Scratch::new("recreate");
        let (sink, events) = counting();
        let last: Arc<Mutex<Vec<String>>> = Arc::default();
        let last2 = last.clone();
        let sink2: Sink = Arc::new(move |list: &[ExtensionInfo]| {
            *lock(&last2) = list.iter().map(|e| e.id.clone()).collect();
            sink(list);
        });
        install(&s, "vignette", "Vignette");
        let reg = ExtRegistry::new();
        reg.start(s.root(), s.state(), Some(sink2));
        assert_eq!(reg.list().len(), 1);
        // The first read (in `start`) already told the sink once.
        assert_eq!(events.load(Ordering::SeqCst), 1);

        for round in 1..=2usize {
            std::fs::remove_dir_all(s.root()).unwrap();
            let took = wait_for_events(&events, round * 2);
            assert_eq!(events.load(Ordering::SeqCst), round * 2, "round {round}: no event for the deleted folder after {took:?}");
            assert!(lock(&last).is_empty(), "round {round}: the emitted list is not empty");

            install(&s, "vignette", "Vignette");
            let took = wait_for_events(&events, round * 2 + 1);
            assert_eq!(events.load(Ordering::SeqCst), round * 2 + 1, "round {round}: no event for the re-created folder after {took:?}");
            assert_eq!(*lock(&last), vec!["vignette".to_string()]);
            assert!(took < Duration::from_millis(2500), "round {round}: found after {took:?}");
        }
        reg.stop();
    }

    #[test]
    fn two_overlapping_scans_leave_the_sink_with_the_newest_list() {
        let s = Scratch::new("overlap");
        install(&s, "vignette", "Vignette");
        let last: Arc<Mutex<Vec<String>>> = Arc::default();
        let last2 = last.clone();
        // A sink that is slow to take the empty list (the page busy), as when the poll and a Reload meet.
        let sink: Sink = Arc::new(move |list: &[ExtensionInfo]| {
            if list.is_empty() {
                std::thread::sleep(Duration::from_millis(300));
            }
            *lock(&last2) = list.iter().map(|e| e.id.clone()).collect();
        });
        let reg = ExtRegistry::new();
        reg.configure(s.root(), s.state(), Some(sink));
        std::fs::remove_dir_all(s.root().join("vignette")).unwrap();
        let poll = {
            let reg = reg.clone();
            std::thread::spawn(move || reg.scan(false))
        };
        std::thread::sleep(Duration::from_millis(100));
        install(&s, "vignette", "Vignette");
        let reload = reg.reload();
        poll.join().unwrap();
        assert_eq!(reload.len(), 1);
        assert_eq!(reg.list().len(), 1);
        assert_eq!(*lock(&last), vec!["vignette".to_string()], "the page was left with an older list than list_extensions returns");
    }

    #[test]
    fn the_polling_thread_picks_up_a_new_folder_without_a_call() {
        let s = Scratch::new("thread");
        let (sink, events) = counting();
        let reg = ExtRegistry::new();
        reg.start(s.root(), s.state(), Some(sink));
        install(&s, "late", "Late");
        let started = std::time::Instant::now();
        while reg.list().is_empty() && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
        reg.stop();
        let list = reg.list();
        assert_eq!(list.len(), 1, "the thread did not find the folder within 5 s");
        assert!(started.elapsed() < Duration::from_millis(2500), "found after {:?}", started.elapsed());
        assert_eq!(events.load(Ordering::SeqCst), 1);
    }
}
