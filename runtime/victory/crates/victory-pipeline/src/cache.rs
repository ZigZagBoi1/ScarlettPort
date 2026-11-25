use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::Converter;

/// Identifies one conversion result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey(blake3::Hash);

impl CacheKey {
    pub fn new(converter: &dyn Converter, input: &[u8]) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(converter.id().as_bytes());
        h.update(&[0]);
        h.update(&converter.version().to_le_bytes());
        h.update(input);
        CacheKey(h.finalize())
    }

    pub fn to_hex(&self) -> String {
        self.0.to_hex().to_string()
    }
}

/// Content-addressed store under the user's cache directory.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// Opens (creating) a cache at `root`. Refuses locations inside the repo.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        victory_core::ensure_outside_repo(&root)?;
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(Cache { root })
    }

    pub fn path_for(&self, key: &CacheKey) -> PathBuf {
        let hex = key.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    /// Returns the cached output for `input`, converting it first if needed.
    pub fn get_or_convert(&self, converter: &dyn Converter, input: &[u8]) -> Result<PathBuf> {
        let key = CacheKey::new(converter, input);
        let path = self.path_for(&key);
        if path.is_file() {
            tracing::trace!(converter = converter.id(), key = %key.to_hex(), "cache hit");
            return Ok(path);
        }
        let output = converter
            .convert(input)
            .with_context(|| format!("converter {} v{}", converter.id(), converter.version()))?;
        publish(&path, &output)?;
        Ok(path)
    }

    /// Reclaims partial writes from terminated processes in this key's cache directory.
    /// Called before reading a large entry so a cache hit also cleans up interrupted writes.
    #[cfg(windows)]
    pub fn reclaim_staging_files(&self, key: &CacheKey) {
        let path = self.path_for(key);
        let Ok(entries) = std::fs::read_dir(path.parent().expect("hashed path has a parent")) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some((pid, seq)) = name.to_str().and_then(|s| s.strip_prefix(".tmp-")).and_then(|s| s.split_once('-')) else {
                continue;
            };
            let (Ok(owner), Ok(counter)) = (pid.parse::<u32>(), seq.parse::<u64>()) else { continue };
            if owner != 0 && name == format!(".tmp-{owner}-{counter}").as_str()
                && entry.file_type().is_ok_and(|t| t.is_file()) && writer_exited(owner)
            {
                remove_abandoned_stage(&entry.path(), owner);
            }
        }
    }

    /// The stored bytes of `key`, if there are any.
    pub fn read(&self, key: &CacheKey) -> Option<Vec<u8>> {
        std::fs::read(self.path_for(key)).ok()
    }

    /// Stores `bytes` as `key`'s output, replacing what was there (written aside and
    /// renamed over it: a reader sees the old bytes or the new, never a mix).
    pub fn write(&self, key: &CacheKey, bytes: &[u8]) -> Result<PathBuf> {
        let path = self.path_for(key);
        publish(&path, bytes)?;
        Ok(path)
    }
}

/// Writes `output` to `path` through a staging file in its folder.
fn publish(path: &Path, output: &[u8]) -> Result<()> {
    let dir = path.parent().expect("hashed path has a parent");
    std::fs::create_dir_all(dir)?;
    // Write-then-rename so readers never see partial output.
    let tmp = tempfile_in(dir);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    let written = file.write_all(output);
    drop(file);
    let published = written.and_then(|()| std::fs::rename(&tmp, path));
    if published.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(published?)
}

fn tempfile_in(dir: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".tmp-{}-{n}", std::process::id()))
}

#[cfg(windows)]
fn writer_exited(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    // SAFETY: we only query the process and close the handle returned by OpenProcess.
    unsafe {
        let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if process.is_null() {
            return GetLastError() == ERROR_INVALID_PARAMETER;
        }
        let exited = WaitForSingleObject(process, 0) == WAIT_OBJECT_0;
        CloseHandle(process);
        exited
    }
}

#[cfg(windows)]
fn remove_abandoned_stage(path: &Path, pid: u32) {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_DISPOSITION_INFO, FILE_FLAG_OPEN_REPARSE_POINT, FileDispositionInfo, SetFileInformationByHandle,
    };
    // Pin this directory entry before rechecking the PID. A concurrent cleaner or a
    // process reusing the PID must not replace the file between the check and deletion.
    let Ok(file) = std::fs::OpenOptions::new().access_mode(DELETE).share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path) else { return };
    if writer_exited(pid) {
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: file owns the handle and info has the layout required by FileDispositionInfo.
        unsafe {
            SetFileInformationByHandle(file.as_raw_handle(), FileDispositionInfo,
                (&raw const info).cast(), size_of::<FILE_DISPOSITION_INFO>() as u32);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Upper {
        version: u32,
        runs: AtomicUsize,
    }

    impl Converter for Upper {
        fn id(&self) -> &'static str {
            "test.upper"
        }
        fn version(&self) -> u32 {
            self.version
        }
        fn convert(&self, input: &[u8]) -> Result<Vec<u8>> {
            self.runs.fetch_add(1, Ordering::Relaxed);
            Ok(input.to_ascii_uppercase())
        }
    }

    #[test]
    fn converts_once_then_hits() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let a = cache.get_or_convert(&conv, b"abc").unwrap();
        let b = cache.get_or_convert(&conv, b"abc").unwrap();
        assert_eq!(a, b);
        assert_eq!(std::fs::read(&a).unwrap(), b"ABC");
        assert_eq!(conv.runs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_written_key_is_replaced_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let key = CacheKey::new(&conv, b"abc");
        assert_eq!(cache.read(&key), None);
        cache.write(&key, b"one").unwrap();
        assert_eq!(cache.read(&key).as_deref(), Some(&b"one"[..]));
        cache.write(&key, b"second").unwrap();
        assert_eq!(cache.read(&key).as_deref(), Some(&b"second"[..]));
        // no staging file is left beside it
        let path = cache.path_for(&key);
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn failed_publication_removes_the_staging_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let path = cache.path_for(&CacheKey::new(&conv, b"abc"));
        std::fs::create_dir_all(&path).unwrap();

        assert!(cache.get_or_convert(&conv, b"abc").is_err());
        assert!(path.is_dir());
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_retry_reclaims_only_a_dead_writers_stages() {
        use std::process::{Command, Stdio};
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let conv = Upper { version: 1, runs: AtomicUsize::new(0) };
        let key = CacheKey::new(&conv, b"table");
        let path = cache.write(&key, b"published").unwrap();
        let shard = path.parent().unwrap();
        let mut child = Command::new("cmd.exe").args(["/d", "/q"])
            .stdin(Stdio::piped()).stdout(Stdio::null()).spawn().unwrap();
        let stage = shard.join(format!(".tmp-{}-7", child.id()));
        let unrelated = stage.with_extension("backup");
        std::fs::write(&stage, b"partial table").unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();
        cache.reclaim_staging_files(&key);
        let live_stage = stage.is_file();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(live_stage);
        cache.reclaim_staging_files(&key);
        assert!(!stage.exists());
        assert_eq!(cache.read(&key).unwrap(), b"published");
        assert_eq!(std::fs::read(unrelated).unwrap(), b"keep");
    }

    #[test]
    fn version_bump_invalidates() {
        let v1 = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let v2 = Upper {
            version: 2,
            runs: AtomicUsize::new(0),
        };
        assert_ne!(CacheKey::new(&v1, b"x"), CacheKey::new(&v2, b"x"));
    }

    #[test]
    fn refuses_cache_inside_repo() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache");
        assert!(Cache::open(inside).is_err());
    }
}
