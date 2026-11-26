//! Where the engines live. Nothing is embedded in the launcher and nothing is unpacked or
//! downloaded at run time: the release ZIP puts a `runtime` folder beside ScarlettPort.exe
//! (ScarlettPort-Runtime.exe, the ScarlettPort runtime, and its `D3D12` folder for the World engine; xcl.exe, dxcompiler.dll and
//! dxil.dll for the Classic engine when present), and the launcher runs them from there.
//! `SCARLETT_RUNTIME_DIR` points at another folder (for development builds).
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Settings and the default cache location (data only, never executables).
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("ScarlettPort")
}

/// The `runtime` folder beside the launcher.
pub fn runtime_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("SCARLETT_RUNTIME_DIR") {
        return PathBuf::from(d);
    }
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.join("runtime")))
        .unwrap_or_else(|| PathBuf::from("runtime"))
}

pub fn xcl_exe() -> PathBuf {
    runtime_dir().join("xcl.exe")
}

/// The release's name for the ScarlettPort runtime (scarlett.exe).
pub const RUNTIME_EXE: &str = "ScarlettPort-Runtime.exe";

/// The World engine: `ScarlettPort-Runtime.exe`, else a development build's `scarlett.exe`.
pub fn scarlett_exe() -> PathBuf {
    let dir = runtime_dir();
    let p = dir.join(RUNTIME_EXE);
    if p.is_file() {
        return p;
    }
    dir.join("scarlett.exe")
}

/// The former spelling of a `SCARLETT_*` switch, which the runtime still reads when the
/// `SCARLETT_*` one is unset.
fn legacy_name(key: &str) -> Option<String> {
    key.strip_prefix("SCARLETT_").map(|rest| format!("VICTORY_{rest}")) // rename:keep
}

/// Sets a runtime switch on `cmd` under its `SCARLETT_*` name.
pub fn set_switch(cmd: &mut std::process::Command, key: &str, value: &str) {
    cmd.env(key, value);
}

/// Clears a runtime switch (also under its former spelling, which the runtime would read instead).
pub fn clear_switch(cmd: &mut std::process::Command, key: &str) {
    cmd.env_remove(key);
    if let Some(old) = legacy_name(key) {
        cmd.env_remove(old);
    }
}

/// The data folder's former name under `%LOCALAPPDATA%`.
const LEGACY_DATA_DIR: &str = "victory"; // rename:keep

/// On first start: what the former data folder holds (saves, caches, settings) and
/// `%LOCALAPPDATA%\ScarlettPort` does not is moved over (copied when it cannot be moved);
/// the launcher's own files in `%LOCALAPPDATA%\ScarlettPort` are left as they are.
pub fn migrate_legacy_data() {
    let Some(base) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) else { return };
    let root = data_dir();
    let old_toml = root.join(format!("{LEGACY_DATA_DIR}.toml"));
    if old_toml.is_file() && !root.join("scarlett.toml").exists() {
        let _ = std::fs::rename(&old_toml, root.join("scarlett.toml"));
    }
    let old = base.join(LEGACY_DATA_DIR);
    let Ok(entries) = std::fs::read_dir(&old) else { return };
    if std::fs::create_dir_all(&root).is_err() {
        return;
    }
    for entry in entries.flatten() {
        let target = root.join(entry.file_name());
        if target.exists() || std::fs::rename(entry.path(), &target).is_ok() {
            continue;
        }
        let _ = copy_tree(&entry.path(), &target);
    }
    let _ = std::fs::remove_dir(&old);
}

/// A shader cache folder kept under its former name inside `cache` takes the new one.
pub fn migrate_legacy_cache(cache: &Path, new: &Path) {
    let old = cache.join(LEGACY_DATA_DIR);
    if old.is_dir() && !new.exists() {
        let _ = std::fs::rename(&old, new);
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if !from.is_dir() {
        return std::fs::copy(from, to).map(|_| ());
    }
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        copy_tree(&entry.path(), &to.join(entry.file_name()))?;
    }
    Ok(())
}

#[cfg(test)]
mod switch_tests {
    #[test]
    fn legacy_names() {
        assert_eq!(super::legacy_name("SCARLETT_LOOK").as_deref(), Some("VICTORY_LOOK")); // rename:keep
        assert_eq!(super::legacy_name("XCL_DXC_DIR"), None);
    }
}

/// What was found in the runtime folder: (folder, World engine hash, Classic engine hash).
/// A hash is the SHA-256 of the engine's executable; it is what "shaders built for" refers to.
pub struct Found {
    pub dir: PathBuf,
    pub world: Option<String>,
    pub classic: Option<String>,
}

pub fn file_hash(p: &Path) -> Option<String> {
    let bytes = std::fs::read(p).ok().filter(|b| !b.is_empty())?;
    Some(Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect())
}

/// Looks for the engines beside the launcher.
pub fn locate() -> Result<Found, String> {
    let dir = runtime_dir();
    let found = Found { world: file_hash(&scarlett_exe()), classic: file_hash(&xcl_exe()), dir: dir.clone() };
    if found.world.is_none() && found.classic.is_none() {
        return Err(format!(
            "No engine found in {}. Extract the whole release ZIP and keep the runtime folder beside ScarlettPort.exe.",
            dir.display()
        ));
    }
    Ok(found)
}
