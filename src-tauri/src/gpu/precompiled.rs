//! The engine's built-in shaders, compiled by FXC in `build.rs` and embedded as bytecode.
//!
//! `build.rs` compiles every `shaders/*.hlsl` with the profiles, entry points and flags of
//! [`super::compile_shader`] and writes the table [`PRECOMPILED`]. A shader is found by the hash and length of its
//! source text (plus entry point and target), so the callers keep passing the HLSL they always did and a text that
//! is not built in (a test's, an example's) simply is not found and compiles at run time. Extension shaders never
//! come here: they call [`super::compile_shader`] directly.

/// One program of the table.
pub(crate) struct Precompiled {
    /// The file stem, for test messages.
    #[cfg_attr(not(test), allow(dead_code))]
    pub name: &'static str,
    /// [`source_hash`] of the source the bytecode was made from.
    pub hash: u64,
    pub len: usize,
    pub entry: &'static str,
    /// `vs_5_0` or `ps_5_0`.
    pub target: &'static str,
    pub code: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/precompiled_shaders.rs"));

/// FNV-1a 64; `build.rs` has the same function.
pub(crate) fn source_hash(text: &str) -> u64 {
    text.bytes()
        .fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3))
}

/// The bytecode built from exactly `source`, with `entry` and `target`.
pub(crate) fn find(source: &str, entry: &str, target: &str) -> Option<&'static Precompiled> {
    #[cfg(test)]
    if tracking::forced_runtime() {
        return None;
    }
    let hash = source_hash(source);
    PRECOMPILED
        .iter()
        .find(|p| p.hash == hash && p.len == source.len() && p.entry == entry && p.target == target)
}

/// A shader the table did not have was compiled at run time. Counted per thread in tests, which assert that the
/// built-ins never get here.
#[cfg(test)]
pub(crate) mod tracking {
    use std::cell::{Cell, RefCell};

    thread_local! {
        static RUNTIME_COMPILES: Cell<usize> = const { Cell::new(0) };
        static FORCE_RUNTIME: Cell<bool> = const { Cell::new(false) };
        static SERVED: RefCell<Vec<(String, String, String)>> = const { RefCell::new(Vec::new()) };
    }

    /// The table served this program on this thread.
    pub fn note_served(source: &str, entry: &str, target: &str) {
        SERVED.with(|s| s.borrow_mut().push((source.to_string(), entry.to_string(), target.to_string())));
    }

    /// Every program the table served on this thread, as (source, entry, target).
    pub fn served() -> Vec<(String, String, String)> {
        SERVED.with(|s| s.borrow().clone())
    }

    pub fn note_runtime_compile() {
        RUNTIME_COMPILES.with(|c| c.set(c.get() + 1));
    }

    /// Run-time compiles on this thread since it started.
    pub fn runtime_compiles() -> usize {
        RUNTIME_COMPILES.with(Cell::get)
    }

    /// Make the table invisible on this thread: the old behaviour, to measure it.
    pub fn force_runtime(on: bool) {
        FORCE_RUNTIME.with(|c| c.set(on));
    }

    pub fn forced_runtime() -> bool {
        FORCE_RUNTIME.with(Cell::get)
    }
}
