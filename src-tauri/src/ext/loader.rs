//! The loader: turns an extension's shader file into bytecode off the render thread (`docs/EXTENSIONS.md` 4.5, 5.1).
//!
//! ```text
//! filter (render thread)            loader thread ("ext loader")              bounded workers (ext::run_bounded)
//!   submit(spec, device) ─ticket─►  read file ─► hash ─► cache hit? ─────► done
//!   ticket.take() each frame                              │ miss
//!                                                         ├─► naga translation (10 s, 512 MiB stack) ┐ .wgsl
//!                                                         └─► FXC ps_5_0 (30 s, 512 MiB stack) ──────┘ .hlsl: FXC only
//!                                   CreatePixelShader on the filter's device ─► ticket
//! ```
//!
//! The loader thread never calls `naga` or FXC itself: both run on their own worker, which can be abandoned on a
//! timeout (and counted, `ext::stuck_workers`). Jobs run one at a time, in order; a job whose [`Ticket`] was dropped
//! before it started is skipped. Results are cached by a hash of everything that goes into the translation (the source
//! text, the kind of file, the `Params` members, the image names), so a reload that did not change the shader (the
//! manifest or a script changed) costs a file read, and a source that failed once is not tried again until it changes,
//! which also stops a shader that hangs FXC from starting a new stuck worker on every poll.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11PixelShader};

use super::compile::compile_ps;
use super::hlsl::translate_hlsl;
use super::wgsl::{translate, ParamDecl, Translated, MAX_SOURCE_BYTES};
use super::{ErrorKind, ExtError};

/// What to build: one shader file of an extension.
#[derive(Clone, Debug)]
pub struct BuildSpec {
    /// The file name as the author knows it, for messages (`vhs.wgsl`).
    pub file_name: String,
    pub path: PathBuf,
    /// `.hlsl` (the generated prelude and FXC only) instead of WGSL.
    pub hlsl: bool,
    /// The members of `Params`, in order.
    pub params: Vec<ParamDecl>,
    /// The keys of the `path` params, in order.
    pub images: Vec<String>,
}

/// A shader that compiled: the translation (layouts, texture slots) and the `ps_5_0` bytecode.
#[derive(Debug)]
pub struct Built {
    pub translated: Translated,
    pub bytecode: Vec<u8>,
    /// Hash of the source and what was translated with it; the cache key.
    pub source_hash: u64,
}

/// The message for the card (starts with the file name) or a built shader.
pub type BuildResult = Result<Arc<Built>, String>;

/// A built shader, with the pixel shader made on the requesting device.
pub struct Loaded {
    pub built: Arc<Built>,
    pub ps: ID3D11PixelShader,
}

// ---------------------------------------------------------------------------------------------------------------
// The cache

const CACHE_ENTRIES: usize = 32;

#[derive(Default)]
struct Cache {
    map: HashMap<u64, Result<Arc<Built>, ExtError>>,
    order: VecDeque<u64>,
}

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

fn cache() -> std::sync::MutexGuard<'static, Cache> {
    CACHE.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// How many results the cache holds (tests).
pub fn cached_results() -> usize {
    cache().map.len()
}

/// Whether the file `spec` names, as it is now, has a cached result (tests).
pub fn is_cached(spec: &BuildSpec) -> bool {
    read_source(spec).is_ok_and(|source| cache().map.contains_key(&source_hash(spec, &source)))
}

/// Forget every cached result (tests).
pub fn clear_cache() {
    *cache() = Cache::default();
}

fn source_hash(spec: &BuildSpec, source: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    spec.hlsl.hash(&mut h);
    source.hash(&mut h);
    for p in &spec.params {
        p.name.hash(&mut h);
        p.ty.hash(&mut h);
    }
    spec.images.hash(&mut h);
    h.finish()
}

// ---------------------------------------------------------------------------------------------------------------
// Building

/// Read the file and build it (cache, translate, FXC), on the calling thread, which only waits for the bounded
/// workers. The loader thread calls it; so do tests. A message already carries the file name.
pub fn build(spec: &BuildSpec) -> BuildResult {
    let source = read_source(spec)?;
    let key = source_hash(spec, &source);
    let hit = cache().map.get(&key).cloned();
    let result = hit.unwrap_or_else(|| {
        let result = compile_source(spec, &source, key).map(Arc::new);
        // A refusal because earlier jobs are stuck is not about this source: do not remember it.
        let transient = matches!(&result, Err(e) if e.kind == ErrorKind::Internal && e.message.contains("restart Better Cam"));
        if !transient {
            let mut c = cache();
            if c.map.insert(key, result.clone()).is_none() {
                c.order.push_back(key);
                while c.order.len() > CACHE_ENTRIES {
                    if let Some(old) = c.order.pop_front() {
                        c.map.remove(&old);
                    }
                }
            }
        }
        result
    });
    // The message is made here, not cached: the same text under another file name reads with that name.
    result.map_err(|e| e.render(&spec.file_name))
}

fn read_source(spec: &BuildSpec) -> Result<String, String> {
    let name = &spec.file_name;
    let meta = std::fs::metadata(&spec.path).map_err(|e| format!("{name}: cannot be read: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{name}: is not a file"));
    }
    if meta.len() > MAX_SOURCE_BYTES as u64 {
        return Err(format!("{name}: the shader is {} KB, the limit is {} KB", meta.len().div_ceil(1024), MAX_SOURCE_BYTES / 1024));
    }
    let bytes = std::fs::read(&spec.path).map_err(|e| format!("{name}: cannot be read: {e}"))?;
    String::from_utf8(bytes).map_err(|_| format!("{name}: is not UTF-8 text"))
}

fn compile_source(spec: &BuildSpec, source: &str, key: u64) -> Result<Built, ExtError> {
    let images: Vec<&str> = spec.images.iter().map(String::as_str).collect();
    if spec.hlsl {
        let (translated, bytecode) = translate_hlsl(source, &spec.params, &images)?;
        return Ok(Built { translated, bytecode, source_hash: key });
    }
    let translated = translate(source, &spec.params, &images)?;
    // The label is only FXC's name for the generated text: the cache is keyed by source, so a result must not carry
    // the file name of whichever extension asked first.
    let bytecode = compile_ps("shader.wgsl", &translated.hlsl)?;
    Ok(Built { translated, bytecode, source_hash: key })
}

// ---------------------------------------------------------------------------------------------------------------
// The loader thread

struct TicketInner {
    result: Mutex<Option<Result<Loaded, String>>>,
    cancelled: AtomicBool,
}

/// A request in flight. Dropping it cancels the job if it has not started, and discards the result if it has.
pub struct Ticket {
    inner: Arc<TicketInner>,
}

impl Ticket {
    /// The result, once; `None` while the job is queued or running, or when the lock is busy for a moment.
    pub fn take(&self) -> Option<Result<Loaded, String>> {
        self.inner.result.try_lock().ok()?.take()
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.inner.cancelled.store(true, Ordering::Release);
    }
}

struct Job {
    spec: BuildSpec,
    device: ID3D11Device,
    ticket: Arc<TicketInner>,
}

struct Queue {
    jobs: Mutex<VecDeque<Job>>,
    wake: Condvar,
}

static QUEUE: OnceLock<Arc<Queue>> = OnceLock::new();

fn queue() -> &'static Arc<Queue> {
    QUEUE.get_or_init(|| {
        let q = Arc::new(Queue { jobs: Mutex::new(VecDeque::new()), wake: Condvar::new() });
        let worker = q.clone();
        if let Err(e) = std::thread::Builder::new().name("ext loader".into()).spawn(move || run(worker)) {
            eprintln!("[ext] cannot start the loader thread: {e}");
        }
        q
    })
}

fn run(q: Arc<Queue>) {
    loop {
        let job = {
            let mut jobs = q.jobs.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(j) = jobs.pop_front() {
                    break j;
                }
                jobs = q.wake.wait(jobs).unwrap_or_else(|e| e.into_inner());
            }
        };
        if job.ticket.cancelled.load(Ordering::Acquire) {
            continue;
        }
        // A panic in a job must not end the loader: the card gets a message and the next job runs.
        let outcome = catch_unwind(AssertUnwindSafe(|| make_loaded(&job)))
            .unwrap_or_else(|_| Err(format!("{}: the shader loader failed; the shader was not used", job.spec.file_name)));
        if !job.ticket.cancelled.load(Ordering::Acquire) {
            *job.ticket.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
        }
    }
}

fn make_loaded(job: &Job) -> Result<Loaded, String> {
    let built = build(&job.spec)?;
    let mut ps = None;
    // SAFETY: plain call with bytecode FXC produced; the device is free-threaded.
    unsafe { job.device.CreatePixelShader(&built.bytecode, None, Some(&mut ps)) }
        .map_err(|e| format!("{}: the GPU refused the shader ({e})", job.spec.file_name))?;
    let ps = ps.ok_or_else(|| format!("{}: the GPU made no shader", job.spec.file_name))?;
    Ok(Loaded { built, ps })
}

/// Queue a build for the filter on `device`. Never blocks.
pub fn submit(spec: BuildSpec, device: ID3D11Device) -> Ticket {
    let inner = Arc::new(TicketInner { result: Mutex::new(None), cancelled: AtomicBool::new(false) });
    let q = queue();
    q.jobs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_back(Job { spec, device, ticket: inner.clone() });
    q.wake.notify_one();
    Ticket { inner }
}

/// Jobs waiting for the loader (tests).
pub fn queued_jobs() -> usize {
    queue().jobs.lock().unwrap_or_else(|e| e.into_inner()).len()
}
