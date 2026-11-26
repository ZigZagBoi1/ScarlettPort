//! Live switches: runtime switches that can be flipped mid-run, at a frame
//! boundary with nothing in the air (the in-session A/B mode, [`crate::ab`]).
//!
//! Each switch is a [`Switch`] static at its use site. Its first read takes
//! the value from the environment (`VICTORY_<NAME>`) and registers an entry
//! in a global name -> atomic map; every later read is one relaxed atomic
//! load. [`set`] changes the value for every reader from then on, and
//! [`reset`] puts back what the environment gave. Only names in [`LIVE`]
//! may be set; [`BOOT_ONLY`] names (state built at boot from them, shaders
//! or tables keyed on them) are refused (state built at boot cannot change
//! mid-session).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Switches that are safe to flip at a frame boundary once flights are
/// settled (names without the `VICTORY_` prefix).
pub const LIVE: &[&str] = &[
    "COPY_NOQUERY",
    "CB_RUNS",
    "CB_RELAY_SPAN",
    "CB_WIDE_MERGE",
    "CB_ALIASWRITE",
    "MERGE_RUNS",
    "MERGE_GAP",
    "MERGE_RACE",
    "SKIP_WC_BACK",
    "READFIX",
    "PIPELINE_WAIT_MS",
    "HOIST_CLAMPS",
    "STATE_DEDUP",
    "KICK_MERGE",
    "MEMWAIT_SAMEQUEUE",
    "MEM_SUBPAGE",
    "FPS_CAP",
    "LIMITER",
    "BIND_CACHE",
    "TABLE_BUFFERS",
    "SYNC_NULL_SKIP",
    "DECLARE_ONCE",
    "INDIRECT_GATHER",
    "INDIRECT_REST",
    "HEAP_SYNC_THREADS",
    "TEXTURE_HEAPS",
    "STAGING_BLOCKS",
    "PIPE_OWNED_MERGE",
    "CARD_ROOM_KEPT_MB",
    "CARD_SYNC_BATCH",
    "TARGET_VARIANTS",
    "FAIR_DUE",
    "RELOOK_MS",
    "TEX_SPAN_PCT",
];

/// Switches known and NOT settable live (see the table in live-ab.md).
pub const BOOT_ONLY: &[&str] = &[
    "REMAP_LAZY",
    "REMAP_LAZY_COMMIT",
    "MEM2_PAGE_REHASH",
    "MEM2_GPU_ONLY_NOVIEW",
    "MEM2_GPU_ONLY_NOSNAP",
    "MEMORY_MAP",
    "RAM_RELIEF_MB",
    "RELAY",
    "SYNC_PRECISE",
    "PS_HWSGPR",
    "D16_UINT",
    "FAST_BOOT",
    "SCREEN_WORKERS",
    "ALIAS_CAST",
];

/// `VICTORY_CB_RUNS` -> `CB_RUNS`.
pub fn short(name: &str) -> &str {
    let n = name.trim();
    n.strip_prefix("VICTORY_").unwrap_or(n)
}

/// Whether `name` may be flipped live.
pub fn is_live(name: &str) -> bool {
    LIVE.contains(&short(name))
}

struct Entry {
    value: AtomicU64,
    boot: u64,
}

fn registry() -> &'static Mutex<HashMap<String, &'static Entry>> {
    static R: OnceLock<Mutex<HashMap<String, &'static Entry>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// A value as a switch reads it: a number, or `on`/`true` (1) and
/// `off`/`false` (0); anything else is `None` (the default applies).
pub fn parse(v: &str) -> Option<u64> {
    let v = v.trim();
    match v {
        "on" | "true" | "yes" => Some(1),
        "off" | "false" | "no" => Some(0),
        _ => v.parse().ok(),
    }
}

fn entry(name: &str, default: u64) -> &'static Entry {
    let key = short(name).to_string();
    let mut r = registry().lock().unwrap_or_else(|e| e.into_inner());
    r.entry(key.clone()).or_insert_with(|| {
        let boot = std::env::var(format!("VICTORY_{key}")).ok().and_then(|v| parse(&v)).unwrap_or(default);
        let now = pending().as_mut().and_then(|m| m.remove(&key)).unwrap_or(boot);
        Box::leak(Box::new(Entry { value: AtomicU64::new(now), boot }))
    })
}

/// A live switch, declared as a static where it is read.
pub struct Switch {
    name: &'static str,
    default: u64,
    cell: OnceLock<&'static Entry>,
}

impl Switch {
    pub const fn new(name: &'static str, default: u64) -> Self {
        Switch { name, default, cell: OnceLock::new() }
    }

    /// The value now (one relaxed load after the first read).
    #[inline]
    pub fn get(&self) -> u64 {
        self.cell.get_or_init(|| entry(self.name, self.default)).value.load(Ordering::Relaxed)
    }

    /// `get() != 0`.
    #[inline]
    pub fn on(&self) -> bool {
        self.get() != 0
    }

    /// The name as declared (without `VICTORY_`).
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The value the environment gave at boot.
    pub fn boot(&self) -> u64 {
        self.cell.get_or_init(|| entry(self.name, self.default)).boot
    }
}

/// Why [`set`] refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    BootOnly(String),
    Unknown(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::BootOnly(n) => write!(f, "{n} is boot-only, not settable live"),
            Refused::Unknown(n) => write!(f, "{n} is not a known live switch"),
        }
    }
}

/// Whether `name` could be set; does nothing.
pub fn check(name: &str) -> Result<(), Refused> {
    let n = short(name);
    if is_live(n) {
        Ok(())
    } else if BOOT_ONLY.contains(&n) {
        Err(Refused::BootOnly(n.to_string()))
    } else {
        Err(Refused::Unknown(n.to_string()))
    }
}

/// Values set before their switch was first read: taken by its first read.
static PENDING: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

fn pending() -> std::sync::MutexGuard<'static, Option<HashMap<String, u64>>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Sets live switch `name` to `value` for every reader from now on. A
/// switch not read yet takes it at its first read (its boot value is still
/// the environment's or its default, for [`reset`]).
pub fn set(name: &str, value: u64) -> Result<(), Refused> {
    check(name)?;
    let key = short(name);
    let r = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(e) = r.get(key) {
        e.value.store(value, Ordering::Relaxed);
    } else {
        pending().get_or_insert_with(HashMap::new).insert(key.to_string(), value);
    }
    Ok(())
}

/// Puts `name` back to its boot value.
pub fn reset(name: &str) {
    let key = short(name);
    if let Some(m) = pending().as_mut() {
        m.remove(key);
    }
    let r = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(e) = r.get(key) {
        e.value.store(e.boot, Ordering::Relaxed);
    }
}

/// The value a switch has now, if it was read or set (for logging).
pub fn value(name: &str) -> Option<u64> {
    let key = short(name);
    if let Some(v) = pending().as_ref().and_then(|m| m.get(key).copied()) {
        return Some(v);
    }
    registry().lock().unwrap_or_else(|e| e.into_inner()).get(key).map(|e| e.value.load(Ordering::Relaxed))
}
