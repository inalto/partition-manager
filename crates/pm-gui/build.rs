//! Embeds `partition-manager.manifest` (requireAdministrator, per-monitor DPI awareness) into
//! the `partition-manager` executable, in release builds only.
//!
//! Debug builds skip the manifest so `cargo run -p pm-gui` does not raise a UAC prompt during
//! development (the app then runs unelevated and shows "Not elevated: read-only", or use
//! `--disk-file`). Tests never get the manifest (it is linked into the binary only).

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=partition-manager.rc");
    println!("cargo:rerun-if-changed=partition-manager.manifest");
    println!("cargo:rerun-if-env-changed=PM_GUI_MANIFEST");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let release = env::var("PROFILE").map_or(false, |p| p == "release");
    // PM_GUI_MANIFEST=1 forces the manifest in a debug build, =0 skips it in release
    let wanted = match env::var("PM_GUI_MANIFEST").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => release,
    };
    if target_os != "windows" || !wanted {
        return;
    }

    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
        gnu_manifest();
    } else {
        embed_resource::compile_for("partition-manager.rc", ["partition-manager"], embed_resource::NONE);
    }
}

/// MinGW's gcc links its own `default-manifest.o` (an `asInvoker` manifest, resource 24/1)
/// into every executable via `%:if-exists(default-manifest.o%s)` in its specs. Linking ours
/// next to it makes ld fail to merge the two (".rsrc merge failure: multiple non-default
/// manifests") and leaves both in the file. Instead, compile our manifest into an object
/// named `default-manifest.o` and put its directory first on gcc's startfile search path
/// (`-B`), so ours replaces the default one.
///
/// `windres` is run from the directory of the configured linker (the portable MinGW is not on
/// PATH; windres also needs that gcc as its preprocessor), falling back to PATH.
fn gnu_manifest() {
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let dir = out.join("manifest");
    std::fs::create_dir_all(&dir).expect("create manifest dir");
    let obj = dir.join("default-manifest.o");

    let mut windres = PathBuf::from("windres");
    let mut path = env::var_os("PATH").unwrap_or_default();
    if let Ok(linker) = env::var("RUSTC_LINKER") {
        if let Some(bin) = Path::new(&linker).parent().filter(|d| d.join("windres.exe").exists()) {
            windres = bin.join("windres.exe");
            let mut dirs = vec![bin.to_path_buf()];
            dirs.extend(env::split_paths(&path));
            path = env::join_paths(dirs).expect("join PATH");
        }
    }
    let target = if env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86") { "pe-i386" } else { "pe-x86-64" };
    let status = Command::new(&windres)
        .env("PATH", &path)
        .args(["--input", "partition-manager.rc", "--output-format=coff", "--target", target, "--output"])
        .arg(&obj)
        .status()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", windres.display()));
    assert!(status.success(), "windres failed with {status}");

    // gcc concatenates the -B prefix and the file name: keep the trailing slash
    let prefix = format!("{}/", dir.display()).replace('\\', "/");
    println!("cargo:rustc-link-arg-bin=partition-manager=-B{prefix}");
}
