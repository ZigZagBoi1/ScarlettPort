//! Fingerprint of the translator's own source, for the shader cache key
//! (`scarlett-gpu`'s `shader_cache`): a rebuilt executable whose translator did
//! not change keeps its cached translations. Also lists every `SCARLETT_*`
//! switch the translator reads, whose values join the key at run time.
//! Reads only this crate's `src` folder; writes nothing but cargo directives,
//! and the Agility SDK's two DLLs beside the unit-test executable (below).

use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// FNV-1a, 64 bits, two lanes with different offsets: a mismatch only costs
/// a cache miss, so this need not be cryptographic.
fn fnv(state: &mut [u64; 2], bytes: &[u8]) {
    for &b in bytes {
        for (i, s) in state.iter_mut().enumerate() {
            *s ^= u64::from(b) ^ (i as u64 * 0x9e);
            *s = s.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

/// The unit tests opt into the Agility SDK (`bench_tests`)
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
    // that does not): a folder put there later takes `cargo clean -p scarlett-shader`.
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
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    let mut list = Vec::new();
    files(&src, &mut list);
    list.sort();
    let mut state = [0xcbf2_9ce4_8422_2325u64, 0x8422_2325_cbf2_9ce4u64];
    let mut switches = std::collections::BTreeSet::new();
    for p in &list {
        let rel = p.strip_prefix(&src).unwrap_or(p).to_string_lossy().replace('\\', "/");
        let text = std::fs::read(p).unwrap_or_default();
        fnv(&mut state, rel.as_bytes());
        fnv(&mut state, &(text.len() as u64).to_le_bytes());
        fnv(&mut state, &text);
        let s = String::from_utf8_lossy(&text);
        let mut rest = &s[..];
        while let Some(at) = rest.find("\"SCARLETT_") {
            let tail = &rest[at + 1..];
            let end = tail
                .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                .unwrap_or(tail.len());
            if tail[end..].starts_with('"') {
                switches.insert(tail[..end].to_string());
            }
            rest = &tail[end..];
        }
    }
    println!("cargo:rustc-env=SCARLETT_SHADER_SOURCE_HASH={:016x}{:016x}", state[0], state[1]);
    let joined: Vec<_> = switches.into_iter().collect();
    println!("cargo:rustc-env=SCARLETT_SHADER_SWITCHES={}", joined.join(","));
}
