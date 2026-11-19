//! FXC compilation of translated filters, bounded in time and isolated from panics.

use std::time::Duration;

use super::{run_bounded, ErrorKind, ExtError};
use crate::gpu::{compile_shader, GpuError};

/// How long FXC may take. Long unrolled loops and huge functions can make FXC slow; it cannot be cancelled, so the
/// caller stops waiting and the worker thread ends when FXC does.
pub const COMPILE_TIMEOUT: Duration = Duration::from_secs(30);

/// Compile translated HLSL (entry point `main`) to `ps_5_0` bytecode with the engine's own compile function.
///
/// FXC's message refers to the generated HLSL, whose lines mean nothing to the author, so the error carries the
/// message without its position, and the offending HLSL line next to it. `name` is only a label for FXC.
pub fn compile_ps(name: &str, hlsl: &str) -> Result<Vec<u8>, ExtError> {
    let (name, source) = (name.to_string(), hlsl.to_string());
    run_bounded("FXC compile", COMPILE_TIMEOUT, move || {
        compile_shader(&name, &source, "main", "ps_5_0").map_err(|e| match e {
            GpuError::Shader { log, .. } => ExtError::fxc(with_context(&log, &name, &source)),
            other => ExtError::fxc(other.to_string()),
        })
    })
}

/// Compile the full text of an `.hlsl` extension (generated prelude plus the author's file, entry point `main`) with
/// FXC on a bounded worker. Unlike translated WGSL, this source is the author's own, so FXC's `(line,column)` can be
/// mapped back: `author_line` is the line of the full text where the author's file starts (1-based).
pub fn compile_hlsl(full: &str, author_line: u32) -> Result<Vec<u8>, ExtError> {
    let source = full.to_string();
    run_bounded("FXC compile", COMPILE_TIMEOUT, move || {
        compile_shader(HLSL_NAME, &source, "main", "ps_5_0").map_err(|e| match e {
            GpuError::Shader { log, .. } => hlsl_error(&log, author_line),
            other => ExtError::fxc(other.to_string()),
        })
    })
}

/// The name FXC prints in front of `(line,column)` for an `.hlsl` extension.
const HLSL_NAME: &str = "shader.hlsl";

/// One line of FXC's log read as `[dir\]name(line,column[-column]): message`.
struct Located {
    line: u32,
    column: u32,
    message: String,
}

/// FXC puts the directory of the process in front of the name it was given; this finds `name(` after any such prefix
/// (a path ends in a separator before the name) and reads the position and the text after it.
fn locate(line: &str, name: &str) -> Option<Located> {
    let key = format!("{name}(");
    let at = line.find(&key)?;
    let prefix = &line[..at];
    if !(prefix.is_empty() || prefix.ends_with(['\\', '/'])) {
        return None;
    }
    let rest = &line[at + key.len()..];
    let (pos, tail) = rest.split_once(')')?;
    let (l, c) = pos.split_once(',')?;
    Some(Located {
        line: l.trim().parse().ok()?,
        column: c.split('-').next()?.trim().parse().ok()?,
        message: tail.trim_start_matches(':').trim().to_string(),
    })
}

/// The first error of FXC's log (else its first line).
fn first_error(log: &str) -> Option<&str> {
    log.lines().find(|l| l.contains("error")).or_else(|| log.lines().find(|l| !l.trim().is_empty()))
}

/// The first error of FXC's log, with its position counted in the author's file; further errors are summarised.
fn hlsl_error(log: &str, author_line: u32) -> ExtError {
    let Some(first) = first_error(log) else {
        return ExtError::fxc("the shader did not compile");
    };
    let more = log.lines().filter(|l| l.contains("error")).count().saturating_sub(1);
    let suffix = if more > 0 { format!(" (and {more} more)") } else { String::new() };
    match locate(first, HLSL_NAME) {
        Some(at) if at.line >= author_line => ExtError {
            kind: ErrorKind::Fxc,
            message: format!("{}{suffix}", at.message),
            line: Some(at.line - author_line + 1),
            column: Some(at.column),
        },
        Some(at) => ExtError::fxc(format!("in the generated prelude: {}{suffix}", at.message)),
        None => ExtError::fxc(format!("{}{suffix}", first.trim())),
    }
}

/// FXC's first error without its position (and without the directory FXC prints), plus the text of the HLSL line it
/// points at: `error X3004: undeclared identifier 'x'` / `generated HLSL: return x;`.
fn with_context(log: &str, name: &str, hlsl: &str) -> String {
    let Some(first) = first_error(log) else {
        return "the shader did not compile".to_string();
    };
    match locate(first, name) {
        Some(at) => match hlsl.lines().nth((at.line as usize).wrapping_sub(1)) {
            Some(text) => format!("{}\n  generated HLSL: {}", at.message, text.trim()),
            None => at.message,
        },
        None => first.trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fxc_positions_are_read_after_a_directory_prefix() {
        let l = locate(r"D:\Program Files (x86)\App\shader.hlsl(35,17-30): error X3004: undeclared identifier 'x'", HLSL_NAME)
            .expect("located");
        assert_eq!((l.line, l.column), (35, 17));
        assert_eq!(l.message, "error X3004: undeclared identifier 'x'");
        let l = locate("shader.hlsl(3,1): error X1: bad", HLSL_NAME).expect("located");
        assert_eq!((l.line, l.column), (3, 1));
        // another name, or text that only looks like a position
        assert!(locate("other.hlsl(3,1): error", HLSL_NAME).is_none());
        assert!(locate("see myshader.hlsl(3,1): error", HLSL_NAME).is_none());
        assert!(locate("shader.hlsl(x,1): error", HLSL_NAME).is_none());
    }

    #[test]
    fn hlsl_errors_count_lines_in_the_authors_file() {
        let log = "C:\\x\\shader.hlsl(12,5-9): error X3004: undeclared identifier 'q'\nC:\\x\\shader.hlsl(13,1): error X3000: more\n";
        let e = hlsl_error(log, 10);
        assert_eq!((e.line, e.column), (Some(3), Some(5)));
        assert_eq!(e.message, "error X3004: undeclared identifier 'q' (and 1 more)");
        // in the generated lines: no author line
        let e = hlsl_error("shader.hlsl(4,1): error X1: oops", 10);
        assert_eq!(e.line, None);
        assert!(e.message.starts_with("in the generated prelude"), "{}", e.message);
        // not a position at all
        let e = hlsl_error("something else went wrong error", 10);
        assert_eq!(e.line, None);
        assert_eq!(hlsl_error("", 10).message, "the shader did not compile");
    }

    #[test]
    fn fxc_messages_for_generated_hlsl_carry_the_line_not_the_position() {
        let hlsl = "line one\nline two with the problem\n";
        let m = with_context("C:\\w\\bomb(2,3): error X3004: nope", "bomb", hlsl);
        assert_eq!(m, "error X3004: nope\n  generated HLSL: line two with the problem");
        // a line number outside the text
        assert_eq!(with_context("bomb(9,3): error X1: nope", "bomb", hlsl), "error X1: nope");
        assert_eq!(with_context("", "bomb", hlsl), "the shader did not compile");
    }
}
