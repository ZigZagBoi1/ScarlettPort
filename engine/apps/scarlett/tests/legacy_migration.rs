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
