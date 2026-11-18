//! FXC compilation of translated filters, bounded in time and isolated from panics.

use std::time::Duration;

use super::{run_bounded, ExtError};
use crate::gpu::{compile_shader, GpuError};

/// How long FXC may take. Long unrolled loops and huge functions can make FXC slow; it cannot be cancelled, so the
/// caller stops waiting and the worker thread ends when FXC does.
pub const COMPILE_TIMEOUT: Duration = Duration::from_secs(30);

/// Compile translated HLSL (entry point `main`) to `ps_5_0` bytecode with the engine's own compile function.
///
/// FXC's message refers to the generated HLSL, whose lines mean nothing to the author, so the error carries the
/// offending HLSL line next to the message.
pub fn compile_ps(name: &str, hlsl: &str) -> Result<Vec<u8>, ExtError> {
    let (name, source) = (name.to_string(), hlsl.to_string());
    run_bounded("FXC compile", COMPILE_TIMEOUT, move || {
        compile_shader(&name, &source, "main", "ps_5_0").map_err(|e| match e {
            GpuError::Shader { log, .. } => ExtError::fxc(with_context(&log, &source)),
            other => ExtError::fxc(other.to_string()),
        })
    })
}

/// "name(12,5): error X3004: ..." plus the text of HLSL line 12.
fn with_context(log: &str, hlsl: &str) -> String {
    let line = log
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(','))
        .and_then(|(n, _)| n.trim().parse::<usize>().ok());
    match line.and_then(|n| hlsl.lines().nth(n.wrapping_sub(1))) {
        Some(text) => format!("{log}\n  generated HLSL: {}", text.trim()),
        None => log.to_string(),
    }
}
