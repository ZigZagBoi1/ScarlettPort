//! Where the user's files live.
//!
//! Resolution order for each value: environment variable, then the config
//! file (`SCARLETT_CONFIG`, else `./scarlett.toml`), then the default.
//! See `scarlett.example.toml` at the repository root.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::policy::ensure_outside_repo;

#[derive(Debug, Default, Clone)]
pub struct Config {
    /// The user's dump of the title.
    pub game_dir: Option<PathBuf>,
    /// Root for all derived data. Always outside the repository.
    pub cache_dir: PathBuf,
    /// What titles write and must keep (persistent local storage). Never
    /// inside the cache, which may be deleted at any time.
    pub data_dir: PathBuf,
    /// Other titles used to exercise platform-level code.
    pub test_titles: Vec<PathBuf>,
    /// The config file that was read, if any.
    pub source: Option<PathBuf>,
}

/// Other sections are ignored: the ScarlettPort launcher writes `[display]`, `[game]` and
/// `[pace]` for older runtimes into the same file. Keys inside `[paths]` stay checked.
#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    paths: FilePaths,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePaths {
    game_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    #[serde(default)]
    test_titles: Vec<PathBuf>,
}

impl Config {
    /// Loads configuration from the environment and the config file.
    pub fn load() -> Result<Self> {
        let explicit = explicit_config_path(|k| std::env::var_os(k));
        let path = explicit
            .clone()
            .unwrap_or_else(|| PathBuf::from("scarlett.toml"));
        let file = if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            Some((path, text))
        } else if let Some(p) = explicit {
            anyhow::bail!(
                "SCARLETT_CONFIG points at {}, which does not exist",
                p.display()
            );
        } else {
            None
        };
        let (source, parsed) = match file {
            Some((path, text)) => {
                let parsed: FileConfig =
                    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                (Some(path), parsed)
            }
            None => (None, FileConfig::default()),
        };
        Self::resolve(parsed.paths, source, |k| std::env::var_os(k))
    }

    fn resolve(
        file: FilePaths,
        source: Option<PathBuf>,
        env: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Result<Self> {
        let env_path = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let test_titles = match env("SCARLETT_TEST_TITLES") {
            Some(v) if !v.is_empty() => std::env::split_paths(&v)
                .filter(|p| !p.as_os_str().is_empty())
                .collect(),
            _ => file.test_titles,
        };
        let cache_dir = env_path("SCARLETT_CACHE_DIR")
            .or(file.cache_dir)
            .map_or_else(|| default_dir("cache"), Ok)?;
        let data_dir = env_path("SCARLETT_DATA_DIR")
            .or(file.data_dir)
            .map_or_else(|| default_dir("data"), Ok)?;
        Ok(Config {
            game_dir: env_path("SCARLETT_GAME_DIR").or(file.game_dir),
            cache_dir,
            data_dir,
            test_titles,
            source,
        })
    }

    /// The game directory, or a helpful error if it is not configured.
    pub fn require_game_dir(&self) -> Result<&Path> {
        self.game_dir.as_deref().context(
            "no game directory configured: set SCARLETT_GAME_DIR or [paths].game_dir in \
             scarlett.toml (see scarlett.example.toml)",
        )
    }

    /// Returns `<cache>/<sub>`, creating it, after checking the asset policy.
    pub fn cache_subdir(&self, sub: &str) -> Result<PathBuf> {
        subdir(&self.cache_dir, "cache", sub)
    }

    /// Returns `<data>/<sub>`, creating it, after checking the asset policy.
    pub fn data_subdir(&self, sub: &str) -> Result<PathBuf> {
        subdir(&self.data_dir, "data", sub)
    }
}

/// The config file explicitly named by the environment, if any. An empty
/// `SCARLETT_CONFIG` means unset, like the other path variables.
fn explicit_config_path(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    env("SCARLETT_CONFIG")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn subdir(root: &Path, store: &str, sub: &str) -> Result<PathBuf> {
    let dir = root.join(sub);
    ensure_outside_repo(&dir)?;
    if migrate_legacy_store(root, store)? {
        // Migration can change what the destination resolves to.
        ensure_outside_repo(&dir)?;
    }
    std::fs::create_dir_all(&dir).with_context(|| {
        format!(
            "cannot create the {store} folder {}: check that its drive is connected and              writable, or pick another {store} folder",
            dir.display()
        )
    })?;
    Ok(dir)
}

/// `%LOCALAPPDATA%/ScarlettPort/<name>` (elsewhere the XDG or home cache).
fn default_dir(name: &str) -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_CACHE_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .context(
            "cannot determine a user directory; set SCARLETT_CACHE_DIR and SCARLETT_DATA_DIR",
        )?;
    Ok(base.join("ScarlettPort").join(name))
}

/// The data folder's former name under `%LOCALAPPDATA%`.
const LEGACY_DATA_DIR: &str = "victory"; // rename:keep

/// Migrate only the default store a caller is about to use. Resolving paths
/// alone must not touch another runtime's files.
fn migrate_legacy_store(root: &Path, store: &str) -> Result<bool> {
    let Some(base) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) else {
        return Ok(false);
    };
    let destination = base.join("ScarlettPort");
    if root != destination.join(store) || root.exists() {
        return Ok(false);
    }
    // Different subdirectories can be opened concurrently at startup.
    static MIGRATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = MIGRATION.lock().unwrap_or_else(|e| e.into_inner());
    let old = base.join(LEGACY_DATA_DIR).join(store);
    if root.exists() {
        return Ok(true);
    }
    if !old.is_dir() {
        return Ok(false);
    }
    ensure_outside_repo(&old)?;
    std::fs::create_dir_all(&destination)
        .with_context(|| format!("creating {}", destination.display()))?;
    match std::fs::rename(&old, root) {
        Ok(()) => Ok(()),
        // Another process may have migrated this store while we were opening it.
        Err(_) if root.is_dir() => Ok(()),
        Err(error) => copy_legacy_store(&old, root).with_context(|| {
            format!(
                "renaming {} to {} failed: {error}",
                old.display(),
                root.display()
            )
        }),
    }?;
    Ok(true)
}

/// Stage a fallback copy beside the final directory so a partial copy never
/// looks like an initialized store. The original remains intact.
fn copy_legacy_store(from: &Path, to: &Path) -> Result<()> {
    let parent = to
        .parent()
        .context("legacy store destination has no parent")?;
    let stage =
        tempfile::tempdir_in(parent).with_context(|| format!("staging {}", to.display()))?;
    copy_tree(from, stage.path())
        .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    match std::fs::rename(stage.path(), to) {
        Ok(()) => {
            let _ = stage.keep();
            Ok(())
        }
        Err(_) if to.is_dir() => Ok(()),
        Err(error) => Err(error).with_context(|| format!("publishing {}", to.display())),
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn subdir_creates_a_missing_cache_folder() {
        // #252: a cache folder that does not exist yet, two levels deep.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("not-yet").join("scarlett");
        let dir = subdir(&root, "cache", "shaders").unwrap();
        assert!(dir.is_dir());
        assert_eq!(dir, root.join("shaders"));
    }

    #[cfg(windows)]
    #[test]
    fn legacy_migration_copy_failure_does_not_publish_an_empty_store() {
        use std::os::windows::fs::OpenOptionsExt;

        let profile = tempfile::tempdir().unwrap();
        let legacy = profile.path().join("legacy");
        let dest = profile.path().join("current");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("open"), b"profile data").unwrap();
        std::fs::write(legacy.join("locked"), b"save contents").unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(legacy.join("locked"))
            .unwrap();

        let error = copy_legacy_store(&legacy, &dest).unwrap_err();
        assert!(
            error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.raw_os_error() == Some(32))
        );
        assert!(
            !dest.exists(),
            "a failed copy must not be treated as an initialized store"
        );
        assert_eq!(std::fs::read(legacy.join("open")).unwrap(), b"profile data");
        drop(lock);

        copy_legacy_store(&legacy, &dest).unwrap();
        for entry in ["open", "locked"] {
            assert_eq!(
                std::fs::read(dest.join(entry)).unwrap(),
                std::fs::read(legacy.join(entry)).unwrap()
            );
        }
    }

    #[test]
    fn environment_overrides_file() {
        let file = FilePaths {
            game_dir: Some("from-file".into()),
            cache_dir: Some("cache-from-file".into()),
            ..Default::default()
        };
        let env = |k: &str| (k == "SCARLETT_GAME_DIR").then(|| OsString::from("from-env"));
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.game_dir.unwrap(), PathBuf::from("from-env"));
        assert_eq!(cfg.cache_dir, PathBuf::from("cache-from-file"));
    }

    #[test]
    fn empty_env_values_are_ignored() {
        let file = FilePaths {
            game_dir: Some("from-file".into()),
            ..Default::default()
        };
        let env = |k: &str| (k == "SCARLETT_GAME_DIR").then(OsString::new);
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.game_dir.unwrap(), PathBuf::from("from-file"));
    }

    #[test]
    fn empty_test_titles_env_is_ignored() {
        let file = FilePaths {
            test_titles: vec!["from-file".into()],
            ..Default::default()
        };
        let env = |k: &str| (k == "SCARLETT_TEST_TITLES").then(OsString::new);
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.test_titles, vec![PathBuf::from("from-file")]);
    }

    #[test]
    fn test_titles_env_splits_paths() {
        let value = std::env::join_paths(["first", "second"]).unwrap();
        let file = FilePaths::default();
        let env = |k: &str| (k == "SCARLETT_TEST_TITLES").then(|| value.clone());
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(
            cfg.test_titles,
            vec![PathBuf::from("first"), PathBuf::from("second")]
        );
    }

    #[test]
    fn empty_config_env_is_ignored() {
        let empty = |k: &str| (k == "SCARLETT_CONFIG").then(OsString::new);
        assert_eq!(explicit_config_path(empty), None);
        let set = |k: &str| (k == "SCARLETT_CONFIG").then(|| OsString::from("custom.toml"));
        assert_eq!(
            explicit_config_path(set),
            Some(PathBuf::from("custom.toml"))
        );
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../scarlett.example.toml");
        let parsed: FileConfig = toml::from_str(text).unwrap();
        assert!(parsed.paths.game_dir.is_some());
    }

    #[test]
    fn launcher_sections_are_ignored_and_paths_stay_checked() {
        let text = "[display]\nhdr = false\n\n[game]\nrumble = true\n\n[pace]\npace = false\n\n[paths]\ngame_dir = \"D:/game\"\n";
        let parsed: FileConfig = toml::from_str(text).unwrap();
        assert_eq!(parsed.paths.game_dir, Some(PathBuf::from("D:/game")));
        assert!(toml::from_str::<FileConfig>("[paths]\ngame_folder = \"D:/game\"\n").is_err());
    }

    #[test]
    fn cache_inside_repo_is_refused() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache");
        let cfg = Config {
            cache_dir: inside,
            ..Default::default()
        };
        assert!(cfg.cache_subdir("x").is_err());
    }

    #[test]
    fn data_inside_repo_is_refused() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
        let cfg = Config {
            data_dir: inside,
            ..Default::default()
        };
        assert!(cfg.data_subdir("x").is_err());
    }
}
