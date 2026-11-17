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

fn main() {
    stage_vcam_dll();
    tauri_build::build()
}
