//! Fingerprints the shader translator, so that cached translations
//! (`src/shader_cache.rs`) last until the translator itself changes and not
//! only until the next build of anything.

use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// The unit tests opt into the Agility SDK (`agility_opt_in` in `lib.rs`)
/// only when its `D3D12` folder is beside the test executable (in `deps`):
/// with the opt-in and no folder, D3D12 refuses to start at all. A folder
/// that `setup-scarlett.ps1` put beside `scarlett.exe` is copied there.
fn agility_for_tests() {
    println!("cargo:rustc-check-cfg=cfg(scarlett_agility_tests)");
    let Some(out) = std::env::var_os("OUT_DIR").map(PathBuf::from) else {
        return;
    };
    // <target>/<profile>/build/<crate>-<hash>/out
    let Some(profile) = out.ancestors().nth(3) else {
        return;
    };
    let (beside, deps) = (profile.join("D3D12"), profile.join("deps").join("D3D12"));
    // Only paths that exist (cargo runs this again on every build for one
    // that does not): a folder put there later takes `cargo clean -p scarlett-gpu`.
    for dll in [beside.join("D3D12Core.dll"), deps.join("D3D12Core.dll")] {
        if dll.exists() {
            println!("cargo:rerun-if-changed={}", dll.display());
        }
    }
    if !deps.join("D3D12Core.dll").exists() && beside.join("D3D12Core.dll").exists() {
        let _ = std::fs::create_dir_all(&deps);
        for dll in ["D3D12Core.dll", "d3d12SDKLayers.dll"] {
            let _ = std::fs::copy(beside.join(dll), deps.join(dll));
        }
    }
    if deps.join("D3D12Core.dll").exists() {
        println!("cargo:rustc-cfg=scarlett_agility_tests");
    }
}

fn main() {
    agility_for_tests();
    let here = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let crates = here.join("..");
    let root = crates.join("..");
    // The translator, what it is built from, and the form entries are kept in.
    let mut all = vec![
        crates.join("scarlett-shader/Cargo.toml"),
        here.join("src/shader_cache.rs"),
        root.join("Cargo.lock"),
        root.join("rust-toolchain.toml"),
    ];
    let source = crates.join("scarlett-shader/src");
    println!("cargo:rerun-if-changed={}", source.display());
    files(&source, &mut all);
    all.sort();
    let mut hash = blake3::Hasher::new();
    for path in &all {
        println!("cargo:rerun-if-changed={}", path.display());
        // Names and line endings as they are in every checkout.
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let bytes: Vec<u8> = std::fs::read(path).unwrap_or_default().into_iter().filter(|&b| b != 13).collect();
        hash.update(name.as_bytes());
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-env=SCARLETT_TRANSLATOR={}", hash.finalize().to_hex());
}
