//! Measurement: what each compute dispatch of chosen programs reads and
//! writes, one line per dispatch, to a file (`SCARLETT_SKIN_LOG=<hash
//! prefix>,..` or `all`, `SCARLETT_SKIN_LOG_FILE=<path>`). Each line: the
//! frame (presents so far), the host list, the program, the group counts,
//! a hash of the root constants and user data, then every buffer the
//! program's tables and root views name: kind, address, length and a hash
//! of the title memory there when recorded. Off when the variable is unset;
//! meant for working out which dispatches repeat, offline.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

static FRAME: AtomicU64 = AtomicU64::new(0);

struct Log {
    want: Vec<String>,
    out: Option<std::io::BufWriter<std::fs::File>>,
    written: u64,
    buf: Vec<u8>,
}

fn log() -> &'static Mutex<Option<Log>> {
    static LOG: OnceLock<Mutex<Option<Log>>> = OnceLock::new();
    LOG.get_or_init(|| {
        let want: Vec<String> = std::env::var("SCARLETT_SKIN_LOG")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if want.is_empty() {
            return Mutex::new(None);
        }
        let path = std::env::var("SCARLETT_SKIN_LOG_FILE").unwrap_or_else(|_| "skinlog.txt".into());
        let out = std::fs::File::create(&path).ok().map(|f| std::io::BufWriter::with_capacity(1 << 20, f));
        tracing::info!(?want, path, "SCARLETT_SKIN_LOG: compute dispatches' inputs and outputs to a file");
        Mutex::new(Some(Log { want, out, written: 0, buf: Vec::new() }))
    })
}

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SCARLETT_SKIN_LOG").is_some_and(|v| !v.is_empty()))
}

/// A present.
pub(crate) fn frame() {
    FRAME.fetch_add(1, Relaxed);
    if on()
        && let Some(l) = log().lock().unwrap_or_else(|e| e.into_inner()).as_mut()
        && let Some(o) = &mut l.out
    {
        let _ = o.flush();
    }
}

/// Whether dispatches of `hash` are logged.
pub(crate) fn wants(hash: &str) -> bool {
    log().lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|l| {
        l.want.iter().any(|w| w == "all" || hash.starts_with(w.as_str()))
    })
}

/// The hash (16 hex digits) of `len` bytes of title memory at `address`
/// (`big` over 1 MB), `-` if they cannot be read.
pub(crate) fn content(address: u64, len: u64) -> String {
    let mut g = log().lock().unwrap_or_else(|e| e.into_inner());
    let Some(l) = g.as_mut() else { return "-".into() };
    let n = len as usize;
    if len > 1 << 20 {
        return "big".into();
    }
    if address == 0 || n == 0 {
        return "-".into();
    }
    l.buf.resize(n, 0);
    if !crate::host::read_into(address as *const u8, &mut l.buf) {
        return "-".into();
    }
    let h = blake3::hash(&l.buf);
    let b = h.as_bytes();
    format!("{:016x}", u64::from_le_bytes(b[..8].try_into().unwrap_or_default()))
}

/// One dispatch's line (without the newline).
pub(crate) fn write(line: &str) {
    let mut g = log().lock().unwrap_or_else(|e| e.into_inner());
    let Some(l) = g.as_mut() else { return };
    if l.written > 1 << 30 {
        return;
    }
    if let Some(o) = &mut l.out {
        let _ = writeln!(o, "{}\t{line}", FRAME.load(Relaxed));
        l.written += line.len() as u64 + 8;
    }
}

/// Measurement only, never to ship: `SCARLETT_SKIP_SET_<NAME>=<hash
/// prefix>,..` names a set of compute programs whose dispatches are
/// dropped while the live switch `SCARLETT_SKIP_SET_<NAME>` is on in an
/// in-session A/B (`SCARLETT_AB`), or always with `SCARLETT_SKIP_SETS_ON=1`.
/// For the frame time each costs, with a broken picture.
struct SkipSet {
    name: String,
    hashes: Vec<String>,
}

fn skip_sets() -> &'static [SkipSet] {
    static SETS: OnceLock<Vec<SkipSet>> = OnceLock::new();
    SETS.get_or_init(|| {
        let sets: Vec<SkipSet> = std::env::vars()
            .filter(|(k, _)| k.starts_with("SCARLETT_SKIP_SET_"))
            .map(|(name, v)| SkipSet {
                name,
                hashes: v.split(',').map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect(),
            })
            .collect();
        for s in &sets {
            tracing::warn!(set = s.name, hashes = ?s.hashes, "measurement: compute dispatches of these programs are dropped while the set is on");
        }
        sets
    })
}

/// Whether a dispatch of the program `hash` is dropped now.
pub(crate) fn skip(hash: impl FnOnce() -> String) -> bool {
    let sets = skip_sets();
    if sets.is_empty() {
        return false;
    }
    static ALWAYS: OnceLock<bool> = OnceLock::new();
    let always = *ALWAYS.get_or_init(|| std::env::var("SCARLETT_SKIP_SETS_ON").is_ok_and(|v| v == "1"));
    let live: Vec<&SkipSet> = sets.iter().filter(|s| always || crate::ab::pick(&s.name).unwrap_or(false)).collect();
    if live.is_empty() {
        return false;
    }
    let h = hash();
    live.iter().any(|s| s.hashes.iter().any(|w| h.starts_with(w.as_str())))
}

/// Measurement: `SCARLETT_SKIN_READERS=<hex address>+<hex length>,..`: the
/// pipelines whose draws or dispatches name a buffer overlapping one of
/// those ranges (table views, root views, vertex buffers), counted, and
/// logged every 200 presents. For finding who reads the skinning output.
fn reader_ranges() -> &'static [(u64, u64)] {
    static R: OnceLock<Vec<(u64, u64)>> = OnceLock::new();
    R.get_or_init(|| {
        std::env::var("SCARLETT_SKIN_READERS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|r| {
                let (a, n) = r.trim().split_once('+')?;
                let a = u64::from_str_radix(a.trim_start_matches("0x"), 16).ok()?;
                let n = u64::from_str_radix(n.trim_start_matches("0x"), 16).ok()?;
                Some((a, a + n))
            })
            .collect()
    })
}

pub(crate) fn readers_on() -> bool {
    !reader_ranges().is_empty()
}

static READERS: Mutex<Option<(u64, std::collections::BTreeMap<String, u64>)>> = Mutex::new(None);

/// A draw or dispatch of `name` names the buffer `[a, a + n)`, `how`.
pub(crate) fn named(name: &dyn Fn() -> String, a: u64, n: u64, how: &str) {
    if !reader_ranges().iter().any(|&(lo, hi)| a < hi && a + n.max(1) > lo) {
        return;
    }
    let mut g = READERS.lock().unwrap_or_else(|e| e.into_inner());
    let (since, map) = g.get_or_insert_with(|| (FRAME.load(Relaxed), Default::default()));
    *map.entry(format!("{} {how}", name())).or_default() += 1;
    let now = FRAME.load(Relaxed);
    if now >= *since + 200 {
        let frames = (now - *since).max(1) as f64;
        for (k, v) in map.iter() {
            tracing::info!("skin readers: {k}: {:.1}/frame", *v as f64 / frames);
        }
        map.clear();
        *since = now;
    }
}
