//! Extensions (spike E1): custom filters written in WGSL.
//!
//! This module only proves the shader path: [`wgsl::translate`] turns WGSL into HLSL and the reflected uniform
//! layouts, [`compile::compile_ps`] compiles the HLSL with FXC (`ps_5_0`), and [`layout`] packs parameter values
//! into the layout `naga` reflected. There is no manifest loader, registry or engine wiring yet; see
//! `docs/SPIKE-EXT.md` for the verdict, the exact shader convention and the measurements.

pub mod compile;
pub mod layout;
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
