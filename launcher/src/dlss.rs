//! NVIDIA DLSS Super Resolution's files (NVIDIA Streamline): fetched on request from NVIDIA's own
//! GitHub releases (github.com/NVIDIA-RTX/Streamline) into the data folder, after NVIDIA's licence
//! is shown. They are NVIDIA's, under NVIDIA's licence; ScarlettPort neither ships nor carries them.
//! Only the files DLSS upscaling loads are taken out of the release archive, by ranged requests
//! (the archive is about 263 MB; the files about 61 MB), with their licence texts beside them.
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

pub const TAG: &str = "v2.14.16";
const AGENT: &str = "ScarlettPort-Launcher";
/// What DLSS upscaling loads: Streamline's interposer, its common and DLSS plugins, and DLSS.
pub const DLLS: [&str; 4] = ["sl.interposer.dll", "sl.common.dll", "sl.dlss.dll", "nvngx_dlss.dll"];

pub fn archive_url() -> String {
    format!("https://github.com/NVIDIA-RTX/Streamline/releases/download/{TAG}/streamline-sdk-{TAG}.zip")
}

pub fn licence_url() -> String {
    format!("https://github.com/NVIDIA-RTX/Streamline/blob/{TAG}/license.txt")
}

fn licence_raw_url() -> String {
    format!("https://raw.githubusercontent.com/NVIDIA-RTX/Streamline/{TAG}/license.txt")
}

/// `<data>/streamline`: the licence, and `bin/x64` with the DLLs.
pub fn root() -> PathBuf {
    crate::runtime::data_dir().join("streamline")
}

/// The folder the engine is pointed at (SCARLETT_STREAMLINE_DIR).
pub fn dir() -> PathBuf {
    root().join("bin").join("x64")
}

pub fn installed() -> bool {
    DLLS.iter().all(|d| dir().join(d).is_file())
}

/// NVIDIA's licence for Streamline, from NVIDIA's repository, to show before anything is fetched.
pub fn licence() -> Result<String, String> {
    let r = ureq::get(&licence_raw_url()).set("User-Agent", AGENT).timeout(std::time::Duration::from_secs(30)).call().map_err(|e| e.to_string())?;
    r.into_string().map_err(|e| e.to_string())
}

/// A file on a server read through ranged requests, in blocks.
struct Remote {
    url: String,
    len: u64,
    at: u64,
    block_at: u64,
    block: Vec<u8>,
    fetched: u64,
    progress: Box<dyn Fn(u64) + Send>,
}

const BLOCK: u64 = 4 << 20;

impl Remote {
    fn open(url: &str, progress: Box<dyn Fn(u64) + Send>) -> Result<Self, String> {
        // The release address redirects to a storage host; the size comes with a one-byte range.
        let r = ureq::get(url).set("User-Agent", AGENT).set("Range", "bytes=0-0").timeout(std::time::Duration::from_secs(60)).call().map_err(|e| e.to_string())?;
        if r.status() != 206 {
            return Err("the server does not give parts of the archive".into());
        }
        let real = r.get_url().to_string();
        let len = r.header("Content-Range").and_then(|c| c.rsplit('/').next()).and_then(|n| n.trim().parse().ok()).ok_or("no archive size")?;
        Ok(Remote { url: real, len, at: 0, block_at: 0, block: Vec::new(), fetched: 0, progress })
    }

    fn fill(&mut self) -> std::io::Result<()> {
        let first = self.at;
        let last = (first + BLOCK).min(self.len) - 1;
        let r = ureq::get(&self.url)
            .set("User-Agent", AGENT)
            .set("Range", &format!("bytes={first}-{last}"))
            .timeout(std::time::Duration::from_secs(120))
            .call()
            .map_err(std::io::Error::other)?;
        if r.status() != 206 {
            return Err(std::io::Error::other("the server does not give parts of the archive"));
        }
        let mut buf = Vec::with_capacity((last - first + 1) as usize);
        r.into_reader().take(last - first + 1).read_to_end(&mut buf)?;
        self.fetched += buf.len() as u64;
        (self.progress)(self.fetched);
        self.block_at = first;
        self.block = buf;
        Ok(())
    }
}

impl Read for Remote {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.at >= self.len || out.is_empty() {
            return Ok(0);
        }
        let inside = self.at >= self.block_at && self.at < self.block_at + self.block.len() as u64;
        if !inside {
            self.fill()?;
        }
        let from = (self.at - self.block_at) as usize;
        let n = out.len().min(self.block.len() - from);
        out[..n].copy_from_slice(&self.block[from..from + n]);
        self.at += n as u64;
        Ok(n)
    }
}

impl Seek for Remote {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let at = match to {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => self.len as i64 + n,
            SeekFrom::Current(n) => self.at as i64 + n,
        };
        if at < 0 {
            return Err(std::io::Error::other("seek before the start"));
        }
        self.at = at as u64;
        Ok(self.at)
    }
}

/// Whether an entry of the archive is one to keep, and where under [`root`] it goes.
fn wanted(name: &str) -> Option<PathBuf> {
    let name = name.replace('\\', "/");
    if name.contains("/development/") || name.ends_with('/') {
        return None;
    }
    let file = name.rsplit('/').next()?;
    let lower = file.to_ascii_lowercase();
    let parent_is_bin = name.to_ascii_lowercase().ends_with(&format!("bin/x64/{lower}"));
    if parent_is_bin && (DLLS.contains(&lower.as_str()) || lower.ends_with(".license.txt")) {
        return Some(PathBuf::from("bin").join("x64").join(file));
    }
    // The SDK's own licence at its top (one folder deep at most).
    (lower == "license.txt" && name.matches('/').count() <= 1).then(|| PathBuf::from("license.txt"))
}

/// Fetches the DLSS files into [`dir`]. `progress(got, total)`: bytes fetched, against an
/// estimate of the total (only a part of the archive is fetched).
pub fn download(progress: Box<dyn Fn(u64, u64) + Send>) -> Result<(), String> {
    const ESTIMATE: u64 = 64 << 20;
    let remote = Remote::open(&archive_url(), Box::new(move |got| progress(got, ESTIMATE.max(got))))?;
    let mut zip = zip::ZipArchive::new(remote).map_err(|e| format!("the archive could not be read: {e}"))?;
    let root = root();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let Some(rel) = wanted(entry.name()) else { continue };
        let to = root.join(&rel);
        std::fs::create_dir_all(to.parent().unwrap_or(&root)).map_err(|e| e.to_string())?;
        let part = to.with_extension("part");
        let mut out = std::fs::File::create(&part).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut out).map_err(|e| format!("{}: {e}", rel.display()))?;
        drop(out);
        std::fs::rename(&part, &to).map_err(|e| e.to_string())?;
    }
    let missing: Vec<&str> = DLLS.iter().copied().filter(|d| !dir().join(d).is_file()).collect();
    if !missing.is_empty() {
        return Err(format!("not in the archive: {}", missing.join(", ")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_dlss_files_and_licences_are_taken() {
        let w = |n: &str| wanted(n).map(|p| p.to_string_lossy().replace('\\', "/"));
        assert_eq!(w("streamline-sdk-v2.14.16/bin/x64/sl.interposer.dll").as_deref(), Some("bin/x64/sl.interposer.dll"));
        assert_eq!(w("bin/x64/nvngx_dlss.dll").as_deref(), Some("bin/x64/nvngx_dlss.dll"));
        assert_eq!(w("bin/x64/nvngx_dlss.license.txt").as_deref(), Some("bin/x64/nvngx_dlss.license.txt"));
        assert_eq!(w("streamline-sdk-v2.14.16/license.txt").as_deref(), Some("license.txt"));
        assert_eq!(w("bin/x64/development/sl.interposer.dll"), None);
        assert_eq!(w("bin/x64/sl.dlss_g.dll"), None);
        assert_eq!(w("bin/x64/nvngx_dlssg.dll"), None);
        assert_eq!(w("docs/license.txt/x"), None);
    }
}
