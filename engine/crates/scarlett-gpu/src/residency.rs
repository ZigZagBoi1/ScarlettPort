//! Host-memory admission for speculative texture work and reusable upload storage.
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
static PRESSURED: AtomicBool = AtomicBool::new(false);
static HEADROOM: AtomicU64 = AtomicU64::new(u64::MAX);
/// Windows' commit is near its limit ([`commit_tight`]).
static COMMIT_TIGHT: AtomicBool = AtomicBool::new(false);

fn memory_status() -> Option<MEMORYSTATUSEX> {
    // SAFETY: the API initializes the fields of a correctly sized structure.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status)
}

// The texture sweep may run for seconds inside a single submitted frame.
static NEXT_COMMIT_SAMPLE: AtomicU64 = AtomicU64::new(0);
static AVAILABLE_COMMIT: AtomicU64 = AtomicU64::new(u64::MAX);
static RETRY_UPLOAD_AT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn allocation_error(error: &windows::core::Error) {
    if error.code() == windows::Win32::Foundation::E_OUTOFMEMORY {
        RETRY_UPLOAD_AT.fetch_max(crate::host::coarse_ms().saturating_add(500), Ordering::Relaxed);
    }
}

/// How often refusals after the first are summed up in one line.
const SUMMARY_MS: u64 = 10_000;

/// Host buffers Windows refused for want of memory (`E_OUTOFMEMORY`), counted
/// so that the log says it once with the cause and then sums the rest up:
/// players' logs ended in hundreds of identical "no upload buffer" lines a
/// millisecond apart, and the cause, the PC's RAM plus page file running
/// out, was named nowhere (issue #583).
struct Refusals {
    told: bool,
    /// Refusals since the last line.
    count: u64,
    /// When the first of those came.
    since: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum Tell {
    /// The first refusal of the run: say what ran out and what to do.
    First,
    /// `count` more in the last `secs` seconds.
    Summary { count: u64, secs: u64 },
    Quiet,
}

impl Refusals {
    const fn new() -> Self {
        Self { told: false, count: 0, since: 0 }
    }

    /// One more refusal at `now` (ms).
    fn refused(&mut self, now: u64) -> Tell {
        if !self.told {
            self.told = true;
            return Tell::First;
        }
        if self.count == 0 {
            self.since = now;
        }
        self.count += 1;
        self.due(now)
    }

    /// The refusals not yet told, once [`SUMMARY_MS`] have passed since the first of them.
    fn due(&mut self, now: u64) -> Tell {
        let elapsed = now.saturating_sub(self.since);
        if self.count == 0 || elapsed < SUMMARY_MS {
            return Tell::Quiet;
        }
        Tell::Summary { count: std::mem::take(&mut self.count), secs: elapsed / 1000 }
    }
}

static REFUSALS: std::sync::Mutex<Refusals> = std::sync::Mutex::new(Refusals::new());

fn refusals() -> std::sync::MutexGuard<'static, Refusals> {
    REFUSALS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn gb(bytes: u64) -> f64 {
    bytes as f64 / GIB as f64
}

/// The line for the first refusal, with the system commit (`used` of `limit`
/// bytes; both 0 when it could not be read).
fn first_line(what: &str, size: u64, used: u64, limit: u64) -> String {
    let commit = if limit == 0 {
        "RAM + page file".to_string()
    } else {
        format!("RAM + page file: {:.1} of {:.1} GB committed", gb(used), gb(limit))
    };
    format!(
        "no {what} of {size} bytes: Windows has run out of memory for programs ({commit}); \
         a bigger page file (README) keeps the game going. Idle host caches are kept short while it lasts, \
         and later refusals for want of memory are summed up every {} s",
        SUMMARY_MS / 1000
    )
}

fn summary_line(count: u64, secs: u64) -> String {
    let s = if count == 1 { "" } else { "s" };
    format!("{count} more allocation{s} refused for want of memory in the last {secs} s")
}

/// Whether `error` is Windows refusing memory for a host buffer (`what`, of
/// `size` bytes). If it is, it is counted and told here and the caller says
/// nothing more; any other error the caller logs as before. The first one
/// also keeps idle host caches short, as [`commit_tight`] does, until the
/// once-per-second sample finds commit back below 80 % of its limit.
pub(crate) fn out_of_memory(error: &windows::core::Error, what: &str, size: u64) -> bool {
    allocation_error(error);
    if error.code() != windows::Win32::Foundation::E_OUTOFMEMORY {
        return false;
    }
    let tell = refusals().refused(crate::host::coarse_ms());
    match tell {
        Tell::First => {
            COMMIT_TIGHT.store(true, Ordering::Relaxed);
            let (used, limit) = memory_status()
                .map_or((0, 0), |s| (s.ullTotalPageFile.saturating_sub(s.ullAvailPageFile), s.ullTotalPageFile));
            tracing::warn!(
                commit_used_mib = used >> 20,
                commit_limit_mib = limit >> 20,
                "{}",
                first_line(what, size, used, limit)
            );
        }
        Tell::Summary { count, secs } => tracing::warn!("{}", summary_line(count, secs)),
        Tell::Quiet => {}
    }
    true
}

thread_local! {
    static STAGING_DEFERRED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn note_staging_deferred() {
    STAGING_DEFERRED.set(true);
}

pub(crate) fn take_staging_deferred() -> bool {
    STAGING_DEFERRED.replace(false)
}

pub(crate) fn upload_refused() -> bool {
    crate::host::coarse_ms() < RETRY_UPLOAD_AT.load(Ordering::Relaxed)
}

fn sample_commit(now: u64) -> u64 {
    let next = NEXT_COMMIT_SAMPLE.load(Ordering::Relaxed);
    if now >= next
        && NEXT_COMMIT_SAMPLE
            .compare_exchange(next, now.saturating_add(32), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        && let Some(status) = memory_status()
    {
        AVAILABLE_COMMIT.store(status.ullAvailPageFile, Ordering::Relaxed);
    }
    AVAILABLE_COMMIT.load(Ordering::Relaxed)
}

fn has_upload_reserve(commit: u64, bake: bool) -> bool {
    // Keep a protective floor against Rust/game OOM aborts without starving
    // bindless world textures or one-shot ped decoration bakes when steady-state
    // commit headroom sits in the 1-4 GiB range on 32 GiB hosts.
    commit >= if bake { 256 * MIB } else { 512 * MIB }
}

fn has_growth_reserve(commit: u64, bake: bool) -> bool {
    // Growing a fresh 64 MiB upload block for speculative bindless prefetch
    // requires more headroom than reusing slices already committed inside the
    // staging pool.
    commit >= if bake { 256 * MIB } else { 1024 * MIB }
}

/// Leave commit for game allocations and submissions already in flight while
/// still letting bindless textures and one-shot material bakes upload.
pub(crate) fn admit_texture_pass(bake: bool) -> bool {
    let now = crate::host::coarse_ms();
    if now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) {
        return false;
    }
    has_upload_reserve(sample_commit(now), bake)
}

/// Whether the staging allocator may commit a brand-new OS upload buffer
/// (reusing existing free ranges in already-committed pool blocks stays open
/// whenever `admit_texture_pass` is true).
pub(crate) fn allow_staging_growth(bake: bool) -> bool {
    let now = crate::host::coarse_ms();
    if now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) {
        return false;
    }
    has_growth_reserve(sample_commit(now), bake)
}

pub(crate) fn limited_host_ram() -> bool {
    static LIMITED: OnceLock<bool> = OnceLock::new();
    *LIMITED.get_or_init(|| memory_status().is_some_and(|s| s.ullTotalPhys <= 32 * GIB))
}

fn next_pressure(was: bool, physical: u64, commit: u64) -> bool {
    if was {
        physical < 5 * GIB || commit < 6 * GIB
    } else {
        physical <= 3 * GIB || commit <= 4 * GIB
    }
}

/// Whether the system's commit (`total - available`) is past 85 % of its
/// limit, with a band so that it does not flip every second: on above 85 %,
/// off again below 80 %. Players' out-of-memory crashes came with commit at
/// its limit while physical memory was still free (issue #311), so caches
/// kept on free RAM alone grew the very thing that ran out.
fn next_commit_tight(was: bool, total: u64, available: u64) -> bool {
    if total == 0 {
        return false;
    }
    let used = u128::from(total.saturating_sub(available)) * 100;
    let total = u128::from(total);
    if was { used >= total * 80 } else { used > total * 85 }
}

/// Windows' commit is near its limit: caches of the PC's memory kept for
/// later use (spare scratch, idle copies, idle host lists) are kept short.
pub(crate) fn commit_tight() -> bool {
    COMMIT_TIGHT.load(Ordering::Relaxed)
}

/// Called by the once-per-second submission sweep. A failed sample keeps the last good state.
pub(crate) fn refresh() {
    // Refusals that came in a burst and then stopped are still summed up.
    if let Tell::Summary { count, secs } = refusals().due(crate::host::coarse_ms()) {
        tracing::warn!("{}", summary_line(count, secs));
    }
    let Some(status) = memory_status() else { return };
    let (physical, commit) = (status.ullAvailPhys, status.ullAvailPageFile);
    HEADROOM.store(physical.min(commit), Ordering::Relaxed);
    let tight_was = commit_tight();
    let tight = next_commit_tight(tight_was, status.ullTotalPageFile, commit);
    if tight != tight_was {
        COMMIT_TIGHT.store(tight, Ordering::Relaxed);
        tracing::info!(
            commit_used_mib = status.ullTotalPageFile.saturating_sub(commit) >> 20,
            commit_limit_mib = status.ullTotalPageFile >> 20,
            "system commit {}: idle host caches {}",
            if tight { "above 85 % of its limit" } else { "back below 80 % of its limit" },
            if tight { "kept short" } else { "kept as before" }
        );
    }
    let was = pressured();
    let pressure = next_pressure(was, physical, commit);
    if pressure != was {
        PRESSURED.store(pressure, Ordering::Relaxed);
        if pressure {
            tracing::warn!(
                pid = std::process::id(),
                frame = crate::frame::current(),
                physical_free_mib = physical >> 20,
                available_commit_mib = commit >> 20,
                buffer_mirror_span_mib = crate::shadow::live_span_bytes() >> 20,
                texture_staging_mib = crate::texture::live_staging_bytes() >> 20,
                spare_scratch_mib = crate::host::ALIVE[4].load(Ordering::Relaxed) >> 20,
                game_filled_gpu_texture_mib = crate::resource::KINDS[1].load(Ordering::Relaxed) >> 20,
                "host memory pressure: limiting texture prefetch and idle upload storage"
            );
        } else {
            tracing::info!(physical_free_mib = physical >> 20, available_commit_mib = commit >> 20, "host memory pressure cleared");
        }
    }
}

pub(crate) fn headroom() -> Option<u64> {
    let bytes = HEADROOM.load(Ordering::Relaxed);
    (bytes != u64::MAX).then_some(bytes)
}

pub(crate) fn pressured() -> bool {
    PRESSURED.load(Ordering::Relaxed)
}

/// True when OS commit headroom is critically low (< 1.5 GiB) or an allocation
/// just failed with `E_OUTOFMEMORY`, requiring emergency throttling.
pub(crate) fn critical_commit() -> bool {
    let now = crate::host::coarse_ms();
    now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) || sample_commit(now) < 1536 * MIB
}

/// A soft per-frame admission limit. A single texture can cross it. The
/// queue retains unserved slots so they can be retried on later frames.
pub(crate) fn prefetch_limit(bytes: u64, pressure: bool, critical: bool, limited: bool, staging: u64) -> u64 {
    if critical {
        bytes.min(16 << 20)
    } else if pressure || (limited && staging >= GIB) {
        bytes.min(64 << 20)
    } else {
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bake_and_lookahead_leave_room_for_live_game_allocations() {
        for (commit, bake, lookahead, growth) in [
            (128 * MIB, false, false, false),
            (256 * MIB, true, false, false),
            (512 * MIB, true, true, false),
            (768 * MIB, true, true, false),
            (1024 * MIB, true, true, true),
        ] {
            assert_eq!(has_upload_reserve(commit, true), bake);
            assert_eq!(has_upload_reserve(commit, false), lookahead);
            assert_eq!(has_growth_reserve(commit, false), growth);
        }
    }

    #[test]
    fn commit_tight_turns_on_above_85_and_off_below_80_percent() {
        assert!(!next_commit_tight(false, 0, 0));
        assert!(!next_commit_tight(false, 100 * GIB, 15 * GIB));
        assert!(next_commit_tight(false, 100 * GIB, 14 * GIB));
        assert!(next_commit_tight(true, 100 * GIB, 20 * GIB));
        assert!(!next_commit_tight(true, 100 * GIB, 21 * GIB));
        // A 48 GiB PC with 45 GiB committed and RAM still free.
        assert!(next_commit_tight(false, 48 * GIB, 3 * GIB));
    }

    #[test]
    fn the_first_refusal_is_told_and_the_rest_summed_up_every_10_s() {
        let mut r = Refusals::new();
        assert_eq!(r.due(50_000), Tell::Quiet);
        assert_eq!(r.refused(1_000), Tell::First);
        // A burst: counted, not told one by one.
        for t in 1_001..1_301 {
            assert_eq!(r.refused(t), Tell::Quiet);
        }
        assert_eq!(r.due(11_000), Tell::Quiet);
        assert_eq!(r.refused(11_001), Tell::Summary { count: 301, secs: 10 });
        // The burst stops: the sweep tells the rest 10 s after the first of them.
        assert_eq!(r.refused(12_000), Tell::Quiet);
        assert_eq!(r.refused(12_500), Tell::Quiet);
        assert_eq!(r.due(21_999), Tell::Quiet);
        assert_eq!(r.due(22_000), Tell::Summary { count: 2, secs: 10 });
        assert_eq!(r.due(40_000), Tell::Quiet);
        // One long after counts from when it came, not from the last line.
        assert_eq!(r.refused(100_000), Tell::Quiet);
        assert_eq!(r.due(110_000), Tell::Summary { count: 1, secs: 10 });
    }

    #[test]
    fn the_first_line_names_the_commit_and_the_page_file() {
        let l = first_line("upload buffer", 4_194_304, 15 * GIB + GIB / 2, 16 * GIB);
        assert!(l.starts_with("no upload buffer of 4194304 bytes: Windows has run out of memory for programs"), "{l}");
        assert!(l.contains("(RAM + page file: 15.5 of 16.0 GB committed)") && l.contains("bigger page file (README)"), "{l}");
        assert!(first_line("shared buffer", 65_536, 0, 0).contains("(RAM + page file);"));
        assert_eq!(summary_line(1, 10), "1 more allocation refused for want of memory in the last 10 s");
        assert_eq!(summary_line(300, 12), "300 more allocations refused for want of memory in the last 12 s");
    }

    #[test]
    fn either_constraint_enters_pressure_and_both_must_recover() {
        assert!(next_pressure(false, 12 * GIB, 4 * GIB));
        assert!(next_pressure(false, 3 * GIB, 16 * GIB));
        assert!(next_pressure(true, 4 * GIB, 7 * GIB));
        assert!(next_pressure(true, 6 * GIB, 5 * GIB));
        assert!(!next_pressure(true, 5 * GIB, 6 * GIB));
        assert!(!next_pressure(false, 4 * GIB, 5 * GIB));
    }

    #[test]
    fn prefetch_respects_existing_budgets_and_resumes_after_recovery() {
        assert_eq!(prefetch_limit(512 << 20, false, false, true, GIB), 64 << 20);
        assert_eq!(prefetch_limit(512 << 20, false, false, true, GIB - 1), 512 << 20);
        assert_eq!(prefetch_limit(512 << 20, false, false, false, 2 * GIB), 512 << 20);
        assert_eq!(prefetch_limit(0, true, true, false, 0), 0);
        assert_eq!(prefetch_limit(8 << 20, true, true, false, 0), 8 << 20);
        assert_eq!(prefetch_limit(512 << 20, true, false, false, 0), 64 << 20);
        assert_eq!(prefetch_limit(512 << 20, true, true, false, 0), 16 << 20);
        assert_eq!(prefetch_limit(192 << 20, false, false, false, 0), 192 << 20);
    }
}
