use std::path::{Path, PathBuf};

/// The virtual camera DLL is a bundled resource (`tauri.conf.json`), and `tauri-build` refuses a resource that does
/// not exist. So the DLL that `npm run vcam:build` produced is copied to a staging path first, and when it has not been
/// built yet a placeholder takes its place: `cargo check` and `cargo test` then work on a fresh checkout, and the app
/// refuses to install a placeholder (`vcam_install::looks_like_dll`).
///
/// The DLL is never built from here: a nested cargo call from a build script can deadlock on the target-dir lock.
/// `npm run tauri dev` and `npm run tauri build` build it first (`beforeDevCommand`, `beforeBuildCommand`).
const DLL: &str = "better_cam_vcam.dll";

fn stage_vcam_dll() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let target = manifest.parent().expect("workspace root").join("target");
    let built = target.join("release").join(DLL);
    let stage = target.join("vcam-stage");
    let staged = stage.join(DLL);
    println!("cargo:rerun-if-changed={}", built.display());
    // Written once per change of the DLL; a missing DLL makes cargo rerun this script, which is cheap.
    std::fs::create_dir_all(&stage).expect("create target/vcam-stage");
    if built.is_file() {
        copy_if_different(&built, &staged);
    } else {
        std::fs::write(
            &staged,
            b"placeholder: better_cam_vcam.dll was not built when this was compiled (run `npm run vcam:build`)",
        )
        .expect("write the DLL placeholder");
        if std::env::var("PROFILE").as_deref() == Ok("release") {
            println!(
                "cargo:warning={DLL} was not built: this release build carries a placeholder and cannot install the virtual camera. Run `npm run vcam:build` first (`npm run tauri build` does)."
            );
        }
    }
}

fn copy_if_different(from: &Path, to: &Path) {
    let same = match (std::fs::read(from), std::fs::read(to)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same {
        std::fs::copy(from, to).expect("stage the virtual camera DLL");
    }
}

/// The engine's own HLSL (`src/gpu/shaders`) is compiled here with FXC (`D3DCompile`, the same call, profiles and
/// flags as `gpu::compile_shader`) and the bytecode is embedded (`src/gpu/precompiled.rs`), so neither engine creation
/// nor the rebuild after a GPU device loss compiles anything. A compile error fails the build with FXC's message.
///
/// Which source each program is made of mirrors the run-time callers: the convert shaders get `convert_common.hlsl`
/// in front, the scale and transform shaders `geometry_common.hlsl` (both followed by `"\n"`), every other file is a
/// pixel shader by itself with the entry point `main`. The run time finds a blob by a hash of the source text, so a
/// source that differs by a byte (a test's, an extension's) is not found and compiles at run time as before.
mod shaders {
    use std::ffi::{c_void, CString};
    use std::fmt::Write as _;
    use std::path::{Path, PathBuf};

    // `d3dcompiler_47.dll` ships with Windows; raw-dylib needs no import library from the SDK.
    #[link(name = "d3dcompiler_47", kind = "raw-dylib")]
    extern "system" {
        fn D3DCompile(
            src: *const c_void,
            src_len: usize,
            source_name: *const u8,
            defines: *const c_void,
            include: *mut c_void,
            entry: *const u8,
            target: *const u8,
            flags1: u32,
            flags2: u32,
            code: *mut *mut Blob,
            errors: *mut *mut Blob,
        ) -> i32;
    }

    /// `ID3DBlob`: `IUnknown` and the two buffer methods.
    #[repr(C)]
    struct BlobVtbl {
        query_interface: usize,
        add_ref: usize,
        release: unsafe extern "system" fn(*mut Blob) -> u32,
        buffer_pointer: unsafe extern "system" fn(*mut Blob) -> *mut c_void,
        buffer_size: unsafe extern "system" fn(*mut Blob) -> usize,
    }
    #[repr(C)]
    struct Blob {
        vtbl: *const BlobVtbl,
    }

    /// Takes the blob's bytes and releases it.
    unsafe fn take(blob: *mut Blob) -> Vec<u8> {
        if blob.is_null() {
            return Vec::new();
        }
        let v = &*(*blob).vtbl;
        let bytes = std::slice::from_raw_parts((v.buffer_pointer)(blob) as *const u8, (v.buffer_size)(blob)).to_vec();
        (v.release)(blob);
        bytes
    }

    /// `D3DCOMPILE_ENABLE_STRICTNESS | D3DCOMPILE_OPTIMIZATION_LEVEL3`, as in `gpu::compile_shader`.
    const FLAGS: u32 = (1 << 11) | (1 << 15);

    fn compile(name: &str, source: &str, entry: &str, target: &str) -> Vec<u8> {
        let c_name = CString::new(name).expect("shader name");
        let c_entry = CString::new(entry).expect("entry point");
        let c_target = CString::new(target).expect("target");
        let (mut code, mut errors) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: the source and the C strings outlive the call; the out pointers are locals.
        let hr = unsafe {
            D3DCompile(
                source.as_ptr() as *const c_void,
                source.len(),
                c_name.as_ptr() as *const u8,
                std::ptr::null(),
                std::ptr::null_mut(),
                c_entry.as_ptr() as *const u8,
                c_target.as_ptr() as *const u8,
                FLAGS,
                0,
                &mut code,
                &mut errors,
            )
        };
        // SAFETY: both pointers come from the call above and are taken once.
        let (code, errors) = unsafe { (take(code), take(errors)) };
        if hr < 0 || code.is_empty() {
            panic!(
                "FXC failed on shader {name} ({entry}, {target}), HRESULT 0x{:08X}:\n{}",
                hr as u32,
                String::from_utf8_lossy(&errors).trim()
            );
        }
        code
    }

    /// FNV-1a 64; `gpu/precompiled.rs` has the same function.
    fn hash(text: &str) -> u64 {
        text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3))
    }

    pub fn build(dir: &Path, out: &Path) {
        println!("cargo:rerun-if-changed={}", dir.display());
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("read src/gpu/shaders")
            .map(|e| e.expect("shader entry").path())
            .filter(|p| p.extension().is_some_and(|e| e == "hlsl"))
            .collect();
        files.sort();
        let read = |stem: &str| {
            let path = dir.join(format!("{stem}.hlsl"));
            println!("cargo:rerun-if-changed={}", path.display());
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        };
        let convert_common = read("convert_common");
        let geometry_common = read("geometry_common");

        let mut table = String::new();
        for path in files {
            let stem = path.file_stem().and_then(|s| s.to_str()).expect("shader file name").to_string();
            if stem.ends_with("_common") {
                continue;
            }
            let body = read(&stem);
            let source = if stem.starts_with("convert_") {
                format!("{convert_common}\n{body}")
            } else if stem.starts_with("scale_") || stem == "transform" {
                format!("{geometry_common}\n{body}")
            } else {
                body
            };
            let programs: &[(&str, &str)] = match stem.as_str() {
                "fullscreen_vs" => &[("main", "vs_5_0")],
                "encode_nv12" => &[("PSLuma", "ps_5_0"), ("PSChroma", "ps_5_0")],
                _ => &[("main", "ps_5_0")],
            };
            for &(entry, target) in programs {
                let blob = format!("{stem}.{entry}.bin");
                std::fs::write(out.join(&blob), compile(&stem, &source, entry, target)).expect("write shader bytecode");
                writeln!(
                    table,
                    "    Precompiled {{ name: {stem:?}, hash: 0x{:016x}, len: {}, entry: {entry:?}, target: {target:?}, code: include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{blob}\")) }},",
                    hash(&source),
                    source.len()
                )
                .unwrap();
            }
        }
        std::fs::write(
            out.join("precompiled_shaders.rs"),
            format!("pub(crate) static PRECOMPILED: &[Precompiled] = &[\n{table}];\n"),
        )
        .expect("write the shader table");
    }
}

fn main() {
    stage_vcam_dll();
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    shaders::build(&manifest.join("src").join("gpu").join("shaders"), &out);
    tauri_build::build()
}
