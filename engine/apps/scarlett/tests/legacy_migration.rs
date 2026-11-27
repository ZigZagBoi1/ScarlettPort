#![cfg(windows)]

use std::fs;
use std::path::Path;
use std::process::Command;

fn scarlett(profile: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_scarlett"));
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if upper.starts_with("SCARLETT_") || upper.starts_with("VICTORY_") {
            command.env_remove(name);
        }
    }
    command.current_dir(profile).env("LOCALAPPDATA", profile);
    command
}

#[test]
fn only_a_store_the_cli_opens_is_migrated() {
    let temp = tempfile::tempdir().unwrap();
    let profile = temp.path();
    let legacy = profile.join("victory");
    let current = profile.join("ScarlettPort");
    for (store, bytes) in [("cache", b"cached bytes".as_slice()), ("data", b"save bytes"), ("experiments", b"notes")] {
        let path = legacy.join(store);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("sentinel"), bytes).unwrap();
    }
    let input = profile.join("input.bin");
    fs::write(&input, b"not a shader or executable").unwrap();
    for argv in [&["--help"][..], &["shader-info", "input.bin"], &["census"]] {
        let output = scarlett(profile).args(argv).env("SCARLETT_TEST_TITLES", &input).output().unwrap();
        assert!(output.status.success(), "{argv:?}: {output:?}");
        assert!(!current.exists(), "{argv:?} touched the runtime store");
    }
    assert_eq!(fs::read(legacy.join("cache/sentinel")).unwrap(), b"cached bytes");
    assert_eq!(fs::read(legacy.join("data/sentinel")).unwrap(), b"save bytes");
    assert_eq!(fs::read(legacy.join("experiments/sentinel")).unwrap(), b"notes");

    // Precompile opens the cache, but no GPU shader work is needed for this input.
    let output = scarlett(profile).args(["shaders", "input.bin"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fs::read(current.join("cache/sentinel")).unwrap(), b"cached bytes");
    assert!(!legacy.join("cache").exists());
    assert!(!current.join("data").exists());
    assert_eq!(fs::read(legacy.join("data/sentinel")).unwrap(), b"save bytes");
    assert!(!current.join("experiments").exists());
    assert_eq!(fs::read(legacy.join("experiments/sentinel")).unwrap(), b"notes");
}

#[test]
fn legacy_store_junction_into_a_source_tree_is_not_migrated() {
    let profile = tempfile::tempdir().unwrap();
    let root = profile.path();
    let source_tree = root.join("source");
    let legacy_cache = root.join("victory").join("cache");
    fs::create_dir(&source_tree).unwrap();
    fs::write(source_tree.join(".scarlett-root"), b"").unwrap();
    fs::create_dir(legacy_cache.parent().unwrap()).unwrap();
    let junction = Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/j"])
        .arg(&legacy_cache)
        .arg(&source_tree)
        .output()
        .unwrap();
    assert!(junction.status.success(), "{junction:?}");
    fs::write(root.join("input.bin"), b"not a shader").unwrap();
    let output = scarlett(root).args(["shaders", "input.bin"]).output().unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(legacy_cache.is_dir(), "the guarded legacy link must stay in place");
    assert!(!root.join("ScarlettPort/cache").exists());
    assert!(!source_tree.join("shaders").exists());
}

#[test]
fn readable_legacy_cache_is_copied_when_rename_is_blocked() {
    use std::os::windows::fs::OpenOptionsExt;

    let profile = tempfile::tempdir().unwrap();
    let root = profile.path();
    let legacy = root.join("victory/cache");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(legacy.join("sentinel"), b"old cache").unwrap();
    fs::write(root.join("input.bin"), b"not a shader").unwrap();
    // FILE_FLAG_BACKUP_SEMANTICS permits opening a directory; share read
    // and write but not delete, preventing a rename while allowing a copy.
    let opened = fs::OpenOptions::new()
        .read(true)
        .share_mode(0x3)
        .custom_flags(0x02000000)
        .open(&legacy)
        .unwrap();
    let output = scarlett(root).args(["shaders", "input.bin"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fs::read(legacy.join("sentinel")).unwrap(), b"old cache");
    assert_eq!(fs::read(root.join("ScarlettPort/cache/sentinel")).unwrap(), b"old cache");
    drop(opened);
}
