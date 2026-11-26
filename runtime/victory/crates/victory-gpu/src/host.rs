//! The host GPU: one D3D12 device and direct queue (decision 0007), and a
//! window to present on.
//!
//! shortcut: every submission is waited for, so CPU and GPU never overlap
//! (unless `VICTORY_LATE_WAIT=1`, see [`late`]);
//! overlap them with per-submission fences once more than clears run.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject, WaitForSingleObjectEx};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCWSTR, w};

/// Resource states without the console's own bits (compression hints from
/// bit 25 up), which the host rejects.
pub(crate) fn pc_state(state: u32) -> u32 {
    state & 0x01ff_ffff
}

/// A console state as a host texture state: PC bits only, and none of the
/// buffer-only ones the console allows on textures (vertex, constant and
/// index buffer, stream out, indirect arguments, as in "generic read").
pub(crate) fn texture_state(state: u32) -> u32 {
    const BUFFER_ONLY: u32 = 0x1 | 0x2 | 0x100 | 0x200;
    pc_state(state) & !BUFFER_ONLY
}

/// A DXGI description (nul-padded UTF-16) as text.
fn adapter_name(description: &[u16]) -> String {
    let end = description.iter().position(|&c| c == 0).unwrap_or(description.len());
    String::from_utf16_lossy(&description[..end])
}

/// Render target views live only until their command is recorded, so a
/// ring of host descriptors is enough.
const RTV_RING: u32 = 1024;

pub(crate) struct Host {
    pub(crate) device: ID3D12Device,
    pub(crate) casts_fully_typed_formats: bool,
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    /// The last value signalled on `fence`, behind the submission lock.
    submitted: Mutex<u64>,
    event: HANDLE,
    rtvs: ID3D12DescriptorHeap,
    rtv_size: u32,
    /// A ring of depth views, used as `rtvs`.
    dsvs: ID3D12DescriptorHeap,
    dsv_size: u32,
    next_dsv: AtomicU32,
    /// With `VICTORY_D3D12_DEBUG` set (diagnostics builds): the debug
    /// layer's messages.
    messages: Option<ID3D12InfoQueue>,
    next_rtv: AtomicU32,
    presenter: Mutex<Option<Presenter>>,
    /// The adapter the device is on, for its memory budget
    /// ([`Host::video_memory`]); `None` if the system does not name it.
    adapter: Option<IDXGIAdapter3>,
}

// SAFETY: D3D12 devices, queues, fences and descriptor heaps are
// free-threaded; the event handle is only waited on under `submitted`.
unsafe impl Send for Host {}
// SAFETY: as above.
unsafe impl Sync for Host {}

/// Host objects the title let go of while a submission may still use them
/// (a texture its descriptors name, a descriptor slot's old resource), by
/// the submission count they wait for. The title's descriptor writes and
/// releases reach the host at once, from its own threads, while the host
/// runs a submission: freeing there is a GPU page fault on the freed
/// texture (a device hang).
static RETIRED: Mutex<Vec<(u64, Retired)>> = Mutex::new(Vec::new());
/// Submissions started and finished (waited for).
static STARTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FINISHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A retired object, dropped on whatever thread finishes the submission.
struct Retired(#[allow(dead_code)] Box<dyn std::any::Any>);
// SAFETY: what is retired is D3D12 objects (free-threaded) and the
// reference-counted wrappers holding them, dropped once.
unsafe impl Send for Retired {}

/// Milliseconds since the first call.
pub(crate) fn now_ms() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// This moment, for telling later how long ago it was ([`long_ago`]): the
/// frames begun so far (high half, `crate::frame::current`) and tenths of a
/// second since the start. Host copies of title memory, staging buffers and
/// host textures note when they were last used with it.
pub(crate) fn moment() -> u64 {
    (crate::frame::current() << 32) | ((now_ms() / 100) & 0xffff_ffff)
}

/// Whether `moment` lies at least `ms` milliseconds and `frames` frames
/// back. Both: at seconds per frame, what every frame uses is not touched
/// for seconds, and at many frames per second not for many frames. (A title
/// that never waits for frames has no frame count: nothing is then idle.)
pub(crate) fn long_ago(moment: u64, ms: u64, frames: u64) -> bool {
    let now = self::moment();
    let tenths = (now & 0xffff_ffff).saturating_sub(moment & 0xffff_ffff);
    (now >> 32).saturating_sub(moment >> 32) >= frames && tenths * 100 >= ms
}

/// Available physical RAM on the host in bytes, from `GlobalMemoryStatusEx`.
pub(crate) fn available_physical_ram() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    let ok = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
    ok.then_some(m.ullAvailPhys)
}

/// Effective idle timeout (in milliseconds) for VRAM-backed host textures,
/// scaled down under VRAM pressure. Never returns less than 1,000 ms so
/// handles not yet recorded into a list or slot remain protected.
pub(crate) fn vram_idle_ms(video_memory: Option<[u64; 4]>) -> u64 {
    const NORMAL_MS: u64 = 5_000;
    const MODERATE_MS: u64 = 2_500;
    const HIGH_MS: u64 = 1_000;
    let Some([budget, used, ..]) = video_memory else {
        return NORMAL_MS;
    };
    if budget == 0 {
        return NORMAL_MS;
    }
    let used_pct = (u128::from(used) * 100) / u128::from(budget);
    if used_pct >= 85 {
        HIGH_MS
    } else if used_pct >= 75 {
        MODERATE_MS
    } else {
        NORMAL_MS
    }
}

/// Effective idle timeout (in milliseconds) for system-RAM caches (staging
/// buffers, loose shadows, and the mapped pipeline library), scaled down
/// when free physical RAM is low. Never returns less than 2,000 ms.
pub(crate) fn ram_idle_ms(avail_phys: Option<u64>) -> u64 {
    const GIB: u64 = 1024 * 1024 * 1024;
    const NORMAL_MS: u64 = 10_000;
    const MODERATE_MS: u64 = 5_000;
    const HIGH_MS: u64 = 2_000;
    let Some(avail) = avail_phys else {
        return NORMAL_MS;
    };
    if avail < 2 * GIB {
        HIGH_MS
    } else if avail < 4 * GIB {
        MODERATE_MS
    } else {
        NORMAL_MS
    }
}

/// How many references are held on a host object (its COM count): ours
/// all, since the host's views and commands hold none. A buffer or texture
/// with one is held by its owner alone: no descriptor slot keeps it
/// (`HostHeap::kept`) and no list (`HostList::keep`).
pub(crate) fn references(object: &impl Interface) -> u32 {
    type Method = unsafe extern "system" fn(*mut std::ffi::c_void) -> u32;
    let raw = object.as_raw();
    // SAFETY: a live COM object, whose first three methods are those of
    // `IUnknown` (query, add a reference, release one); the pair leaves the
    // count as it was, and the release returns it. The first is not called.
    unsafe {
        let methods = *raw.cast::<*const [Method; 3]>();
        ((*methods)[1])(raw);
        ((*methods)[2])(raw)
    }
}

/// `VICTORY_RAM_RELIEF_MB=<n>`: when the PC has less than `n` MB of
/// physical memory available, the process's working set is emptied into the
/// page file, once every five seconds (`K32EmptyWorkingSet`).
///
/// Why: a title that commits more than the PC holds (19.5 GB against 15.8
/// GB here) reaches an allocation that fails, and this title writes through
/// the null pointer it got: `0xc0000005` at `victory-image+0x23cf683` at
/// world entry, with the OOM report's `memory.load_percent` at 94 to 99.
/// Faulting the working set out for a moment hands it the room to finish
/// that allocation. It is a valve, not a fix: every page faulted out is
/// faulted back in afterwards. Off by default.
fn relieve() {
    static LIMIT: OnceLock<Option<u64>> = OnceLock::new();
    let Some(limit) = *LIMIT.get_or_init(|| {
        std::env::var("VICTORY_RAM_RELIEF_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&mb| mb > 0)
    }) else {
        return;
    };
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = now_ms();
    if now.saturating_sub(LAST.load(Ordering::Relaxed)) < 15_000 {
        return;
    }
    let Some(free) = available_physical_ram() else {
        return;
    };
    if free >> 20 >= limit {
        return;
    }
    LAST.store(now, Ordering::Relaxed);
    use windows_sys::Win32::System::ProcessStatus::K32EmptyWorkingSet;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: our own process and no out parameters.
    let ok = unsafe { K32EmptyWorkingSet(GetCurrentProcess()) } != 0;
    tracing::info!(
        free_mb = free >> 20,
        emptied = ok,
        "memory relief: the working set was emptied into the page file"
    );
}

/// Once a second, before a submission starts: lets go of what nothing
/// names any more (copies of windows of title memory, staging buffers,
/// host copies of textures the title fills).
/// `VICTORY_SWEEP=0` keeps everything, as before.
fn sweep() {
    // The low-memory valve first, whatever the sweep is set to.
    relieve();
    // The in-place probe and census, if asked for (`VICTORY_INPLACE_PROBE`).
    crate::inplace::tick();
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VICTORY_SWEEP").is_none_or(|v| v != "0")) {
        return;
    }
    crate::vram::tick();
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = now_ms();
    let last = LAST.load(Ordering::Relaxed);
    if now < last + crate::vram::sweep_plan(0, 0).sweep_every_ms
        || LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    crate::pipeline::trim_library();
    crate::shadow::let_go_idle();
    crate::texture::let_go_idle();
    crate::resource::let_go_idle();
    crate::tiles::give_back_let_go();
}

/// Drops `object` once every submission running now has finished: at once
/// if none is.
pub(crate) fn retire<T: 'static>(object: T) {
    let started = STARTED.load(Ordering::Acquire);
    if started <= FINISHED.load(Ordering::Acquire) {
        RETIREMENTS[0].fetch_add(1, Ordering::Relaxed);
        drop(object);
        return;
    }
    RETIREMENTS[1].fetch_add(1, Ordering::Relaxed);
    RETIRED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((started, Retired(Box::new(object))));
}

/// Submissions started and finished so far ([`STARTED`], [`FINISHED`]): a
/// heap map's pool slot given back while submission n may run is handed
/// out again once n has finished (`descriptor::slot_map`), as [`retire`]
/// drops objects.
pub(crate) fn submissions() -> (u64, u64) {
    (STARTED.load(Ordering::Acquire), FINISHED.load(Ordering::Acquire))
}

/// Drops what waited for submissions up to `finished`.
fn bury(finished: u64) {
    let done: Vec<(u64, Retired)> = {
        let mut retired = RETIRED.lock().unwrap_or_else(|e| e.into_inner());
        if retired.is_empty() {
            return;
        }
        let (done, keep) = std::mem::take(&mut *retired)
            .into_iter()
            .partition(|(at, _)| *at <= finished);
        *retired = keep;
        done
    };
    drop(done);
}

/// Objects [`retire`] dropped at once (no submission was running) and
/// kept until a running submission finished, since the start.
static RETIREMENTS: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// What the layer did to the host's objects lately, newest last (sweeps
/// that let host copies go, pipeline library calls, saves and trims): said
/// once when the host device is removed ([`log_removal_context`]), with
/// what the submitting thread and the pipeline workers were doing at that
/// moment. A device the driver removes "internally" names no command, and
/// the pass that happens to notice first names only the view it was at:
/// the three such losses of runs 72, 73 and 82 each came within 5 ms of a
/// pipeline library call that no line of the log tied to them.
/// `VICTORY_TRAIL=0`: nothing is kept and nothing is said.
static TRAIL: Mutex<std::collections::VecDeque<(u64, u32, String)>> = Mutex::new(std::collections::VecDeque::new());
/// The entries kept.
const TRAIL_MOST: usize = 96;

fn trail_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TRAIL").is_none_or(|v| v != "0"))
}

/// Microseconds since the first call.
pub(crate) fn now_us() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_micros() as u64
}

/// The calling thread, as the log's other lines name threads.
pub(crate) fn thread_id() -> u32 {
    // SAFETY: a plain query.
    unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }
}

/// Notes something done to the host's objects ([`TRAIL`]); `what` is only
/// run when the trail is kept.
pub(crate) fn trail(what: impl FnOnce() -> String) {
    if !trail_on() {
        return;
    }
    let entry = (now_us(), thread_id(), what());
    // Sweeps also go to the log as they happen: the trail is only said at
    // a loss, and the driver's errors in the system's event log (run 224's
    // first fell inside a sweep of 4,711 host textures) need the sweeps of
    // clean runs beside them.
    if entry.2.starts_with("sweep:") {
        tracing::info!("{}", entry.2);
    }
    let mut trail = TRAIL.lock().unwrap_or_else(|e| e.into_inner());
    if trail.len() >= TRAIL_MOST {
        trail.pop_front();
    }
    trail.push_back(entry);
}

/// What the submitting thread is doing (0 nothing: between submissions,
/// 1 preparing one, 2 waiting for the card to run it, 3 copying back what
/// it wrote), the lists of that submission, and since when ([`now_us`]).
static SUBMITTING: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

fn submitting(phase: u64, lists: usize) {
    SUBMITTING[1].store(lists as u64, Ordering::Relaxed);
    SUBMITTING[2].store(now_us(), Ordering::Relaxed);
    SUBMITTING[0].store(phase, Ordering::Relaxed);
}

/// What was going on when the host device went: whether a submission was
/// on the card, the pipeline library's state and the calls running in it,
/// and the trail.
pub(crate) fn removal_context() -> String {
    use std::fmt::Write;
    let now = now_us();
    let ms = |at: u64| now.saturating_sub(at) as f64 / 1000.0;
    let phase = match SUBMITTING[0].load(Ordering::Relaxed) {
        0 => "between submissions (nothing of ours was running on the card)",
        1 => "preparing a submission (nothing of ours was running on the card)",
        2 => "waiting for the card to run a submission",
        _ => "copying back what a submission wrote (nothing of ours was running on the card)",
    };
    let mut text = format!(
        "the submitting thread was {phase}, of {} lists, for {:.1} ms; {}; objects let go at once {} and after a running submission {}; submissions started {} finished {}",
        SUBMITTING[1].load(Ordering::Relaxed),
        ms(SUBMITTING[2].load(Ordering::Relaxed)),
        crate::pipeline::library_removal_note(),
        RETIREMENTS[0].load(Ordering::Relaxed),
        RETIREMENTS[1].load(Ordering::Relaxed),
        STARTED.load(Ordering::Relaxed),
        FINISHED.load(Ordering::Relaxed),
    );
    let trail = TRAIL.lock().unwrap_or_else(|e| e.into_inner());
    let _ = write!(text, "; the last {} things done, newest last:", trail.len());
    for (at, thread, what) in trail.iter() {
        let _ = write!(text, "\n  {:.1} ms before, thread {thread}: {what}", ms(*at));
    }
    text
}

/// Says [`removal_context`] in the log, once: called by whoever finds the
/// host device removed.
pub(crate) fn log_removal_context() {
    #[cfg(windows)]
    crate::ab::device_removed();
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !trail_on() || SAID.swap(true, Ordering::Relaxed) {
        return;
    }
    tracing::error!("when the host device went: {}", removal_context());
}

/// Time spent in submissions since the last frame-rate line, in
/// microseconds: preparing them on the CPU (title memory copies, texture
/// detiling), then waiting for the GPU.
static SUBMISSION_US: [std::sync::atomic::AtomicU64; 2] =
    [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

/// Adds to [`SUBMISSION_US`] and to the frame log's totals.
fn submission_us(i: usize, us: u64) {
    SUBMISSION_US[i].fetch_add(us, Ordering::Relaxed);
    crate::framelog::add([crate::framelog::T::PrepareUs, crate::framelog::T::CardWaitUs][i], us);
    if i == 0 {
        crate::framelog::add(crate::framelog::T::Submissions, 1);
    }
}

/// Adds to [`PREPARING`]'s copies (time, bytes) and to the frame log's totals.
fn preparing(i: usize, n: u64) {
    PREPARING[i].fetch_add(n, Ordering::Relaxed);
    crate::framelog::add([crate::framelog::T::CopyUs, crate::framelog::T::CopyBytes][i], n);
}

/// Adds to [`COPY_BACK`] and to the frame log's totals.
fn copy_back(i: usize, n: u64) {
    COPY_BACK[i].fetch_add(n, Ordering::Relaxed);
    crate::framelog::add([crate::framelog::T::CopyBackUs, crate::framelog::T::CopyBackBytes][i], n);
}

/// Adds to [`CARD_CLOCK_US`] and, for the card's idle times, to the frame
/// log's totals.
fn card_clock_us(i: usize, us: u64) {
    CARD_CLOCK_US[i].fetch_add(us, Ordering::Relaxed);
    match i {
        0 => crate::framelog::add(crate::framelog::T::CardIdleUs, us),
        1 => crate::framelog::add(crate::framelog::T::CardIdleBehindUs, us),
        _ => {}
    }
}

/// After submissions: shader-written ranges compared and copied back into
/// title memory (microseconds, bytes that differed), since the last
/// frame-rate line.
static COPY_BACK: [std::sync::atomic::AtomicU64; 2] =
    [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

/// The card's clock, for the frame line of every run, in microseconds since
/// the last one: how long the card had nothing of ours queued; how much of
/// that while the thread that holds the submission lock was working (the
/// card idle behind us: preparing and copying back are done with the card
/// standing); and how long that thread stood waiting for the card.
///
/// Every send of lists to the queue is followed by a signal on a fence of
/// the clock's own ([`Host::card_sent`]). A thread of the clock waits for
/// the newest signal and notes the moment the card reached it with nothing
/// newer sent; the next send ends that idle stretch.
static CARD_CLOCK_US: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Of the thread that holds the submission lock: the microseconds it has
/// worked so far (preparing, copying back; not its waits for the card), and
/// since when it works now ([`now_us`] plus one; 0: it does not).
static WORKING: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// The thread that holds the submission lock starts (`on`) or stops
/// working; whether it worked until now.
fn working(on: bool) -> bool {
    let now = now_us() + 1;
    let since = WORKING[1].swap(if on { now } else { 0 }, Ordering::Relaxed);
    if since != 0 {
        WORKING[0].fetch_add(now.saturating_sub(since), Ordering::Relaxed);
    }
    since != 0
}

/// [`WORKING`] as of now: microseconds worked so far.
fn worked() -> u64 {
    let since = WORKING[1].load(Ordering::Relaxed);
    let now = if since != 0 { (now_us() + 1).saturating_sub(since) } else { 0 };
    WORKING[0].load(Ordering::Relaxed) + now
}

/// The thread that holds the submission lock stood waiting for the card
/// from `since` until now ([`CARD_CLOCK_US`]).
fn stood(since: std::time::Instant) {
    card_clock_us(2, since.elapsed().as_micros() as u64);
}

/// What [`CARD_CLOCK_US`] is measured with.
struct CardClock {
    fence: ID3D12Fence,
    /// Signals sent so far; and, while the card has reached the last of
    /// them, since when and what [`worked`] said then.
    state: Mutex<(u64, Option<(std::time::Instant, u64)>)>,
    sent: std::sync::Condvar,
}

// SAFETY: a fence (free-threaded) behind a lock.
unsafe impl Send for CardClock {}
// SAFETY: as above.
unsafe impl Sync for CardClock {}

/// The clock's thread: waits for the newest signal sent and notes when the
/// card got there with nothing newer behind it.
fn card_clock_watch(clock: &'static CardClock) {
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        return;
    };
    let mut seen = 0;
    loop {
        let target = {
            let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.0 == seen {
                state = clock.sent.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            state.0
        };
        // SAFETY: the clock's fence and this thread's event.
        unsafe {
            if clock.fence.SetEventOnCompletion(target, event).is_ok() {
                WaitForSingleObject(event, INFINITE);
            }
        }
        seen = target;
        let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.0 == target && state.1.is_none() {
            state.1 = Some((std::time::Instant::now(), worked()));
        }
    }
}

/// For the frame-rate line: [`CARD_CLOCK_US`] per frame, `ms` being the
/// frame's time.
fn card_clock_line(frames: f64, ms: f64) {
    let [idle, behind, stood] = [0, 1, 2].map(|i| CARD_CLOCK_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames);
    tracing::info!(
        "the card's clock per frame: nothing of ours queued {idle:.1} ms (the card about {:.0}% busy by this count), \
         {behind:.1} ms of that while the submitting thread was working (the card idle behind us), the rest while the title \
         had not submitted; the submitting thread stood waiting for the card {stood:.1} ms",
        if ms > 0.0 { (100.0 * (1.0 - idle / ms)).clamp(0.0, 100.0) } else { 0.0 }
    );
}

/// Of the preparing: title memory copies (microseconds, bytes) and texture
/// refreshes (microseconds, bytes hashed), since the last frame-rate line.
pub(crate) static PREPARING: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Host buffers made since the start, as (count, bytes): in the upload heap
/// (staging, list scratch), shared with the processor (host copies of title
/// memory) and the graphics card's own (indirect arguments, twins' copies).
/// For the memory line: against what is alive, they show what was let go.
pub(crate) static MADE: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];

/// What of that is alive now, for the memory line (`MADE` counts what was
/// ever made: 42,607 upload buffers, 77.9 GB, in five minutes of run 60,
/// and nothing said how many were alive): host lists; the lists' scratch
/// chunks and their bytes; of those, the chunks no list holds (kept for the
/// next list, [`Spare`]) and their bytes; chunks taken from those kept
/// instead of made, since the start; and the lists' other upload buffers
/// (texture data copied from buffers, scratch too big for a chunk) and
/// their bytes.
pub(crate) static ALIVE: [std::sync::atomic::AtomicU64; 8] = [const { std::sync::atomic::AtomicU64::new(0) }; 8];

/// About host lists, for the memory line: lists made by the host since the
/// start; preludes made (a list's second allocator and list, made when it
/// first needs one, [`lazy_prelude`]); and, in diagnostics builds, the
/// bytes the process's private memory grew by while lists were being made
/// (what a list costs: host objects with the driver's command memory; 631
/// lists were alive in run 65, each with a prelude it may never use).
/// Then the lists alive that were never reset since they were made: the
/// host sides of title lists the title made and has not used (a host list
/// cost 0.68 MB of private memory in run 69: 431 MB for the 632 the title
/// makes at its start).
pub(crate) static LISTS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// The process's private bytes (diagnostics: around the making of lists).
#[cfg(feature = "diagnostics")]
fn private_bytes() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    c.PagefileUsage as u64
}

/// What the runtime does when the host GPU fails for good: report it like a
/// crash and end the run. The console has no such state, and carrying on
/// draws nothing at seconds per frame.
static ON_FAILURE: OnceLock<fn(&str) -> !> = OnceLock::new();

/// Sets what a host GPU failure does (the runtime's crash report).
pub fn on_failure(report: fn(&str) -> !) {
    let _ = ON_FAILURE.set(report);
}

/// Ends the run on a host GPU failure: what failed, the removal reason and
/// the debug layer's first error, if any (`VICTORY_D3D12_DEBUG=1` in a
/// diagnostics build names the command).
pub(crate) fn fail(what: std::fmt::Arguments) -> ! {
    let mut text = what.to_string();
    if let Some(host) = host() {
        // SAFETY: a plain query.
        if let Err(e) = unsafe { host.device.GetDeviceRemovedReason() } {
            text.push_str(&format!("; device removed: {e}"));
            // DRED's breadcrumbs and page fault, if the failing path did
            // not already add them.
            if dred_on() && !text.contains("breadcrumbs") {
                text.push_str(&host.removal_data(&[]));
            }
            let [budget, used, shared] = crate::vram::last_look();
            let [budget, used, shared] = match host.video_memory() {
                Some([b, u, _, s]) => [b, u, s],
                None => [budget, used, shared],
            };
            let cause = crate::vram::removal_cause(e.code().0 as u32, text.contains("page fault at"), budget, used);
            tracing::error!(
                "DEVICE REMOVED: {cause}; reason {e}; card {} / budget {} MB, shared {} MB, frame {}; DRED {}: {}",
                used >> 20,
                budget >> 20,
                shared >> 20,
                crate::frame::current(),
                if dred_on() { "on" } else { "off (VICTORY_DRED=1 turns it on)" },
                text
            );
            log_removal_context();
        }
        match host.log_messages() {
            Some(first) => text.push_str(&format!("; debug layer: {first}")),
            None if host.messages.is_none() => text.push_str(
                "; a diagnostics build with VICTORY_D3D12_DEBUG=1 names the command",
            ),
            None => {}
        }
    }
    tracing::error!("{text}");
    // Tests have no log: the reason goes to their output.
    #[cfg(test)]
    eprintln!("host GPU failure: {text}");
    match ON_FAILURE.get() {
        Some(report) => report(&text),
        None => std::process::abort(),
    }
}

/// The host GPU, created on first use; `None` without one (a run then
/// behaves as before: nothing executes).
pub(crate) fn host() -> Option<&'static Host> {
    static HOST: OnceLock<Option<Host>> = OnceLock::new();
    // Unit tests with `VICTORY_TEST_NO_GPU=1` run as on a machine without a
    // card (the tests that need one return early), so they can run beside
    // a game on the same GPU.
    #[cfg(test)]
    if std::env::var_os("VICTORY_TEST_NO_GPU").is_some_and(|v| v == "1") {
        return None;
    }
    HOST.get_or_init(|| match create() {
        Ok(host) => Some(host),
        Err(e) => {
            tracing::warn!(error = %e, "no host D3D12 device; command lists will not run");
            None
        }
    })
    .as_ref()
}

fn create() -> windows::core::Result<Host> {
    crate::drvwatch::start();
    // SAFETY: plain D3D12 object creation with valid descriptions.
    unsafe {
        // The debug layer only in builds with diagnostics.
        let debug =
            cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_DEBUG").is_some();
        if debug {
            let mut layer: Option<ID3D12Debug> = None;
            match D3D12GetDebugInterface(&mut layer) {
                Ok(()) => layer.iter().for_each(|l| l.EnableDebugLayer()),
                Err(e) => tracing::warn!(error = %e, "no D3D12 debug layer"),
            }
            // GPU-based validation (`VICTORY_D3D12_GBV`): out-of-range
            // descriptor heap indices and the like become messages naming
            // the command instead of page faults. Without state tracking
            // (directly indexed textures are not transitioned).
            if std::env::var_os("VICTORY_D3D12_GBV").is_some()
                && let Some(l) = &layer
            {
                if let Ok(d1) = l.cast::<ID3D12Debug1>() {
                    d1.SetEnableGPUBasedValidation(true);
                    #[cfg(test)]
                    queue2_tests::GPU_VALIDATION.store(true, Ordering::Relaxed);
                }
                // Tests exercise the resource states of individual commands.
                #[cfg(not(test))]
                if let Ok(d2) = l.cast::<ID3D12Debug2>() {
                    d2.SetGPUBasedValidationFlags(
                        D3D12_GPU_BASED_VALIDATION_FLAGS_DISABLE_STATE_TRACKING,
                    );
                }
                tracing::info!("GPU-based validation on");
            }
        }
        // Diagnostics: device-removed data (the command that faulted, the
        // faulting address), read when the device is removed.
        if dred_on() {
            let mut dred: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
            if D3D12GetDebugInterface(&mut dred).is_ok()
                && let Some(dred) = dred
            {
                dred.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                dred.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                if let Ok(d1) = dred.cast::<ID3D12DeviceRemovedExtendedDataSettings1>() {
                    d1.SetBreadcrumbContextEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                }
                tracing::info!("device-removed data (DRED) on: breadcrumbs and page faults");
            }
        }
        let mut device: Option<ID3D12Device> = None;
        // Diagnostics: the software adapter, on which a shader's wild access
        // is an access violation in this process (a crash report with the
        // address and instruction) instead of a GPU fault and driver reset.
        if cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_WARP").is_some() {
            let adapter = warp_adapter()?;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
            tracing::info!("host device on the software adapter (WARP)");
        } else {
            // A laptop with switchable graphics reports its integrated GPU
            // first, and `D3D12CreateDevice(None, ..)` takes that one, whose
            // wave width and feature set are not the ones the translator is
            // tuned for. Ask DXGI for the high-performance adapter instead,
            // and fall back to the default when there is nothing to pick.
            match discrete_adapter() {
                Some(adapter) => D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?,
                None => D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device)?,
            }
        }
        let device = device.ok_or_else(windows::core::Error::empty)?;
        // `VICTORY_D3D12_BACKGROUND=0`: the runtime's background work (the
        // driver compiling shaders again on the "D3D Background Thread"s,
        // for minutes after pipelines were made or loaded) is switched off.
        // An experiment: while those threads ran, the same shot took
        // 1,000-1,200 ms a frame, and 830-950 ms from the moment they
        // stopped (run 54, 08:32:01 to 08:32:06). Left as it is by default:
        // whether the card is slower with the shaders it then keeps is not
        // known.
        if std::env::var_os("VICTORY_D3D12_BACKGROUND").is_some_and(|v| v == "0")
            && let Ok(device6) = device.cast::<ID3D12Device6>()
        {
            let set = device6.SetBackgroundProcessingMode(
                D3D12_BACKGROUND_PROCESSING_MODE_DISABLE_BACKGROUND_WORK,
                D3D12_MEASUREMENTS_ACTION_KEEP_ALL,
                None,
                None,
            );
            tracing::info!(ok = set.is_ok(), "the host's background shader work is switched off (VICTORY_D3D12_BACKGROUND=0)");
        }
        // the widest wave the GPU runs: 32 (NVIDIA) gets console wave64 compute programs whose
        // cross-lane code stays in 32-lane halves translated for waves of 32
        let mut o1 = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
        if device
            .CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS1, (&raw mut o1).cast(), size_of_val(&o1) as u32)
            .is_ok()
        {
            tracing::info!(min = o1.WaveLaneCountMin, max = o1.WaveLaneCountMax, "host wave lanes");
            victory_shader::emit::set_host_max_wave(o1.WaveLaneCountMax);
        }
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS3::default();
        let casts_fully_typed_formats = device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS3,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
            .is_ok()
            && options.CastingFullyTypedFormatSupported.as_bool();
        let messages = debug
            .then(|| device.cast::<ID3D12InfoQueue>().ok())
            .flatten();
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        })?;
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        let event = CreateEventW(None, false, false, None)?;
        let rtvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let rtv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
        let dsvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let dsv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_DSV);
        let adapter = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            .and_then(|factory| factory.EnumAdapterByLuid::<IDXGIAdapter3>(device.GetAdapterLuid()))
            .ok();
        let card_budget = adapter.as_ref().and_then(|a| {
            let mut own = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            a.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut own).ok().map(|()| own.Budget)
        });
        crate::rebar::probe(&device, card_budget);
        // Which card the device landed on. `D3D12CreateDevice(None, ..)` takes
        // the system's default adapter; on a laptop with switchable graphics
        // that can be the integrated one, whose wave width and feature set
        // are not the ones the translator is tuned for.
        if let Some(adapter) = adapter.as_ref()
            && let Ok(desc) = adapter.GetDesc1()
        {
            tracing::info!(
                name = adapter_name(&desc.Description),
                vendor = format_args!("{:#x}", desc.VendorId),
                vram_mb = desc.DedicatedVideoMemory >> 20,
                "host adapter"
            );
        }
        tracing::info!("host D3D12 device ready");
        Ok(Host {
            device,
            casts_fully_typed_formats,
            queue,
            fence,
            submitted: Mutex::new(0),
            event,
            rtvs,
            rtv_size,
            dsvs,
            dsv_size,
            next_dsv: AtomicU32::new(0),
            messages,
            next_rtv: AtomicU32::new(0),
            presenter: Mutex::new(None),
            adapter,
        })
    }
}

/// The adapter DXGI calls high-performance: on a laptop with switchable
/// graphics the discrete card, not the integrated one. `None` when DXGI is
/// too old to be asked, has nothing to offer, or offers software.
fn discrete_adapter() -> Option<IDXGIAdapter1> {
    // SAFETY: plain DXGI adapter queries.
    unsafe {
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let adapter: IDXGIAdapter1 = factory
            .EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
            .ok()?;
        let desc = adapter.GetDesc1().ok()?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            tracing::warn!(
                vendor = format_args!("{:#x}", desc.VendorId),
                "DXGI offers software as the high-performance adapter; taking the default instead"
            );
            return None;
        }
        Some(adapter)
    }
}

/// DXGI may return hardware for `EnumWarpAdapter` (notably under Proton).
/// Reject it before creating a device in diagnostics' software-only mode.
pub(crate) fn warp_adapter() -> windows::core::Result<IDXGIAdapter> {
    // SAFETY: plain DXGI adapter queries.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter()?;
        let desc = adapter.cast::<IDXGIAdapter1>()?.GetDesc1()?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
            tracing::warn!(
                vendor = format_args!("{:#x}", desc.VendorId),
                flags = format_args!("{:#x}", desc.Flags),
                "DXGI did not identify the WARP adapter as software; refusing device"
            );
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_FAIL,
            ));
        }
        Ok(adapter)
    }
}

#[cfg(test)]
mod tests {
    /// Run once with VICTORY_CARD_SYNC_BATCH=0 and once with it set to 1.
    #[test]
    #[ignore = "requires a D3D12 adapter and the card hash pipeline; run alone"]
    fn two_dirty_card_buffers_reach_readback_in_the_same_submission() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        const MIB: usize = 1 << 20;
        const PIECE: usize = 64 << 10;
        let host = super::host().expect("a D3D12 device");
        assert!(host.card_ready(), "the card hash pipeline must be available");
        // SAFETY: a private allocation held through the fenced submissions.
        let allocation = unsafe {
            VirtualAlloc(std::ptr::null(), 2 * MIB, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE)
        };
        assert!(!allocation.is_null());
        let bases = [allocation as usize, allocation as usize + MIB];
        let shadows = bases.map(|base| {
            crate::shadow::register(base, base as u64, MIB as u64);
            let copy = crate::shadow::find(base as u64, MIB as u64).unwrap();
            assert!(copy.carded(), "both title buffers must have a card copy");
            copy
        });
        let readback = host.read_back_buffer(16).unwrap();
        let hash_readback = host.read_back_buffer(64).unwrap();
        let mut previous = [0u128; 4];
        for round in [0x11u32, 0x55] {
            let list = host.new_list().unwrap();
            let mut expected = [0u32; 4];
            for (i, (shadow, &base)) in shadows.iter().zip(&bases).enumerate() {
                for (j, offset) in [32, PIECE + 20].into_iter().enumerate() {
                    let k = 2 * i + j;
                    expected[k] = round * 0x0101_0101 + k as u32;
                    // SAFETY: aligned dword in the private allocation.
                    unsafe { ((base + offset) as *mut u32).write_volatile(expected[k]); }
                    shadow.refresh(&list, (base + offset) as u64, 4);
                    // Request the before-hash for each of the two distinct pieces.
                    shadow.gpu_writes(&list, (base + offset) as u64, 4);
                    // SAFETY: the readback and the copy outlive this fenced list.
                    unsafe { list.list.CopyBufferRegion(&readback, (k * 4) as u64, &shadow.buffer, offset as u64, 4); }
                }
            }
            list.close();
            host.execute(&[&list]);
            let mut mapped = std::ptr::null_mut();
            // SAFETY: execute waited for all four writes, and the buffer holds 16 bytes.
            let got = unsafe {
                readback.Map(0, None, Some(&mut mapped)).unwrap();
                let got = mapped.cast::<[u32; 4]>().read_unaligned();
                readback.Unmap(0, None);
                got
            };
            assert_eq!(got, expected, "both copied pieces of each buffer");
            let hash_list = host.new_list().unwrap();
            for (i, shadow) in shadows.iter().enumerate() {
                let hashes = shadow.card_sides().unwrap().1.expect("before-hashes were made");
                // SAFETY: two consecutive 16-byte piece hashes from each live buffer.
                unsafe { hash_list.list.CopyBufferRegion(&hash_readback, (32 * i) as u64, &hashes, 0, 32); }
            }
            hash_list.close();
            host.execute(&[&hash_list]);
            // SAFETY: all 64 bytes of the readback were written and fenced.
            let current = unsafe {
                hash_readback.Map(0, None, Some(&mut mapped)).unwrap();
                let words = mapped.cast::<[u128; 4]>().read_unaligned();
                hash_readback.Unmap(0, None);
                words
            };
            for (i, hash) in current.iter().enumerate() {
                assert_ne!(*hash, previous[i], "piece {i} must be rehashed");
                assert!(!current[..i].contains(hash), "each piece has different input");
            }
            previous = current;
        }
        for base in bases {
            crate::shadow::unregister(base, base as u64);
        }
        drop(shadows);
        // SAFETY: the lists completed and the copies were unregistered.
        unsafe { VirtualFree(allocation, 0, MEM_RELEASE); }
    }

    #[test]
    fn sparse_copies_preserve_islands_and_observe_remaps() {
        use windows_sys::Win32::System::Memory::*;
        const N: usize = 5 << 16;
        // SAFETY: we reserve a private region, commit selected pages inside it,
        // and only directly access the pages while they are writable.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), N, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            let mut expected = vec![0; N];
            for (offset, value) in [(0, 9), (2 << 16, 37), (N - 4096, 85)] {
                let at = base.add(offset);
                assert_eq!(VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE), at.cast());
                at.write_bytes(value, 4096);
                expected[offset..offset + 4096].fill(value);
            }
            for remap in [false, true] {
                if remap {
                    let mut old = 0;
                    assert_ne!(VirtualProtect(base.cast(), 4096, PAGE_NOACCESS, &mut old), 0);
                    expected[..4096].fill(0);
                    let at = base.add(1 << 16);
                    assert_eq!(VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE), at.cast());
                    at.write_bytes(0xc7, 4096);
                    expected[1 << 16..(1 << 16) + 4096].fill(0xc7);
                }
                let mut out = vec![0xee; N];
                super::copy_mapped(out.as_mut_ptr().add(27), base.add(27), N - 54, false);
                assert_eq!(out[27..N - 27], expected[27..N - 27]);
                assert!(out[..27].iter().chain(&out[N - 27..]).all(|&v| v == 0xee));
            }
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    use super::*;

    /// Every adapter DXGI reports on this machine, and the one the host
    /// device landed on. Run with `cargo test --release -p victory-gpu --
    /// --ignored --nocapture adapters_on_this_machine`.
    #[test]
    #[ignore = "reports the machine's adapters"]
    fn adapters_on_this_machine() {
        // SAFETY: plain DXGI adapter queries.
        unsafe {
            let factory = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
                .expect("a DXGI factory");
            let mut index = 0;
            while let Ok(adapter) = factory.EnumAdapters1(index) {
                let desc = adapter.GetDesc1().expect("an adapter description");
                println!(
                    "adapter {index}: {} (vendor {:#x}, {} MiB dedicated, flags {:#x})",
                    adapter_name(&desc.Description),
                    desc.VendorId,
                    desc.DedicatedVideoMemory >> 20,
                    desc.Flags
                );
                index += 1;
            }
            match host().and_then(|host| host.adapter.as_ref()) {
                Some(adapter) => {
                    let desc = adapter.GetDesc1().expect("an adapter description");
                    println!("the host device is on {}", adapter_name(&desc.Description));
                }
                None => println!("no host device was made on this machine"),
            }
        }
    }

    /// Whichever card DXGI calls high-performance, it is never the software
    /// one: the translator's output is for a real card. Runs on any machine.
    #[test]
    fn the_high_performance_adapter_is_never_software() {
        let Some(adapter) = discrete_adapter() else {
            return; // DXGI has nothing to prefer here
        };
        // SAFETY: a plain DXGI adapter query.
        let desc = unsafe { adapter.GetDesc1() }.expect("an adapter description");
        assert_eq!(
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32,
            0,
            "{} was offered as the high-performance adapter",
            adapter_name(&desc.Description)
        );
    }

    #[test]
    fn warp_adapter_accepts_only_software() {
        // This probes DXGI without creating a D3D12 device or running a title.
        // SAFETY: plain DXGI adapter queries.
        let raw = unsafe {
            let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            else {
                return;
            };
            factory.EnumWarpAdapter::<IDXGIAdapter>()
        };
        let Ok(raw) = raw else { return };
        // SAFETY: EnumWarpAdapter returns a DXGI adapter, which implements
        // IDXGIAdapter1; `raw` owns that interface.
        let desc = unsafe { raw.cast::<IDXGIAdapter1>().unwrap().GetDesc1().unwrap() };
        eprintln!(
            "EnumWarpAdapter: vendor={:#x}, flags={:#x}",
            desc.VendorId, desc.Flags
        );
        assert_eq!(
            warp_adapter().is_ok(),
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
        );
    }
}

/// A host command list and its allocator, always used together.
pub(crate) struct HostList {
    allocator: ID3D12CommandAllocator,
    pub(crate) list: ID3D12GraphicsCommandList,
    /// Work for submission time, in recorded order.
    deferred: Mutex<Deferred>,
    /// Upload memory for data the list's draws make (expanded vertices),
    /// reused after a reset.
    scratch: Mutex<Scratch>,
    /// Host texture states the list assumes, by host copy: at its start and
    /// at its end. Lists are recorded on many threads and run in submission
    /// order, so the transitions from a texture's real state are made at
    /// submission, in `prelude`. Holding the copies keeps every texture the
    /// list names alive until it is reset, even if the title releases the
    /// resource first.
    states: Mutex<States>,
    /// What the list does with depth textures that have a colour twin, by
    /// twin ([`TwinUse`]).
    twins: Mutex<std::collections::HashMap<usize, TwinUse>>,
    /// Placed views read through an alias copy (by title object): the
    /// source's host copy and its write count when last copied in this
    /// list (Marvisak #2341).
    aliases: Mutex<std::collections::HashMap<usize, (usize, u64)>>,
    /// The list failed to close: it is dropped at submission.
    pub(crate) broken: std::sync::atomic::AtomicBool,
    /// The list that runs those transitions just before this one, made
    /// when the list first has such transitions ([`lazy_prelude`]).
    prelude: OnceLock<Option<Prelude>>,
    /// The pipelines the list binds, for reports of slow submissions.
    pipelines: Mutex<std::collections::BTreeSet<String>>,
    /// The work its indirect executions may still do (`indirect`): a
    /// shader-writable word, zeroed when the list is submitted, and after
    /// it what the clamp notes about mesh records. In memory the CPU maps.
    budget: OnceLock<Option<(ID3D12Resource, usize)>>,
    /// The same words in the card's own memory ([`card_budget`]), which the
    /// clamp uses instead when there is one: zeroed in the list before its
    /// first clamp, its notes copied to `budget` when the list closes.
    card_budget: OnceLock<Option<ID3D12Resource>>,
    /// Device-local scratch buffer for clamped indirect arguments (`indirect`),
    /// grown to the largest indirect execution recorded on the list.
    indirect_args: Mutex<Option<(ID3D12Resource, u64)>>,
    /// `VICTORY_BARRIER_MERGE`: work that may write through unordered access
    /// was recorded since the list's last barrier on all resources (true at
    /// the start: lists before it in the same submission are not ordered
    /// against it).
    uav_pending: std::sync::atomic::AtomicBool,
    /// `VICTORY_BARRIER_HAZARD`: what dispatches accessed since the list's
    /// last barrier on all resources, and the title's held requests.
    hazards: Mutex<victory_core::sync::Hazards>,
    /// `VICTORY_PREP_BARRIERS`: barriers asked for and not recorded yet (as
    /// the words of `D3D12_RESOURCE_BARRIER`s, borrowed resource pointers):
    /// recorded as one call before the list's next other command.
    held_barriers: Mutex<Vec<[u64; 4]>>,
    /// Whether `held_barriers` holds any (read without the lock on every
    /// title call).
    has_held_barriers: std::sync::atomic::AtomicBool,
    /// Dispatches and buffer copies recorded since the reset: what may have
    /// written indirect arguments (`crate::indirect` clamps hoisted before).
    writes: std::sync::atomic::AtomicU64,
    /// Buffers for clamped indirect arguments when this list holds the
    /// clamps of the list after it (`clamp_room`).
    clamp_chunks: Mutex<ClampChunks>,
    /// Counts the list in [`ALIVE`] while it lives.
    _alive: Counted,
    /// The pair of timestamps its run is timed with since its last reset
    /// (`crate::cardmeter`), or none.
    meter: AtomicU32,
    /// The records its draws read from slots that its prelude fills
    /// (`crate::indirect::gather`).
    gather: Mutex<Gather>,
    /// The texts of the markers `note_op` set, in order (for device-removed
    /// data without marker texts), with the host GPU range a copy reads.
    #[cfg(feature = "diagnostics")]
    notes: Mutex<Vec<Note>>,
    /// With `VICTORY_GPU_TIMING`: a GPU timestamp before each noted command
    /// and at the end, and where they are read back.
    #[cfg(feature = "diagnostics")]
    stamps: OnceLock<Option<(ID3D12QueryHeap, ID3D12Resource)>>,
}

/// A list's buffers for hoisted clamped indirect arguments with their
/// states, how many are in use since the reset, the bytes taken of the last
/// of those, and whether the list has the barrier that precedes the clamps.
#[derive(Default)]
struct ClampChunks {
    all: Vec<(ID3D12Resource, D3D12_RESOURCE_STATES)>,
    used: usize,
    at: u64,
    barrier: bool,
}

/// A list's prelude: an allocator and a closed list.
type Prelude = (ID3D12CommandAllocator, ID3D12GraphicsCommandList);

/// What a list gathers for its prelude to clamp (`crate::indirect::gather`).
#[derive(Default)]
struct Gather {
    /// Something recorded in the list may write arguments: no record is
    /// gathered from here on (the ones gathered before are read before
    /// the list runs, which is before that).
    closed: bool,
    /// One set per argument buffer, kind and stride.
    sets: Vec<crate::indirect::GatherSet>,
    /// The budget words the gathered records are charged to.
    budget: Option<ID3D12Resource>,
    /// Scratch buffers of earlier recordings, kept for the next sets.
    spare: Vec<ID3D12Resource>,
}

/// The most argument buffers a list gathers from.
const GATHER_SETS: usize = 8;

/// Whether a host list's prelude is made when the list first needs one. A
/// prelude runs the texture transitions and twin copies a list needs before
/// it; lists that name no texture never have one, and every host list was
/// made with one all the same: twice the host objects per list, for the
/// title's 630 lists and for every list made anew.
/// `VICTORY_LAZY_PRELUDE=0`: made with the list, as before.
fn lazy_prelude() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LAZY_PRELUDE").is_none_or(|v| v != "0"))
}

/// Timestamps per list (diagnostics); commands past them are not timed.
#[cfg(feature = "diagnostics")]
const STAMPS: u32 = 4096;

/// A marker's text and the host range its command reads (diagnostics).
#[cfg(feature = "diagnostics")]
type Note = (String, Option<(u64, u64)>);

/// By host copy: the state at the list's start and where it is now, the
/// copy, and how many times the list has used it in a state that writes
/// (Marvisak #2341: an alias copied from it is current only for the count
/// it was copied at).
type States = std::collections::HashMap<
    usize,
    (u32, u32, std::sync::Arc<crate::resource::HostTexture>, u64),
>;

/// Host states a texture is written in ([`States`]' write count).
const WRITE_STATES: u32 = (D3D12_RESOURCE_STATE_RENDER_TARGET.0
    | D3D12_RESOURCE_STATE_UNORDERED_ACCESS.0
    | D3D12_RESOURCE_STATE_DEPTH_WRITE.0
    | D3D12_RESOURCE_STATE_COPY_DEST.0
    | D3D12_RESOURCE_STATE_RESOLVE_DEST.0) as u32;

/// What a list does with a depth texture that has a colour twin
/// ([`crate::resource::Twin`]). Lists are recorded on many threads and run
/// in submission order, so a list cannot know which side an earlier list
/// leaves behind: it notes which sides it uses as it finds them, and its
/// prelude copies one side into the other at submission if that side is
/// behind then. From its first use of a side on, the list knows.
struct TwinUse {
    /// The depth texture (the title resource's host copy) and its twin.
    depth: std::sync::Arc<crate::resource::HostTexture>,
    twin: std::sync::Arc<crate::resource::Twin>,
    /// The sides (depth texture, twin) the list uses as it finds them.
    needs: [bool; 2],
    /// Whether each side lacks what the other was written last, where the
    /// list is now; `None`: as the list found it.
    behind: [Option<bool>; 2],
    /// The draw or dispatch being prepared writes the twin.
    writing: bool,
    /// The depth plane's twin holds what the plane lacks although the list
    /// copied the twins since: the plane's pass waits for the list's close.
    late: bool,
}

/// The pass that draws the twin of a depth plane into the plane: one
/// triangle over each mip of each slice, its pixel shader writing the
/// twin's texel as the depth, with the depth test "always" and no stencil
/// (the stencil plane is copied through a buffer and not touched here).
///
/// A 16-bit twin holds whole steps of 1/65535, and the depth written is
/// the step times the float nearest 1/65535 (0x37800080). That is how
/// The title's tiled lighting computes the depth of the
/// rectangles it then draws with the test "equal", one class of materials
/// a draw (all six of its vertex shaders multiply the class number by that
/// constant), so the pass and the draw give the host the same float, and
/// the host turns the same float into the same 24 bits. The step divided
/// by 65535 is another float for 512 of the 65,536 steps (none under 257),
/// and lands in another 24-bit step for 127 of them (none under 385).
const PLANE_CORNERS: &str = r#"
float4 corner(uint i : SV_VertexID) : SV_Position {
    return float4(float(i & 1) * 4.0 - 1.0, float(i >> 1) * 4.0 - 1.0, 0.0, 1.0);
}
"#;
const PLANE_STEPS: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL)"
Texture2DArray<uint> twin : register(t0);
[RootSignature(RS)]
float plane(float4 p : SV_Position) : SV_Depth {
    return float(twin.Load(int4(int2(p.xy), 0, 0))) * asfloat(0x37800080);
}
"#;
const PLANE_FLOATS: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL)"
Texture2DArray<float> twin : register(t0);
[RootSignature(RS)]
float plane(float4 p : SV_Position) : SV_Depth {
    return twin.Load(int4(int2(p.xy), 0, 0));
}
"#;

/// Views of twins the pass reads, in a heap of ours used as a ring.
/// shortcut: a view must last until its list has run; with one or two
/// passes a frame the ring is far from coming round.
const PLANE_VIEWS: u32 = 256;

/// What the pass draws with: the heap of its views of twins, and per kind
/// of twin (16-bit steps or floats) and depth format the root signature
/// and pipeline, made when first needed (`None`: the host refused).
struct PlanePass {
    views: ID3D12DescriptorHeap,
    cpu: usize,
    gpu: u64,
    step: u32,
    next: AtomicU32,
    made: Mutex<Vec<(bool, i32, Option<PlanePipeline>)>>,
}

/// The pass's root signature and pipeline for one kind of twin and one
/// depth format.
type PlanePipeline = (ID3D12RootSignature, ID3D12PipelineState);

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for PlanePass {}
// SAFETY: as above.
unsafe impl Sync for PlanePass {}

fn plane_pass() -> Option<&'static PlanePass> {
    static PASS: OnceLock<Option<PlanePass>> = OnceLock::new();
    PASS.get_or_init(|| {
        let host = host()?;
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        // SAFETY: plain heap creation and queries.
        unsafe {
            let views: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: PLANE_VIEWS,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .ok()?;
            Some(PlanePass {
                cpu: views.GetCPUDescriptorHandleForHeapStart().ptr,
                gpu: views.GetGPUDescriptorHandleForHeapStart().ptr,
                step: host.device.GetDescriptorHandleIncrementSize(kind),
                views,
                next: AtomicU32::new(0),
                made: Mutex::default(),
            })
        }
    })
    .as_ref()
}

/// Compiles function `entry` of `source` for `target` with the system's
/// compiler, as the indirect-argument clamp is (`crate::indirect`).
pub(crate) fn compile(
    source: &str,
    entry: windows::core::PCSTR,
    target: windows::core::PCSTR,
) -> windows::core::Result<Vec<u8>> {
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            windows::core::s!("victory"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                )
            };
            tracing::warn!("a shader of ours: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes.
    let bytes = unsafe {
        std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
    };
    Ok(bytes.to_vec())
}

impl PlanePass {
    /// The root signature and pipeline for a twin of 16-bit steps (`steps`)
    /// or of floats and the depth format `format`.
    fn pipeline(&self, steps: bool, format: DXGI_FORMAT) -> Option<PlanePipeline> {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, _, pipeline)) = made.iter().find(|m| (m.0, m.1) == (steps, format.0)) {
            return pipeline.clone();
        }
        let pipeline = match Self::make(steps, format) {
            Ok(pipeline) => Some(pipeline),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    steps,
                    format = format.0,
                    "no pipeline to draw a depth plane from its twin: such planes stay as they are"
                );
                None
            }
        };
        made.push((steps, format.0, pipeline.clone()));
        pipeline
    }

    fn make(steps: bool, format: DXGI_FORMAT) -> windows::core::Result<PlanePipeline> {
        use windows::core::s;
        let device = &host().ok_or_else(windows::core::Error::empty)?.device;
        let vs = compile(PLANE_CORNERS, s!("corner"), s!("vs_5_1"))?;
        let source = if steps { PLANE_STEPS } else { PLANE_FLOATS };
        let ps = compile(source, s!("plane"), s!("ps_5_1"))?;
        let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: bytes.as_ptr().cast(),
            BytecodeLength: bytes.len(),
        };
        // SAFETY: the root signature is inside the pixel shader; the
        // description points at live data and never releases its pointer.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &ps[..])?;
            let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                VS: code(&vs[..]),
                PS: code(&ps[..]),
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                // Every texel is written; stencil stays off.
                DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                    DepthEnable: true.into(),
                    DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                    DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                DSVFormat: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            let pipeline: ID3D12PipelineState = device.CreateGraphicsPipelineState(&desc)?;
            Ok((root, pipeline))
        }
    }
}

/// Records into `list` the pass that draws `plane`, the twin of the depth
/// plane of `depth`, into that plane: `depth` in the depth-write state,
/// `plane` readable by pixel shaders. The pass sets the list's descriptor
/// heap, graphics root signature, pipeline, depth target, viewport,
/// scissor and topology and puts none of them back: it is for a list that
/// records nothing of the title's after it.
fn record_plane_pass(
    list: &ID3D12GraphicsCommandList,
    depth: &ID3D12Resource,
    plane: &ID3D12Resource,
) {
    let (Some(host), Some(pass)) = (host(), plane_pass()) else {
        return;
    };
    // SAFETY: live resources.
    let (of, twin) = unsafe { (depth.GetDesc(), plane.GetDesc()) };
    // An R16 typeless twin (53) holds steps, read as R16_UINT (57); an R32
    // typeless one floats, read as R32_FLOAT (41). The depth view names
    // the depth-stencil format of the texture's family (44: 45, 19: 20).
    let steps = twin.Format.0 == 53;
    let format = DXGI_FORMAT(match of.Format.0 {
        44 => 45,
        19 => 20,
        typed => typed,
    });
    let Some((root, pipeline)) = pass.pipeline(steps, format) else {
        return;
    };
    for slice in 0..u32::from(of.DepthOrArraySize) {
        for mip in 0..u32::from(of.MipLevels) {
            let slot = pass.next.fetch_add(1, Ordering::Relaxed) % PLANE_VIEWS;
            let view = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT(if steps { 57 } else { 41 }),
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2DArray: D3D12_TEX2D_ARRAY_SRV {
                        MostDetailedMip: mip,
                        MipLevels: 1,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                        ..Default::default()
                    },
                },
            };
            let target = D3D12_DEPTH_STENCIL_VIEW_DESC {
                Format: format,
                ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
                    Texture2DArray: D3D12_TEX2D_ARRAY_DSV {
                        MipSlice: mip,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                    },
                },
                ..Default::default()
            };
            let target = host.dsv(depth, Some(&target));
            let (width, height) = ((of.Width >> mip).max(1) as u32, (of.Height >> mip).max(1));
            let cpu = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: pass.cpu + (slot * pass.step) as usize,
            };
            let gpu = D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: pass.gpu + u64::from(slot * pass.step),
            };
            // SAFETY: an open list; a slot of our heap; live resources and
            // objects (the pass's own live as long as the process).
            unsafe {
                host.device
                    .CreateShaderResourceView(plane, Some(&view), cpu);
                list.SetDescriptorHeaps(&[Some(pass.views.clone())]);
                list.SetGraphicsRootSignature(&root);
                list.SetPipelineState(&pipeline);
                list.SetGraphicsRootDescriptorTable(0, gpu);
                list.OMSetRenderTargets(0, None, false, Some(&target));
                list.RSSetViewports(&[D3D12_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: width as f32,
                    Height: height as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                }]);
                list.RSSetScissorRects(&[RECT {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                }]);
                list.IASetPrimitiveTopology(
                    windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY(4),
                );
                list.DrawInstanced(3, 1, 0, 0);
            }
            crate::resource::PLANES[0].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Records into `list` the copy of a depth texture into its colour twin
/// (`colour`) or of the twin into the depth texture, both already in their
/// copy states: whole textures of one size in one format family, which the
/// host copies as they are; or, for the twin of a stencil plane, that plane
/// alone, through the twin's buffer.
fn record_twin_copy(
    list: &ID3D12GraphicsCommandList,
    depth: &ID3D12Resource,
    twin: &crate::resource::Twin,
    colour: bool,
) {
    let (from, to) = if colour {
        (depth, &twin.colour.texture)
    } else {
        (&twin.colour.texture, depth)
    };
    crate::resource::TWINS[2 + usize::from(colour)].fetch_add(1, Ordering::Relaxed);
    // shortcut: the depth plane's twin is not given the plane (that is a
    // pass the other way: the plane read by a shader, the twin as its
    // target). The twin then lacks what draws and clears wrote to the
    // plane since shaders last wrote the twin. In the title
    // nothing writes that plane but the twin's pass (its 192 draws against
    // it only test, run 29), so the twin lacks nothing. Counted.
    if colour && twin.plane.is_some() {
        crate::resource::PLANES[1].fetch_add(1, Ordering::Relaxed);
    }
    let Some(through) = &twin.through else {
        // SAFETY: an open list and two live textures.
        unsafe { list.CopyResource(to, from) };
        return;
    };
    // A stencil plane and its twin: each mip and slice of the plane goes
    // into the buffer, laid out as the host lays the plane out for copies,
    // and from the buffer into the other texture, whole subresources both
    // times. In the depth texture the stencil plane's subresources follow
    // the depth plane's; the twin has the one plane.
    let Some(host) = host() else { return };
    // SAFETY: a live texture.
    let of = unsafe { depth.GetDesc() };
    let planes = u32::from(of.MipLevels) * u32::from(of.DepthOrArraySize);
    let mut rows = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); planes as usize];
    // SAFETY: a plain query writing `planes` footprints.
    unsafe {
        host.device.GetCopyableFootprints(
            &of,
            planes,
            planes,
            0,
            Some(rows.as_mut_ptr()),
            None,
            None,
            None,
        );
    }
    let (first_from, first_to) = if colour { (planes, 0) } else { (0, planes) };
    let subresource = |texture: &ID3D12Resource, index: u32| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: index,
        },
    };
    let placed = |row: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(through) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: *row,
        },
    };
    let (common, source, dest) = (
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    // SAFETY: an open list and live resources; the buffer was made as big
    // as these footprints, and rests in the common state between copies.
    unsafe {
        list.ResourceBarrier(&[transition(through, common, dest)]);
        for (i, row) in rows.iter().enumerate() {
            let index = first_from + i as u32;
            list.CopyTextureRegion(&placed(row), 0, 0, 0, &subresource(from, index), None);
        }
        list.ResourceBarrier(&[transition(through, dest, source)]);
        for (i, row) in rows.iter().enumerate() {
            let index = first_to + i as u32;
            list.CopyTextureRegion(&subresource(to, index), 0, 0, 0, &placed(row), None);
        }
        list.ResourceBarrier(&[transition(through, source, common)]);
    }
}

/// The pipelines of each list's last run, by list, while the indirect
/// guard is on (`VICTORY_INDIRECT_GUARD=1`).
static GUARD_PIPELINES: Mutex<Option<std::collections::HashMap<usize, String>>> = Mutex::new(None);
/// Records emptied by the indirect guard since the start.
static GUARD_BAD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The indirect guard emptied `bad` records in the last run of list `key`:
/// logged (the first 50 times) with the pipelines that run bound, among
/// them the pass that wrote the arguments, and kept in the trail for a
/// device-removed report.
fn indirect_guard_report(key: usize, bad: u32) {
    let all = GUARD_BAD.fetch_add(u64::from(bad), Ordering::Relaxed) + u64::from(bad);
    let pipelines = GUARD_PIPELINES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(&key).cloned())
        .unwrap_or_default();
    static SAID: AtomicU32 = AtomicU32::new(0);
    if SAID.fetch_add(1, Ordering::Relaxed) < 50 {
        tracing::warn!(
            "indirect guard: {bad} dispatch records with NaN/Inf or out-of-range group counts ran empty in list {key:#x} ({all} since start, frame {}); its pipelines (the argument writer is among them): {pipelines}",
            crate::frame::current()
        );
    }
    trail(|| format!("indirect guard: {bad} bad dispatch records emptied in list {key:#x}"));
}

/// Whether device-removed data is on (`VICTORY_D3D12_DRED`, diagnostics
/// builds).
pub(crate) fn dred_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        (cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_DRED").is_some())
            || std::env::var_os("VICTORY_DRED").is_some_and(|v| v == "1")
    })
}

/// Indirect mesh dispatches since the last frame line: the largest group
/// count a record asked for, and the groups asked in all (from the lists'
/// budget buffers, a run late).
#[cfg(feature = "diagnostics")]
static MESH_GROUPS: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Indirect records that ran empty for want of their list's budget, and the
/// largest cost of one (told from 2^22 on), likewise (the clamp's words 6
/// and 7; `crate::indirect::rest`).
static BUDGET_NOTES: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Whether a list's indirect-work budget lives in the card's own memory
/// (`VICTORY_INDIRECT_CARD_BUDGET=0` keeps it in memory the CPU maps, as
/// before). The clamp charges it with atomics, at least two in a row for
/// every clamp; in mapped memory on a card of its own each of them
/// crosses the bus.
fn card_budget() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_INDIRECT_CARD_BUDGET").is_none_or(|v| v != "0"))
}

/// Whether each list of a submission runs and is timed on its own, slow
/// ones logged with their commands (`VICTORY_GPU_TIMING`, diagnostics
/// builds; the commands are noted with `VICTORY_D3D12_DRED` on).
#[cfg(feature = "diagnostics")]
fn timing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GPU_TIMING").is_some())
}

/// Names a host object for the device-removed report, which names the
/// allocations a page fault hit (only with device-removed data on).
pub(crate) fn name(object: &impl Interface, text: impl FnOnce() -> String) {
    if dred_on()
        && let Ok(object) = object.cast::<ID3D12Object>()
    {
        // SAFETY: a plain call on a live object.
        let _ = unsafe { object.SetName(&windows::core::HSTRING::from(text())) };
    }
}

/// One in [`ALIVE`]`[0]` while it lives (a host list), and one in
/// [`LISTS`]`[3]` until it is first reset.
struct Counted {
    /// Not reset since it was made.
    fresh: std::sync::atomic::AtomicBool,
}

impl Counted {
    fn new() -> Counted {
        ALIVE[0].fetch_add(1, Ordering::Relaxed);
        LISTS[3].fetch_add(1, Ordering::Relaxed);
        Counted {
            fresh: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// The list is reset: it is in use.
    fn used(&self) {
        if self.fresh.swap(false, Ordering::Relaxed) {
            LISTS[3].fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        ALIVE[0].fetch_sub(1, Ordering::Relaxed);
        self.used();
    }
}

/// A chunk of upload memory for lists' scratch data: the buffer, its
/// mapping and its host GPU address.
struct Chunk {
    _buffer: ID3D12Resource,
    data: usize,
    gpu: u64,
}

// SAFETY: a D3D12 resource (free-threaded) and its persistent mapping,
// written only by the list that holds the chunk.
unsafe impl Send for Chunk {}

/// Upload chunks handed out front to back: the chunks (each with its
/// bytes), the one in use and the bytes used in it.
#[derive(Default)]
struct Scratch {
    chunks: Vec<(Chunk, u64)>,
    current: usize,
    used: u64,
}

// SAFETY: the chunk pointers are persistent mappings of upload buffers.
unsafe impl Send for Scratch {}

impl Drop for Scratch {
    fn drop(&mut self) {
        give_chunks(std::mem::take(&mut self.chunks));
    }
}

const SCRATCH_CHUNK: u64 = 4 << 20;
/// A list's first chunk when the chunks are shared ([`scratch_pool`]): most
/// lists put a few constants or a rectangle's corners there, and every
/// list that did took 4 MiB of the PC's memory for it.
const SCRATCH_FIRST: u64 = 256 << 10;
/// The most bytes of chunks kept while no list holds them, scaled from total
/// system RAM.
fn spare_most(total_ram_bytes: Option<u64>) -> u64 {
    const GIB: u64 = 1 << 30;
    match total_ram_bytes {
        Some(ram) if ram <= 16 * GIB => 64 << 20,
        // 32 GB Windows systems typically report ~31.7–31.9 GiB in
        // `ullTotalPhys` after firmware/hardware reservations.
        Some(ram) if ram >= 30 * GIB => 256 << 20,
        _ => 128 << 20,
    }
}

/// Whether lists share their scratch chunks: a list takes a chunk when it
/// first needs one, a small one first, and gives all of them back when it
/// is reset or dropped, for the next list. Before, a list kept every chunk
/// it ever took (4 MiB each) for as long as it lived, used or not, and the
/// host lists of held submissions, which live for one submission, each
/// made theirs anew: some 57 upload buffers of 4 MiB made, zeroed and
/// freed per frame of gameplay (run 60).
/// `VICTORY_SCRATCH_POOL=0`: chunks of 4 MiB kept by their list, as before.
fn scratch_pool() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SCRATCH_POOL").is_none_or(|v| v != "0"))
}

/// Things of some sizes that nothing holds now, kept for whoever next asks
/// for one of the same size.
struct Spare<T> {
    kept: Vec<(T, u64)>,
    bytes: u64,
}

impl<T> Spare<T> {
    const fn new() -> Self {
        Spare { kept: Vec::new(), bytes: 0 }
    }

    /// A kept thing of `size` bytes, the one given last.
    fn take(&mut self, size: u64) -> Option<T> {
        let at = self.kept.iter().rposition(|(_, s)| *s == size)?;
        self.bytes -= size;
        Some(self.kept.remove(at).0)
    }

    /// Keeps `thing` (`size` bytes) unless that takes the kept bytes past
    /// `most`: then it comes back.
    fn give(&mut self, thing: T, size: u64, most: u64) -> Option<T> {
        if self.bytes + size > most {
            return Some(thing);
        }
        self.bytes += size;
        self.kept.push((thing, size));
        None
    }
}

/// Scratch chunks no list holds.
static SPARE: Mutex<Spare<Chunk>> = Mutex::new(Spare::new());

/// A scratch chunk of `size` bytes: one kept ([`SPARE`]) or a new one.
fn take_chunk(host: &Host, size: u64) -> Option<Chunk> {
    if scratch_pool()
        && let Some(chunk) = SPARE.lock().unwrap_or_else(|e| e.into_inner()).take(size)
    {
        ALIVE[3].fetch_sub(1, Ordering::Relaxed);
        ALIVE[4].fetch_sub(size, Ordering::Relaxed);
        ALIVE[5].fetch_add(1, Ordering::Relaxed);
        return Some(chunk);
    }
    let (buffer, data) = host.upload_buffer(size)?;
    name(&buffer, || "list scratch".into());
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    ALIVE[1].fetch_add(1, Ordering::Relaxed);
    ALIVE[2].fetch_add(size, Ordering::Relaxed);
    Some(Chunk {
        _buffer: buffer,
        data: data as usize,
        gpu,
    })
}

/// A list is done with `chunks` (it was reset after its submissions ran,
/// or dropped): kept for the next list, or freed past [`spare_most`] and
/// without the pool.
fn give_chunks(chunks: Vec<(Chunk, u64)>) {
    if chunks.is_empty() {
        return;
    }
    let most = spare_most(crate::total_system_ram_bytes());
    let mut freed = Vec::new();
    {
        let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
        for (chunk, size) in chunks {
            let back = if scratch_pool() {
                spare.give(chunk, size, most)
            } else {
                Some(chunk)
            };
            match back {
                None => {
                    ALIVE[3].fetch_add(1, Ordering::Relaxed);
                    ALIVE[4].fetch_add(size, Ordering::Relaxed);
                }
                Some(chunk) => {
                    ALIVE[1].fetch_sub(1, Ordering::Relaxed);
                    ALIVE[2].fetch_sub(size, Ordering::Relaxed);
                    freed.push(chunk);
                }
            }
        }
    }
    // Freed outside the lock.
    drop(freed);
}

/// What the host keeps for copies in the card's own memory
/// ([`crate::shadow::cards`]): three lists of ours and the compare.
struct CardWork {
    /// Before a submission's lists: what changed on the copies' CPU side
    /// goes to their card side. Those not in use: a plan takes one and
    /// gives it back when its submission has run (`Host::card_done`), so
    /// that submissions sent ahead ([`late`]) each have their own.
    sync: Vec<Prelude>,
    /// After them, in the same submission: the compare of the declared
    /// ranges with their bytes from before. As `sync`.
    check: Vec<Prelude>,
    /// After it ran: the pieces that differ come down.
    fetch: Prelude,
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    /// One dword per piece compared, set where the piece differs: PC
    /// memory, mapped (buffer, mapping, bytes).
    differs: (ID3D12Resource, usize, u64),
    /// Where the pieces that differ are fetched to: PC memory, mapped.
    fetched: Option<(ID3D12Resource, usize, u64)>,
    /// The dwords of `differs` taken by plans whose results were not read
    /// yet.
    taken: u32,
    /// Plans made and not done with (`Host::card_done`).
    out: u32,
    /// The fetch inside flights ([`inflight_fetch`]), if it is on and was
    /// made.
    inflight: Option<CardInflight>,
}

/// What the fetch inside flights needs ([`inflight_fetch`], [`CARD_FETCH`]).
struct CardInflight {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    /// PC memory, mapped (buffer, mapping, bytes): dword 0 the counter of
    /// tickets, then one dword per dword of `CardWork::differs`, the entry
    /// of that piece.
    table: (ID3D12Resource, usize, u64),
    /// PC memory, mapped (buffer, mapping, slots of [`INFLIGHT_SLOT`]
    /// bytes): where the pieces come down.
    fetched: (ID3D12Resource, usize, u32),
    /// Every ticket below it belongs to a landed flight: its slot may be
    /// written over. Raised by the thread that lands flights; read when a
    /// compare's list is recorded (an older value only gives fewer slots).
    landed: u32,
    /// What fetches the old way brought (copy, offsets, and the highest
    /// send whose lists were surely on the queue ahead of the fetch: its
    /// order of the queue less one). A flight sent at or before that ran
    /// ahead of the fetch, so the CPU's side holds those bytes as the card
    /// had them after the flight, or newer: the flight's own slot is older
    /// there and is not merged there. Dropped once every flight sent up to
    /// it has landed.
    brought: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize, u64)>,
}

// SAFETY: D3D12 objects (free-threaded) and persistent mappings, used only
// under the submission lock.
unsafe impl Send for CardWork {}

static CARD_WORK: Mutex<Option<CardWork>> = Mutex::new(None);

/// The compare of a declared range of a copy in the card's memory with
/// its bytes from before, one thread per 64 KiB piece (cut at multiples of
/// 64 KiB of title memory, as `Shadow::card_piece` cuts them): `differs`
/// gets a dword per piece, 1 where a byte differs. The offsets are widened
/// to 16 bytes; buffers are multiples of 64 KiB, and a piece flagged for a
/// byte just outside the range costs a fetch that finds nothing.
pub(crate) const CARD_COMPARE: &str = r#"
#define RS "RootConstants(num32BitConstants=6, b0), SRV(t0), UAV(u0), UAV(u1)"
cbuffer Span : register(b0) {
    uint first;    // the first piece
    uint skew;     // where the copy starts in its piece 0
    uint size;     // the copy's buffer, in bytes (a multiple of 64 KiB)
    uint pieces;
    uint out_at;   // the first piece's dword in `differs`, in bytes
    uint compare;  // 0: the hashes are stored; else compared with those stored (2: and then stored)
};
ByteAddressBuffer now : register(t0);
// 16 bytes per piece of the copy: the hash of its bytes when they were
// last sent up (or hashed whole, the first time).
RWByteAddressBuffer hashes : register(u0);
RWByteAddressBuffer differs : register(u1);
#ifndef CARD_COMPARE_WIDE
#define CARD_COMPARE_WIDE 0
#endif
#if CARD_COMPARE_WIDE
// fx #889 (Demon12377): a group of 64 threads per piece, each hashing every
// 64th 16-byte step, the 64 hashes then folded in order (was one thread
// walking 4,096 steps one after another). Other hash values than one
// thread's; the same within a run.
groupshared uint4 accum[64];
#endif
[RootSignature(RS)]
[numthreads(64, 1, 1)]
#if CARD_COMPARE_WIDE
void main(uint3 group : SV_GroupID, uint t : SV_GroupIndex) {
    uint idx = group.x;
#else
void main(uint3 id : SV_DispatchThreadID) {
    uint idx = id.x;
#endif
    if (idx >= pieces) return;
    uint piece = first + idx;
    uint lo = piece * 65536u;
    lo = lo > skew ? lo - skew : 0u;
    uint hi = min((piece + 1u) * 65536u - skew, size);
    lo = lo & ~15u;
    hi = (hi + 15u) & ~15u;
#if CARD_COMPARE_WIDE
    uint4 h = uint4(0x9E3779B9u, 0x85EBCA6Bu, 0xC2B2AE35u, 0x27D4EB2Fu)
        ^ (t * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du));
    [loop]
    for (uint at = lo + t * 16u; at < hi; at += 1024u) {
        h = (h ^ now.Load4(at)) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
    }
    accum[t] = h;
    GroupMemoryBarrierWithGroupSync();
    [unroll]
    for (uint stride = 32u; stride > 0u; stride >>= 1u) {
        if (t < stride) {
            uint4 b = accum[t + stride];
            uint4 a = (accum[t] ^ (b + uint4(0x9E3779B9u, 0x85EBCA6Bu, 0xC2B2AE35u, 0x27D4EB2Fu)))
                * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
            accum[t] = a.yzwx ^ (a >> 15);
        }
        GroupMemoryBarrierWithGroupSync();
    }
    if (t != 0u) return;
    h = accum[0];
#else
    uint4 h = uint4(0x9E3779B9u, 0x85EBCA6Bu, 0xC2B2AE35u, 0x27D4EB2Fu);
    [loop]
    for (uint at = lo; at < hi; at += 16u) {
        h = (h ^ now.Load4(at)) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
    }
#endif
    uint id_x = idx;
    if (compare != 0u) {
        differs.Store(out_at + id_x * 4u, any(h != hashes.Load4(piece * 16u)) ? 1u : 0u);
        // With the copy-back on its own thread: the next compare of the
        // piece finds what is written from here on.
        if (compare == 2u) hashes.Store4(piece * 16u, h);
    } else {
        hashes.Store4(piece * 16u, h);
    }
}
"#;

/// Whether [`CARD_COMPARE`] hashes a piece with a group of 64 threads
/// (`VICTORY_CARD_COMPARE_WIDE=1`; off by default): fx #889's coalesced
/// compare. One thread walked a piece's 4,096 16-byte steps one after the
/// other, so a pass took the time of one piece however few it had.
pub(crate) fn compare_wide() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_COMPARE_WIDE").is_some_and(|v| v == "1"))
}

/// Records [`CARD_COMPARE`] for `pieces` pieces with the six constants
/// `constants` (its pieces word, `[3]`, and `out_at`, `[4]`, cut per pass).
///
/// # Safety
/// An open list with the compare's root signature, pipeline and views set.
unsafe fn dispatch_card_compare(list: &ID3D12GraphicsCommandList, constants: [u32; 6]) {
    let pieces = constants[3];
    // SAFETY: as the caller promises.
    unsafe {
        if !compare_wide() {
            list.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
            list.Dispatch(pieces.div_ceil(64), 1, 1);
            return;
        }
        // A group per piece: at most 65,535 a dispatch.
        let mut done = 0u32;
        while done < pieces {
            let batch = (pieces - done).min(65535);
            let c = [constants[0] + done, constants[1], constants[2], batch, constants[4] + done * 4, constants[5]];
            list.SetComputeRoot32BitConstants(0, 6, c.as_ptr().cast(), 0);
            list.Dispatch(batch, 1, 1);
            done += batch;
        }
    }
}

/// Whether the pieces a flight's compare flags come down to the PC inside
/// the flight itself (`VICTORY_CARD_FETCH_INFLIGHT=1`; off by default, and
/// only with [`relay`]).
///
/// With the relay the thread that lands flights fetched the flagged pieces
/// with a list of its own on the host's queue, the one the submitting
/// thread keeps filling: the fetch ran behind every flight sent meanwhile.
/// Measured on an RTX 3070 Laptop: 140.9 ms a frame of that thread waiting
/// for fetches, for 63 pieces (3.2 MB) in about five fetches, some 28 ms a
/// fetch spent queued behind newer flights; and the submitting thread
/// waited 81.7 ms a frame in "title memory read" settles for that thread.
///
/// With the switch the compare's list does the fetch: right behind the
/// compare, [`CARD_FETCH`] copies each piece it flagged into a slot of a
/// buffer in the PC's memory ([`CardInflight`]), so when the flight's fence
/// is reached its bytes are there, and the landing merges them
/// (`Shadow::card_merge_relay`) with no list, signal or wait of its own.
/// The merge's "fetched after" is the flight's own send number
/// (`CardPlan::sent_as`): see [`Host::relay_card_back`] for why that is
/// exact and when a landing falls back to the old fetch.
fn inflight_fetch() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| relay() && std::env::var_os("VICTORY_CARD_FETCH_INFLIGHT").is_some_and(|v| v == "1"))
}

/// The bytes of the PC's memory pieces fetched inside flights go to
/// ([`inflight_fetch`]; `VICTORY_CARD_FETCH_INFLIGHT_MB`, 64): 1,024 slots
/// ([`inflight_slots`]). A flight whose flagged pieces find no free slot
/// lands the old way.
fn inflight_fetch_bytes() -> u64 {
    static MOST: OnceLock<u64> = OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("VICTORY_CARD_FETCH_INFLIGHT_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(64).max(1) << 20
    })
}

/// A slot of [`CARD_FETCH`]: a 64 KiB piece as the compare cuts it, widened
/// to 16 bytes at both ends (one more 16-byte step where the copy does not
/// start at a multiple of 16 in its piece).
pub(crate) const INFLIGHT_SLOT: u64 = (64 << 10) + 16;

/// The slots for `bytes` asked of [`inflight_fetch_bytes`]: one per 64 KiB,
/// rounded down to a power of two (at least 1, at most 2^15: the shader's
/// byte offsets into the slots are 32-bit), so that
/// `ticket % slots` runs on without a jump when the 32-bit ticket wraps
/// (2^32 is a multiple of it; with 1,023 slots it would jump by 4 and two
/// tickets in the air could share a slot). The buffer is `slots` times
/// [`INFLIGHT_SLOT`], a little over `bytes`.
pub(crate) fn inflight_slots(bytes: u64) -> u32 {
    let wanted = (bytes >> 16).clamp(1, 1 << 15);
    1u32 << (63 - wanted.leading_zeros())
}

/// [`CARD_FETCH`]'s table entry for a piece the compare did not flag.
pub(crate) const INFLIGHT_NONE: u32 = 0xffff_ffff;

/// Where piece `i` of `span` starts in its [`CARD_FETCH`] slot, as an
/// offset of the copy: the compare's lower bound of the piece, widened down
/// to 16 bytes (`CARD_COMPARE`, before clamping to the declared range).
/// What `Shadow::card_piece` gives for the piece lies at or above it, and
/// within [`INFLIGHT_SLOT`] of it.
pub(crate) fn inflight_piece_from(span: &crate::shadow::CardSpan, i: u32) -> usize {
    let lo = (span.first + i) as usize * 65536;
    let lo = lo.saturating_sub(span.skew as usize);
    lo & !15
}

/// Whether the ticket `k` of [`CARD_FETCH`]'s counter gets a slot, with
/// every ticket below `floor` landed and `slots` slots: the slot is
/// `k % slots`. The tickets of flights still in the air are at or above
/// `floor` (they are taken in the queue's order and flights land in that
/// order), so two that get a slot are fewer than `slots` apart and never
/// share one. The arithmetic of the shader, for the tests.
pub(crate) fn inflight_gets_slot(k: u32, floor: u32, slots: u32) -> bool {
    k.wrapping_sub(floor) < slots
}

/// The fetch inside a flight ([`inflight_fetch`]), run in the compare's list
/// right behind the compare of a declared range (a UAV barrier between):
/// one group per piece (looping where a range has more than 65,535); where
/// `differs` flags the piece, its first thread takes a ticket of the
/// counter (`table`'s dword 0) and, if [`inflight_gets_slot`], the group
/// copies the piece, cut and widened as the compare hashes it, into slot
/// `ticket % slots` of `fetched`. Every piece's entry in `table` (dword 1 +
/// its dword in `differs`) is written: the ticket, also one that got no
/// slot (the landing repeats the test with the plan's `landed`), or
/// [`INFLIGHT_NONE`].
pub(crate) const CARD_FETCH: &str = r#"
#define RS "RootConstants(num32BitConstants=8, b0), SRV(t0), UAV(u0), UAV(u1), UAV(u2)"
#define SLOT 65552u
#define NONE 0xffffffffu
cbuffer Span : register(b0) {
    uint first;    // the first piece
    uint skew;     // where the copy starts in its piece 0
    uint size;     // the copy's buffer, in bytes (a multiple of 64 KiB)
    uint pieces;
    uint out_at;   // the first piece's dword in `differs`, in bytes
    uint landed;   // every ticket below it is landed
    uint slots;    // slots of `fetched`
    uint groups;   // groups dispatched
};
ByteAddressBuffer now : register(t0);
RWByteAddressBuffer differs : register(u0);
RWByteAddressBuffer table : register(u1);
RWByteAddressBuffer fetched : register(u2);
groupshared uint ticket;
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 group : SV_GroupID, uint t : SV_GroupIndex) {
    [loop]
    for (uint i = group.x; i < pieces; i += groups) {
        if (t == 0u) {
            uint entry = NONE;
            if (differs.Load(out_at + i * 4u) != 0u) {
                uint k;
                table.InterlockedAdd(0, 1u, k);
                entry = k;
            }
            table.Store(4u + out_at + i * 4u, entry);
            ticket = entry;
        }
        GroupMemoryBarrierWithGroupSync();
        uint k = ticket;
        GroupMemoryBarrierWithGroupSync();
        if (k != NONE && (k - landed) < slots) {
            uint piece = first + i;
            uint lo = piece * 65536u;
            lo = lo > skew ? lo - skew : 0u;
            uint hi = min((piece + 1u) * 65536u - skew, size);
            lo = lo & ~15u;
            hi = (hi + 15u) & ~15u;
            uint into = (k % slots) * SLOT;
            [loop]
            for (uint at = lo + t * 16u; at < hi; at += 1024u) {
                fetched.Store4(into + (at - lo), now.Load4(at));
            }
        }
    }
}
"#;

/// Whether the pieces hashed again after ranges were sent up to a copy in
/// the card's memory are hashed in runs of pieces that touch, each piece
/// once (`VICTORY_CARD_REHASH_RUNS=1`; off by default), and not in a pass
/// per range sent up.
///
/// These passes are ours, they run on the card before every submission's
/// lists, and no timing so far names them: each walks its 64 KiB pieces
/// 16 bytes at a time, 4,096 steps one after the other, so a pass takes
/// the time of one piece however few its pieces are. Run 102 sent up 514
/// ranges a frame: 514 such passes. The same pieces are hashed either way;
/// a piece hashed when it need not be, or not hashed when it should, costs
/// a fetch that finds nothing and loses no shader write.
///
/// `VICTORY_CARD_REHASH_RUNS=alt`: twenty seconds with, twenty without, in
/// turn, each change said in the log: one run then measures both on the
/// same scene (the card meter's "send-and-hash lists" in the frame log).
/// Runs 105-110 found no change in the frame, but the card was not the
/// frame's limit then.
fn rehash_runs() -> bool {
    /// 0 off, 1 on, 2 in turn.
    static MODE: OnceLock<u8> = OnceLock::new();
    match *MODE.get_or_init(|| match std::env::var("VICTORY_CARD_REHASH_RUNS").as_deref() {
        Ok("1") => 1,
        Ok("alt") => 2,
        _ => 0,
    }) {
        0 => false,
        1 => true,
        _ => {
            let joined = (now_us() / 20_000_000) % 2 == 1;
            static LAST: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(2);
            if LAST.swap(u8::from(joined), Ordering::Relaxed) != u8::from(joined) {
                tracing::info!(
                    "card rehash passes from now (VICTORY_CARD_REHASH_RUNS=alt): {}",
                    if joined { "JOINED, one pass a run of touching pieces" } else { "one pass a range sent up, as usual" }
                );
            }
            joined
        }
    }
}

/// Since the last frame-rate line, of our own passes over copies in the
/// card's memory: hash passes after ranges were sent up, the pieces they
/// hashed, and compare passes after the lists.
static CARD_PASSES: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Whether what shaders wrote to copies in the card's memory is merged
/// into title memory on several threads (`VICTORY_WIDE_MERGE=1`; off by
/// default). The pieces the card's compare found differing come down in
/// one fetch and were then merged one after the other on the thread that
/// settles: about 630 pieces and 40 MB a frame (run 104), each compared
/// with the copy's CPU side byte by byte and put into title memory through
/// the kernel. A piece is its own bytes of its copy and of title memory,
/// so eight threads take an eighth of them each.
fn wide_merge() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WIDE_MERGE").is_some_and(|v| v == "1"))
}

/// One declared range of a copy in the card's memory, in a submission.
struct CardRange {
    shadow: std::sync::Arc<crate::shadow::Shadow>,
    /// As declared (title addresses), for the statistics.
    declared: (usize, usize),
    span: crate::shadow::CardSpan,
    /// Its first piece's dword in the compare's results.
    at: u32,
    /// Whether the card compares it; else every piece is fetched.
    compared: bool,
}

/// What a submission does for copies in the card's memory.
struct CardPlan {
    ranges: Vec<CardRange>,
    /// The compare, run after the submission's lists.
    check: Option<ID3D12CommandList>,
    /// The copies whose card side was brought up to date, alive until the
    /// submission has run.
    _synced: Vec<std::sync::Arc<crate::shadow::Shadow>>,
    /// The lists of ours the plan recorded (whether the compare, the
    /// list), given back by `Host::card_done`.
    lists: Vec<(bool, Prelude)>,
    /// With [`relay`], the number of this plan's send of changed ranges
    /// (`QUEUE_ORDER`); else 0.
    sent_as: u64,
    /// Whether the compare's list fetches the flagged pieces itself
    /// ([`inflight_fetch`]): every compared range has its [`CARD_FETCH`].
    inflight: bool,
    /// The `CardInflight::landed` its [`CARD_FETCH`] was told: which of its
    /// tickets got a slot ([`inflight_gets_slot`]).
    inflight_landed: u32,
}

/// One piece of submission-time work, in recorded order.
enum Step {
    /// A copy into title memory (destination, source, bytes): the title's
    /// buffer copies, which later refreshes must see.
    Copy(usize, usize, usize),
    /// A copy of title memory into the list's own upload memory.
    Fill(usize, usize, usize),
    /// Brings a range (address, bytes) of a buffer's host copy up to date:
    /// once per submission, whichever lists ask, until a `Copy` or `Run`
    /// intervenes.
    Refresh(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Work that may write title memory; if known, the only title memory
    /// it reads and writes: (destination, source, bytes)
    /// ([`crate::memcopy::known_writes`]).
    Run(Box<dyn Fn() + Send>, Option<(usize, usize, usize)>),
    /// Work that writes host memory only.
    Host(Box<dyn Fn() + Send>),
    /// Shaders write a range (address, bytes) of a buffer's host copy.
    GpuWrite(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Brings a texture's staging buffer up to date: once per submission,
    /// whichever lists sample it, until a `Copy` or `Run` intervenes.
    Texture(std::sync::Arc<crate::texture::Staging>),
}

struct BuilderReport {
    shadow: std::sync::Arc<crate::shadow::Shadow>,
    address: u64,
    len: u64,
    after_step: usize,
}

/// What a list does on the CPU when it is submitted: copies between host
/// memory regions (title buffers, upload buffers and buffer copies), and
/// the upload buffers its GPU commands read, kept until the list is reset.
#[derive(Default)]
struct Deferred {
    builder_uploads: Vec<crate::memory::BuilderUpload>,
    builder_reports: Vec<BuilderReport>,
    raw_copies: [usize; 3],
    /// Destination, source, bytes.
    copies: Vec<Step>,
    uploads: Vec<ID3D12Resource>,
    /// The bytes of `uploads` (counted in [`ALIVE`] while they are held).
    upload_bytes: u64,
    /// Host objects the list's commands name (pipelines, root signatures,
    /// descriptor heaps): the title may release its objects before it
    /// submits the list.
    kept: Vec<windows::core::IUnknown>,
    /// The pipelines [`HostList::bind_pipeline`] kept, by address.
    bound: std::collections::HashSet<usize>,
    /// Buffer ranges refreshed since the last title copy (host copy,
    /// address, bytes): refreshing them again would find the same bytes.
    refreshed: std::collections::HashSet<(usize, usize, usize)>,
    /// Buffer ranges declared written by shaders since the last title copy
    /// (host copy, address, bytes): declaring them again would change
    /// nothing (`VICTORY_GPU_WRITE_ONCE`).
    gpu_written: std::collections::HashSet<(usize, usize, usize)>,
    /// Scratch memory [`HostList::fill_scratch`] fills since the list last
    /// wrote title memory (source, bytes): its host address.
    filled: std::collections::HashMap<(usize, usize), u64>,
    /// Textures whose contents the list brings up to date.
    textures: std::collections::HashSet<usize>,
    /// Bottom-of-pipe writes, done once the list has run: address, what,
    /// bytes.
    writes: Vec<(usize, crate::queue::Written, usize)>,
    /// Textures read back (texture to buffer copies), done once the list
    /// has run: the host buffer the list copies the texels into, the title
    /// memory they go to, the bytes of a row, the rows, the title's row
    /// pitch.
    backs: Vec<(ID3D12Resource, usize, [usize; 3])>,
    /// Whether the indirect-work budget is zeroed for this recording (a
    /// step on the CPU, or in the list for the card's copy).
    budget_zeroed: bool,
    /// Ranges (host copy, from, to) the `VICTORY_NARROW_*` switches kept
    /// from being declared written: counted, nothing else.
    narrowed: Vec<(usize, usize, usize)>,
    /// The indirect scratch buffer (`HostList::indirect_args`, by its
    /// interface pointer) that a clamp in this recording left readable as
    /// indirect arguments; 0 if none. It stays readable until the next
    /// clamp into it ([`HostList::indirect_state`]).
    indirect_readable: usize,
    /// `VICTORY_BVH_QUARANTINE` (fx #935): the list records a stage of the
    /// structure builder, so nothing its shaders write is copied back into
    /// title memory. Copy-backs are declared per list, not per dispatch:
    /// another shader's declaration in the same list covered the builder's
    /// stores too (and with `VICTORY_COPY_BACK_ONLY_READBACK` made them
    /// owned, to be put into title memory on a later read), and quarantine
    /// leaked.
    quarantined: bool,
    /// With `VICTORY_INDIRECT_KEEP=1`: scratch buffers of clamped indirect
    /// arguments left in `INDIRECT_ARGUMENT` after their executions (by
    /// address), brought back to `COMMON` only when a clamp writes them
    /// again (`crate::indirect`).
    indirect_kept: std::collections::HashSet<usize>,
}

impl Deferred {
    /// Holds `buffer` (`size` bytes of upload memory) until the list is
    /// reset.
    fn hold_upload(&mut self, buffer: ID3D12Resource, size: u64) {
        ALIVE[6].fetch_add(1, Ordering::Relaxed);
        ALIVE[7].fetch_add(size, Ordering::Relaxed);
        self.upload_bytes += size;
        self.uploads.push(buffer);
    }

    /// As `*self = Deferred::default()`, keeping the memory of the step
    /// list and the sets (`VICTORY_PREP_REUSE`) unless one grew past
    /// [`KEEP_STEPS`] entries.
    fn clear_keeping(&mut self) {
        const KEEP_STEPS: usize = 1 << 16;
        ALIVE[6].fetch_sub(self.uploads.len() as u64, Ordering::Relaxed);
        ALIVE[7].fetch_sub(self.upload_bytes, Ordering::Relaxed);
        self.upload_bytes = 0;
        self.budget_zeroed = false;
        self.builder_uploads.clear();
        self.builder_reports.clear();
        self.raw_copies = [0; 3];
        // Dropped in the same order as a whole `Deferred` drops its fields.
        if self.copies.capacity() > KEEP_STEPS {
            self.copies = Vec::new();
        } else {
            self.copies.clear();
        }
        self.uploads.clear();
        self.kept.clear();
        self.bound.clear();
        if self.refreshed.capacity() > KEEP_STEPS {
            self.refreshed = std::collections::HashSet::new();
        } else {
            self.refreshed.clear();
        }
        if self.gpu_written.capacity() > KEEP_STEPS {
            self.gpu_written = std::collections::HashSet::new();
        } else {
            self.gpu_written.clear();
        }
        self.filled.clear();
        if self.textures.capacity() > KEEP_STEPS {
            self.textures = std::collections::HashSet::new();
        } else {
            self.textures.clear();
        }
        self.writes.clear();
        self.backs.clear();
        self.quarantined = false;
    }
}

impl Drop for Deferred {
    fn drop(&mut self) {
        ALIVE[6].fetch_sub(self.uploads.len() as u64, Ordering::Relaxed);
        ALIVE[7].fetch_sub(self.upload_bytes, Ordering::Relaxed);
    }
}

/// Whether a queue's submissions go to the card without a wait for each
/// (`VICTORY_LATE_WAIT=1`; off by default, and off in the timing run, which
/// waits for every list on its own).
///
/// Every submission was prepared, sent, waited for and copied back before
/// the next one was looked at, so the card stood idle while we prepared
/// and we stood idle while it ran: in run 86 a frame of 446 ms is 134 ms of
/// preparing, 194 ms of waiting and 78 ms of copying back, one after the
/// other, on the one thread that submits.
///
/// With the switch a queue's submission is sent and joins a line of
/// "flights" ([`Flight`]); the thread goes on to the next one while the
/// card runs. The flights are settled together ([`Host::settle`]: one wait
/// for the card, what their shaders wrote copied back once per byte, then
/// their bottom-of-pipe writes and the fence signals behind them, in
/// order) at the next point where the title, or our own preparing, can
/// observe what they do. Those points, exactly:
///
/// 1. a synchronous submission of ours ([`Host::execute`]): a present, a
///    read-back or map of a texture, a clear or upload run at once;
/// 2. the next submission reads title memory that a flight's shaders may
///    write: a title buffer copy, a fill of upload memory, a texture's
///    memory, any step that runs code of ours on title memory
///    ([`late_blocked`]);
/// 3. the next submission refreshes a range of a host copy that a flight's
///    shaders may write, and the title wrote a page of it since it was last
///    copied ([`stale_under_flights`]): the refresh leaves such a range
///    alone until the flight's writes are back, as it always did within a
///    submission;
/// 4. a queue stands at a wait for a memory value that no flight's
///    bottom-of-pipe write is known to satisfy, at an address a flight may
///    write ([`late_met`]);
/// 5. the title blocks on a fence without an event;
/// 6. the card has run every flight (a thread of ours waits for that:
///    [`late_helper`]), or the flights hold [`late_most`] host lists.
///
/// What the title is told (bottom-of-pipe writes, fence values) is told
/// when the card has really run the work, later than before and never
/// earlier; a fence signalled on a queue while flights are in the air is
/// set behind them. A wait for a memory value that a flight's
/// bottom-of-pipe write satisfies counts as met: the waiting part goes to
/// the card behind that flight, which is the order the wait asks for.
///
/// What it assumes beyond today: memory the title rewrites between two of
/// its submissions, without waiting for the first, may be seen by the
/// first's shaders (as on the console, where the card reads title memory
/// when it runs, not when the list is submitted).
pub(crate) fn late() -> bool {
    late_stage() > 0
}

/// `VICTORY_SYNC_PRECISE=1` (off by default): a submission sent ahead
/// waits only for what it depends on.
///
/// - A texture's staging buffer is not a reason to wait before preparing:
///   its write waits, on the card's fence, for the last submission that
///   copies from it (`crate::texture`'s readers), and only if the texture
///   changed. Before, any staging buffer an unlanded flight had named made
///   the submission wait until every flight had landed.
/// - Title memory a flight's shaders may write and the submission reads
///   (a buffer copy, a fill, another copy's refresh) is waited for until
///   the flights that declared it have landed, as `VICTORY_RELAY=2` does,
///   also with `VICTORY_RELAY=1`.
/// - After its waits a submission looks again: if what it reads is still
///   written by a flight, that is logged once as a hazard (a wrong
///   dependency model shows up as a line, not as a corrupt frame).
pub(crate) fn sync_precise() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_SYNC_PRECISE").is_some_and(|v| v == "1");
        if on {
            tracing::info!("submissions wait only for the submissions they depend on (VICTORY_SYNC_PRECISE=1)");
        }
        on
    })
}

/// `VICTORY_BARRIER_MERGE=1` (off by default): fewer waits for every
/// earlier shader write in a list.
///
/// Before, every title barrier that named a buffer added a barrier on all
/// resources (the host cannot name a buffer's host copy: one copy holds
/// many title buffers, and a title buffer may have more than one), each
/// barrier group was one, and each indirect execution added two more around
/// the pass of ours that clamps its arguments: some 9,000 full drains of
/// the card a frame in the gameplay scene, most between draws that write no
/// unordered access at all.
///
/// With the switch a list notes whether work recorded since its last
/// barrier on all resources may write through unordered access (a
/// dispatch or draw whose shaders can store to a buffer or a texture,
/// `Translated::writes`; our own upscale stretch). Without such writes a
/// barrier on all resources, the title's or ours, is left out: there is
/// nothing it would wait for. With them one call keeps at most one, which
/// covers its barriers on single resources; transitions of one subresource
/// within the call fold into one (`victory_core::sync::plan`). The pass
/// that clamps indirect arguments waits only for pending writes, and no
/// longer after itself: its output is ordered by its transition to the
/// argument state, and its budget word only takes atomics.
pub(crate) fn barrier_merge() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_BARRIER_MERGE").is_some_and(|v| v == "1");
        if on {
            tracing::info!("barriers on all resources only where unordered writes are pending (VICTORY_BARRIER_MERGE=1)");
        }
        on
    })
}

/// Since the last frame-rate line (`VICTORY_BARRIER_MERGE`): barriers on
/// all resources recorded for the title's barriers and groups, left out
/// there, recorded around the clamp of indirect arguments, left out there,
/// and transitions folded away.
pub(crate) static BARRIER_STATS: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// A `VICTORY_PREP_*` switch: on only with the value 1 (off by default),
/// said once in the log.
fn prep_switch(cell: &'static OnceLock<bool>, name: &str, what: &str) -> bool {
    *cell.get_or_init(|| {
        let on = std::env::var_os(name).is_some_and(|v| v == "1");
        if on {
            tracing::info!("{what} ({name}=1)");
        }
        on
    })
}

/// A live switch (`victory_core::live`) on when not 0; said once in the log
/// the first time it is seen on.
fn live_said(sw: &'static victory_core::live::Switch, what: &str) -> bool {
    let on = sw.on();
    if on {
        static SAID: Mutex<Vec<usize>> = Mutex::new(Vec::new());
        let key = std::ptr::from_ref(sw) as usize;
        let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
        if !said.contains(&key) {
            said.push(key);
            tracing::info!("{what} (VICTORY_{}=1)", victory_core::live::short(sw.name()));
        }
    }
    on
}

/// `VICTORY_PREP_COPY_FLUSH=1` (off by default): a title buffer copy done
/// at submission ([`Step::Copy`]) first finishes only the pending refreshes
/// of host copies whose ranges share a 4 KiB page with the bytes it
/// writes, not all of them.
///
/// Before, each of the ~6,500-14,000 copies a frame ended the refreshes
/// gathered so far: a flush (with its write-watch asking for the ranges)
/// for every copy between two lists' refreshes. A refresh of other pages
/// reads the same bytes before the copy as after it, and the write watch
/// counts pages, so a refresh that shares no page with the copy's
/// destination is left pending; one that does is finished first, as
/// before. Textures are checked again after each copy, as before (their
/// threads are waited for).
pub(crate) fn prep_copy_flush() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    prep_switch(&ON, "VICTORY_PREP_COPY_FLUSH", "a title copy finishes only the refreshes of the pages it writes")
}

/// `VICTORY_PREP_COPY_DIRECT=1` (off by default): a title buffer copy of at
/// most 1 MiB whose source and destination the runtime's mapping record
/// (`crate::regions`) says are all mapped is a plain memory move, with the
/// mappings of both held still while it runs, instead of a kernel read
/// (`ReadProcessMemory`, some microseconds a call for ~480 bytes).
/// Memory the record does not hold, or a mapping being changed, goes
/// through the kernel as before.
pub(crate) fn prep_copy_direct() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    prep_switch(&ON, "VICTORY_PREP_COPY_DIRECT", "title copies between mapped memory are plain moves")
}

/// `VICTORY_PREP_BARRIERS=1` (off by default): barriers recorded with no
/// other command between them (the title's barrier calls, our transitions
/// before a draw, copy or dispatch) go to the host list as one
/// `ResourceBarrier` call, recorded before the list's next other command
/// (every title call but a barrier first records them, as does the
/// list's close). Transitions that change nothing and waits for unordered
/// access another one in the run covers are left out; `A -> B -> C` of one
/// subresource through a read-only `B` becomes `A -> C`
/// (`victory_core::prep::coalesce`).
pub(crate) fn prep_barriers() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    prep_switch(&ON, "VICTORY_PREP_BARRIERS", "barriers with nothing between them are one call")
}

/// `VICTORY_PREP_TABLES=1` (off by default): a draw or dispatch does not
/// set a descriptor table the list already has at that root parameter
/// since the root signature was last set (`victory_core::prep::TableCache`).
pub(crate) fn prep_tables() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    prep_switch(&ON, "VICTORY_PREP_TABLES", "descriptor tables set again only when they change")
}

/// `VICTORY_PREP_REUSE=1` (off by default): a list's reset keeps the
/// capacity of its submission-time step lists and sets instead of
/// dropping them (each list grows them again every frame), and a deferred
/// step takes the list's lock once.
pub(crate) fn prep_reuse() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    prep_switch(&ON, "VICTORY_PREP_REUSE", "lists keep their step memory across resets")
}

/// `VICTORY_GPU_WRITE_ONCE=1` (off by default): a list declares a range
/// its shaders write once until the title next writes memory (a title
/// copy or a deferred run), not once per draw or dispatch. Each repeat
/// was a [`Step::GpuWrite`]: a flush of the copy's pending refreshes and
/// a new snapshot of the same range. With a refresh of that range between
/// two declarations the snapshot is taken earlier than before, so this is
/// behind a switch. (Belobug PR #29, romainhedouin dd799aa.)
///
/// `VICTORY_DECLARE_ONCE=1` (off by default; live) is the same, under the
/// name avatarmanz/VIctory-7900XTX b32fb997 gives it (on by default there):
/// `gpu_written` is cleared at every title write and list reset whether
/// either is on or not, so flipping it mid-run keeps no stale range.
pub(crate) fn gpu_write_once() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    static LIVE: victory_core::live::Switch = victory_core::live::Switch::new("DECLARE_ONCE", 0);
    prep_switch(&ON, "VICTORY_GPU_WRITE_ONCE", "a list declares each shader-written range once")
        || live_said(&LIVE, "a list declares each shader-written range once until it next writes title memory")
}

/// `VICTORY_STATE_DEDUP=1` (off by default): a draw or dispatch does not
/// set root 32-bit constants or a root view the list already has at that
/// root parameter (the same words, the same address) since the root
/// signature was last set (`victory_core::prep::RootArgCache`). Pipelines,
/// root signatures, heaps and tables were deduplicated already (by host
/// object, and with `VICTORY_PREP_TABLES`).
pub(crate) fn state_dedup() -> bool {
    // Live (`victory_core::live`, `VICTORY_AB`): a list's cache is cleared
    // when the switch comes on after being off (`draw::State::host_args_stale`).
    static ON: victory_core::live::Switch = victory_core::live::Switch::new("STATE_DEDUP", 0);
    live_said(&ON, "root constants and root views set again only when they change")
}

/// `VICTORY_BIND_CACHE=1` (off by default; live): a list binds a pipeline
/// it bound already without keeping and naming it again, sets the same
/// render targets again only when they change (`draw::State::set_targets`),
/// and copies an unaligned root constant buffer into scratch memory once
/// until it next writes title memory ([`HostList::fill_scratch`]).
/// (avatarmanz/VIctory-7900XTX ab86ef7f, ca1d519f, c96525db.)
pub(crate) fn bind_cache() -> bool {
    // Live: what each cache keeps is kept up to date (or cleared at title
    // writes and list resets) whether it is on or not.
    static ON: victory_core::live::Switch = victory_core::live::Switch::new("BIND_CACHE", 0);
    live_said(&ON, "same pipelines, render targets and unaligned root constant buffers bound once")
}

/// `VICTORY_KICK_MERGE=1` (off by default): the title's submissions sent
/// ahead that run at once as a whole are held and sent as one host
/// submission with the next ones of the same queue, up to
/// `VICTORY_KICK_MOST` (4) of them; anything that orders against the card
/// sends them first (`crate::queue::kick_flush`).
/// [`kick_merge`] was on at some point: kicks may be held.
pub(crate) static KICK_EVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn kick_merge() -> bool {
    // Live (`victory_core::live`, `VICTORY_AB`): held kicks are flushed
    // before a change (`crate::ab`), and `kick_flush` sends what is held
    // whatever the switch says.
    static ON: victory_core::live::Switch = victory_core::live::Switch::new("KICK_MERGE", 0);
    let on = live_said(&ON, "the title's early submissions joined into fewer host submissions");
    if on {
        KICK_EVER.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    on
}

/// The most title submissions [`kick_merge`] joins into one (`VICTORY_KICK_MOST`, 4).
pub(crate) fn kick_most() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_KICK_MOST").ok().and_then(|v| v.parse().ok()).filter(|&n| n >= 1).unwrap_or(4)
    })
}

/// `VICTORY_PREP_PROFILE=1` (off by default): the preparing of a
/// submission timed by part, and the binding of draws and dispatches, on a
/// line of its own with the frame-rate line ([`PROF`]).
pub(crate) fn prep_profile() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    prep_switch(&ON, "VICTORY_PREP_PROFILE", "preparing timed by part")
}

/// [`prep_profile`]'s sums since the last frame-rate line, microseconds
/// unless named a count.
#[derive(Clone, Copy)]
pub(crate) enum Prof {
    /// Before preparing counts: settles, heap sync, sweep, waiting slots.
    Before,
    /// The write-watch asking for the submission (and its stale settle).
    Ask,
    /// The lists' deferred steps (copies and textures among them).
    Deferred,
    /// The batch's last flush and texture refreshes.
    Flush,
    /// Snapshots of the ranges shaders may write.
    Snapshots,
    /// Copies in the card's memory: what goes up, the compare.
    Card,
    /// Copies served on the card (`VICTORY_READFIX`).
    Readfix,
    /// After preparing: the bookkeeping of a flight sent ahead.
    After,
    /// Submissions (count) and lists in them (count).
    Submissions,
    Lists,
    /// Binding draws and dispatches on the title's threads (nanoseconds),
    /// and binds (count).
    BindNs,
    Binds,
    /// `VICTORY_STATE_DEDUP`: root constants and views asked and recorded (counts).
    RootIn,
    RootOut,
    /// `VICTORY_KICK_MERGE`: title submissions joined, host submissions made (counts).
    KicksIn,
    KicksOut,
}

pub(crate) static PROF: [std::sync::atomic::AtomicU64; Prof::KicksOut as usize + 1] =
    [const { std::sync::atomic::AtomicU64::new(0) }; Prof::KicksOut as usize + 1];

pub(crate) fn prof(p: Prof, n: u64) {
    PROF[p as usize].fetch_add(n, Ordering::Relaxed);
}

/// Adds the microseconds since `*at` to `p` and moves `*at` to now.
fn prof_since(p: Prof, at: &mut std::time::Instant) {
    let now = std::time::Instant::now();
    prof(p, (now - *at).as_micros() as u64);
    *at = now;
}

fn prof_line(frames: f64) {
    let s: Vec<f64> = PROF.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    if !prep_profile() && !state_dedup() && !kick_merge() {
        return;
    }
    let g = |p: Prof| s[p as usize];
    let ms = |p: Prof| g(p) / 1e3;
    tracing::info!(
        "prep profile per frame (VICTORY_PREP_PROFILE): {:.1} submissions of {:.1} lists; before preparing {:.1} ms;          preparing: asking (with stale settles) {:.1} ms, deferred steps {:.1} ms, last flush {:.1} ms, snapshots {:.1} ms, card copies {:.1} ms,          readfix {:.1} ms; after {:.1} ms; binding {:.0} draws/dispatches {:.1} ms on the title's threads;          STATE_DEDUP={}: root args {:.0} in -> {:.0} out; KICK_MERGE={}: title submissions {:.1} -> {:.1} host",
        g(Prof::Submissions),
        g(Prof::Lists),
        ms(Prof::Before),
        ms(Prof::Ask),
        ms(Prof::Deferred),
        ms(Prof::Flush),
        ms(Prof::Snapshots),
        ms(Prof::Card),
        ms(Prof::Readfix),
        ms(Prof::After),
        g(Prof::Binds),
        g(Prof::BindNs) / 1e6,
        if state_dedup() { "on" } else { "off" },
        g(Prof::RootIn),
        g(Prof::RootOut),
        if kick_merge() { "on" } else { "off" },
        g(Prof::KicksIn),
        g(Prof::KicksOut),
    );
}

/// Counters of the `VICTORY_PREP_*` switches, since the last frame-rate
/// line.
#[derive(Clone, Copy)]
pub(crate) enum P {
    /// Title copies done at submission; of them done as plain moves.
    Copies,
    CopiesDirect,
    /// Microseconds in title copies (the copy alone).
    CopyUs,
    /// Refresh flushes a title copy made, and those it left out.
    CopyFlushes,
    CopyFlushesSaved,
    /// Calls asking to record barriers, and `ResourceBarrier` calls made.
    BarrierCallsIn,
    BarrierCallsOut,
    /// Barriers asked for and recorded.
    BarriersIn,
    BarriersOut,
    Noops,
    UavsDropped,
    Folded,
    /// Nanoseconds in flushes of held barriers.
    BarrierNs,
    /// Descriptor table sets asked for, and recorded.
    TablesIn,
    TablesOut,
    /// List resets that kept their step memory.
    Reused,
    /// `VICTORY_COPY_PARTS`: refreshes not asked for again, a range of the
    /// copy being current since the last title write.
    PartsSkipped,
}

pub(crate) static PREP_STATS: [std::sync::atomic::AtomicU64; P::PartsSkipped as usize + 1] =
    [const { std::sync::atomic::AtomicU64::new(0) }; P::PartsSkipped as usize + 1];

/// The `VICTORY_PREP_*` line, per frame, with the frame-rate line.
fn prep_line(frames: f64) {
    let s: Vec<f64> = PREP_STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    let on = |b: bool| if b { "on" } else { "off" };
    let g = |p: P| s[p as usize];
    tracing::info!(
        "prep per frame (VICTORY_PREP_ COPY_FLUSH={} COPY_DIRECT={} BARRIERS={} TABLES={} REUSE={}): \
         title copies {:.0} ({:.0} plain moves) {:.1} ms, refresh flushes {:.0} ({:.0} saved); \
         barrier calls {:.0} in -> {:.0} out ({:.0} saved), barriers {:.0} -> {:.0} \
         ({:.0} no-op, {:.0} covered waits, {:.0} folded), {:.2} ms flushing; \
         descriptor tables {:.0} in -> {:.0} out ({:.0} saved); {:.0} list resets reused;          COPY_PARTS={}: {:.0} refreshes of current parts skipped",
        on(prep_copy_flush()),
        on(prep_copy_direct()),
        on(prep_barriers()),
        on(prep_tables()),
        on(prep_reuse()),
        g(P::Copies),
        g(P::CopiesDirect),
        g(P::CopyUs) / 1e3,
        g(P::CopyFlushes),
        g(P::CopyFlushesSaved),
        g(P::BarrierCallsIn),
        g(P::BarrierCallsOut),
        (g(P::BarrierCallsIn) - g(P::BarrierCallsOut)).max(0.0),
        g(P::BarriersIn),
        g(P::BarriersOut),
        g(P::Noops),
        g(P::UavsDropped),
        g(P::Folded),
        g(P::BarrierNs) / 1e6,
        g(P::TablesIn),
        g(P::TablesOut),
        (g(P::TablesIn) - g(P::TablesOut)).max(0.0),
        g(P::Reused),
        on(copy_parts()),
        g(P::PartsSkipped),
    );
}

/// `VICTORY_BARRIER_HAZARD=1` (off by default): a direct dispatch waits
/// for earlier shader work only where it conflicts with it.
///
/// Before, each title barrier on a buffer and each barrier group recorded a
/// barrier on all resources where it stood (with `VICTORY_BARRIER_MERGE`
/// where anything wrote since the last one), in front of most of the ~3,200
/// direct dispatches a frame in the gameplay scene; each is a full drain of
/// the card.
///
/// With the switch such a request is held (`victory_core::sync::Hazards`).
/// The next dispatch names what it reads and writes (the views its tables
/// and root views bind: title memory by address range, textures by the
/// title's object); only if that conflicts with something accessed since
/// the last barrier on all resources (a read after a write, a write after
/// either) does the barrier go in, before it. A dispatch whose accesses are
/// not all known (title memory by address, views picked by a computed
/// index, textures sharing memory) conflicts with everything pending. Any
/// other work (draws, copies, clears, indirect execution, the list's end)
/// records a held barrier first. Transitions and aliasing barriers are
/// recorded as before. A barrier naming one buffer or texture holds a
/// request for that one; with `VICTORY_BARRIER_LITERAL=1` the barrier
/// groups are read too.
pub(crate) fn barrier_hazard() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_BARRIER_HAZARD").is_some_and(|v| v == "1");
        if on {
            tracing::info!("a dispatch waits for earlier shader work only where they conflict (VICTORY_BARRIER_HAZARD=1)");
        }
        on
    })
}

/// `VICTORY_BARRIER_LITERAL=1` (off by default): the title's barrier groups
/// (enhanced barriers) are read as D3D12 lays them out. A group whose
/// barriers only order reads after reads, or have nothing before or after,
/// waits for nothing; with `VICTORY_BARRIER_HAZARD` one on buffers or
/// textures alone holds a request for those. A group with a type or bits
/// D3D12 does not know waits for everything, as before, and is counted.
pub(crate) fn barrier_literal() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_BARRIER_LITERAL").is_some_and(|v| v == "1");
        if on {
            tracing::info!("the title's barrier groups are taken literally (VICTORY_BARRIER_LITERAL=1)");
        }
        on
    })
}

/// Since the last frame-rate line (`VICTORY_BARRIER_HAZARD`,
/// `VICTORY_BARRIER_LITERAL`): requests for all earlier work, requests for
/// one resource's, barriers recorded before a dispatch, barriers recorded
/// before other work, dispatches looked at, of them with accesses not all
/// known; barrier groups that wait for nothing, barrier groups not
/// understood; microseconds recording spent collecting accesses.
pub(crate) static HAZARD_STATS: [std::sync::atomic::AtomicU64; 9] = [const { std::sync::atomic::AtomicU64::new(0) }; 9];

fn hazard_line(frames: f64) {
    if !barrier_hazard() && !barrier_literal() {
        return;
    }
    let s: Vec<f64> = HAZARD_STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    tracing::info!(
        "dispatch hazards per frame (VICTORY_BARRIER_HAZARD={}, LITERAL={}): requests {:.0} for all + {:.0} for one resource; \
         barriers recorded {:.0} before a dispatch + {:.0} before other work; dispatches {:.0}, {:.0} with unknown access; \
         groups waiting for nothing {:.0}, not understood {:.0}; collecting accesses took {:.2} ms",
        u8::from(barrier_hazard()),
        u8::from(barrier_literal()),
        s[0],
        s[1],
        s[2],
        s[3],
        s[4],
        s[5],
        s[6],
        s[7],
        s[8] / 1000.0,
    );
}

fn barrier_line(frames: f64) {
    if !barrier_merge() {
        return;
    }
    let s: Vec<f64> = BARRIER_STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    tracing::info!(
        "waits for every earlier shader write per frame (VICTORY_BARRIER_MERGE=1): the title's barriers {:.0} recorded, \
         {:.0} left out; around indirect argument clamps {:.0} recorded, {:.0} left out; {:.0} transitions folded",
        s[0],
        s[1],
        s[2],
        s[3],
        s[4],
    );
}

/// The host fence's value the card has reached (`u64::MAX` without a host
/// GPU or with the device removed, as D3D12 reports it then).
pub(crate) fn card_completed() -> u64 {
    match host() {
        // SAFETY: a plain query of our fence.
        Some(host) => unsafe { host.fence.GetCompletedValue() },
        None => u64::MAX,
    }
}

/// Blocks until the card has reached `value` on the host fence (a value
/// already signalled on the queue, which never waits on the card). Whether
/// it did.
pub(crate) fn wait_card(value: u64) -> bool {
    let Some(host) = host() else {
        return true;
    };
    // SAFETY: our fence; without an event D3D12 blocks until the value.
    unsafe {
        if host.fence.GetCompletedValue() >= value {
            return true;
        }
        let was_working = working(false);
        let waited = host.fence.SetEventOnCompletion(value, HANDLE::default()).is_ok();
        working(was_working);
        waited
    }
}

/// `VICTORY_LATE_WAIT`: 0 (off), 1, or 2 ([`late_exact`]).
/// Whether submissions are sent ahead of their landing (any late stage).
pub(crate) fn late_on() -> bool {
    late_stage() > 0
}

fn late_stage() -> u8 {
    static STAGE: OnceLock<u8> = OnceLock::new();
    *STAGE.get_or_init(|| {
        if std::env::var_os("VICTORY_GPU_TIMING").is_some() {
            return 0;
        }
        match std::env::var("VICTORY_LATE_WAIT").as_deref() {
            Ok("1") => 1,
            Ok("2") => 2,
            Ok("3") => 3,
            // The relay alone: on top of stage 3.
            Err(_) if crate::shadow::relay() => 3,
            _ => 0,
        }
    })
}

/// Stage 2 of [`late`] (`VICTORY_LATE_WAIT=2`): point 3 of its list is no
/// longer a point. Where the title wrote a page that lies in a range a
/// flight's shaders may write, the bytes the title changed are found (the
/// page as it is now against what the copy held when the flight was
/// prepared) and put into the copy, and only those: what the flight's
/// shaders write beside them stays (`Shadow::merge_under_writes`). The
/// flights are settled for it only where that cannot be done: memory that
/// is not write-watched, or a copy in the card's memory with more changed
/// runs than a submission should send up one by one.
///
/// In run 92 (stage 1) this point settled the flights five times a frame
/// of twelve, each time with the card's work still to wait for: 172 ms a
/// frame of waiting were left of 200.
///
/// What differs from settling first: a byte both the title and a flight's
/// shader wrote is the title's if the shader wrote it first, the shader's
/// if it wrote it afterwards; settled first it was always the shader's.
/// The title writing a byte a shader of its own is writing is a race on
/// the console too.
fn late_exact() -> bool {
    late_stage() > 1
}

/// Stage 3 of [`late`] (`VICTORY_LATE_WAIT=3`: stage 2, and the present is
/// no longer a point where flights are settled). The copy to the back
/// buffer is sent as a flight of its own (`Host::execute_own`, a list of
/// its own for each present in the air) and the swap chain is told to
/// present behind it; the queue's call returns, and the card finishes the
/// frame while the title records the next one. What the title is told
/// about the frame (bottom-of-pipe writes, fences) is told when the card
/// has run it, by the settling thread.
///
/// What is left out with it: the look whether the presented frame is black
/// (diagnostics; it reads the frame back at once). A frame that is saved
/// or listed is presented the old way. Once every second and a half a
/// submission settles the flights all the same, for the sweep that lets
/// idle host objects go (it needs nothing in the air).
pub(crate) fn late_free_present() -> bool {
    late_stage() > 2
}

/// The relay's first stage (`VICTORY_RELAY=1` or `2`; off by default, see `crate::shadow::relay_level`; it turns
/// [`late`] on at stage 3 unless `VICTORY_LATE_WAIT` says another stage,
/// and is off with it, in the timing run and with kept snapshots).
///
/// With [`late`] alone every settle does three things on the thread that
/// holds the submission lock: it waits for everything in the air, copies
/// back with the card standing, and only then goes on preparing. The frame
/// stays a relay: the card works about 242 ms of it and one thread
/// prepares, waits and copies back for about 220.
///
/// With the switch a thread of its own ([`relay_worker`]) lands flights as
/// soon as the card has run them, in the order they were sent and without
/// the submission lock: it fetches and merges what their shaders wrote to
/// copies in the card's memory, compares and merges the other copies,
/// frees the flights' ranges, and then does their read-backs, their
/// bottom-of-pipe writes and the fence signals behind them, in the line's
/// order. The submitting thread meanwhile prepares and sends the next
/// submissions. [`Host::settle`] no longer settles anything: it waits until
/// that thread has landed everything in the air, at the same points as
/// before (the list in [`late`]), so whoever can observe title memory, a
/// fence or a present still does so only after the flights before it are
/// merged.
///
/// What makes that safe is in `crate::shadow::relay` (ranges counted per
/// flight; a merge that never stores into memory the card may be writing;
/// sends to card copies numbered) and here: flights land in the order of
/// [`LINE`] by one thread, so a later flight's results never reach title
/// memory before an earlier flight's; the compare on the card stores the
/// hash it made, so each flight's compare flags exactly what changed since
/// the compare before it, however late the pieces are fetched; the fetch
/// goes to the queue under [`QUEUE_ORDER`], which says which sends of
/// title bytes the card had received when it ran.
pub(crate) fn relay() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| late_stage() > 0 && crate::shadow::relay())
}

/// The relay's second stage (`VICTORY_RELAY=2`): a submission that reads
/// title memory a flight's shaders may write (a title buffer copy, a fill
/// of upload memory, a texture) waits only until the flights that declared
/// that memory have landed, not for everything in the air; and at
/// [`late_most`] lists in the air it waits until there are fewer. At `1`
/// both wait for everything.
fn relay_exact() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| relay() && crate::shadow::relay_level() == 2)
}

/// Told when something joins [`LINE`], for the thread that lands flights
/// ([`relay`]); [`LINE_MOVED`] stays the word for the thread that looks at
/// what queues hold.
static RELAY_MOVED: std::sync::Condvar = std::sync::Condvar::new();

/// Told when flights have landed ([`relay`]): who waits for that looks
/// again.
static LINE_LANDED: std::sync::Condvar = std::sync::Condvar::new();

/// With [`relay`], the order of what we put on the queue: the number of
/// sends of changed ranges to copies in the card's memory so far
/// (`Host::card_before`). Held from the taking of the ranges until their
/// list is on the queue, and while a fetch of pieces goes to the queue: a
/// fetch sent with the count at N ran behind sends 1..N and ahead of every
/// later one.
static QUEUE_ORDER: Mutex<u64> = Mutex::new(0);

/// Since the last frame-rate line, of the thread that lands flights
/// ([`relay`]): landings, flights landed, microseconds it waited for the
/// card with nothing to land, microseconds it waited for fetches of card
/// copies' pieces, microseconds of landing in all.
static RELAY_STATS: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// Since the last frame-rate line, with [`inflight_fetch`]: pieces merged
/// from the slots their flights fetched them to, their bytes, landings that
/// went the old way because a plan's pieces overflowed the slots, and
/// landings that fetched the old way (for any reason, overflow too).
static INFLIGHT_STATS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// After a fetch the old way taken with the queue's count at `after`, for
/// plans whose highest send is `landed`: up to which send the notes of
/// sends to card copies may go (`crate::shadow::relay_fetched`).
///
/// Without [`inflight_fetch`] every later merge is of a later fetch, whose
/// count is `after` or more: notes up to `after` are of no use, as before.
/// With it, a flight still in the air, sent before the fetch, merges its
/// own slot with "fetched after" its own send, which may be below `after`:
/// it needs the notes of the sends between, or it takes the title's bytes
/// that went up in them for its shaders' writes and puts its older card
/// bytes over them. (Flights 5, 6, 7 in the air; 5 lands the old way with
/// the count at 7; a title byte went up in send 7 into a piece 6's shaders
/// write; 6 lands from its slot, which holds that byte from before send 7.)
/// So the mark goes only to the landed plans' highest send: every flight
/// still to land was sent after it, and every later fetch the old way
/// takes a count at least that high. (`CardInflight::brought` is another
/// matter: the card's bytes the fetch put in the CPU's side, not the
/// title's.)
pub(crate) fn fallback_fetched_mark(inflight: bool, after: u64, landed: Option<u64>) -> u64 {
    match (inflight, landed) {
        (false, _) => after,
        (true, Some(landed)) => landed.min(after),
        (true, None) => 0,
    }
}

/// The landed mark of [`CARD_FETCH`]'s tickets after ticket `k` landed: one
/// past `k` if that is ahead of `landed`, in wrapping order (tickets in the
/// air are always within 2^31 of the mark), else `landed`.
pub(crate) fn inflight_raise(landed: u32, k: u32) -> u32 {
    let next = k.wrapping_add(1);
    if (next.wrapping_sub(landed) as i32) > 0 { next } else { landed }
}

/// With [`inflight_fetch`], after a landing merged (either way): the tickets
/// these plans' fetches took are landed, and their slots may be written
/// over. Plans land in the line's order and tickets are taken in the
/// queue's (a UAV barrier opens every compare's list), so every ticket of a
/// plan in the air is ahead of these.
///
/// The counter is never reset and wraps after 2^32 tickets: the mark is
/// raised in wrapping order ([`inflight_raise`]), and the slots are a power
/// of two, so `k % slots` runs on across the wrap. shortcut: ticket
/// 0xffff_ffff reads as [`INFLIGHT_NONE`]; a flagged piece that draws it
/// copies nothing and its landing goes the old way, once every 2^32.
fn inflight_tickets_landed(plans: &[&CardPlan]) {
    let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(work) = work.as_mut() else {
        return;
    };
    let dwords = (work.differs.2 / 4) as usize;
    let Some(inflight) = work.inflight.as_mut() else {
        return;
    };
    // SAFETY: the mapping of the tickets; these plans' fetches have run.
    let table = unsafe { std::slice::from_raw_parts((inflight.table.1 as *const u32).add(1), dwords) };
    for plan in plans.iter().filter(|p| p.inflight) {
        for r in plan.ranges.iter().filter(|r| r.compared) {
            for p in 0..r.span.pieces {
                if let Some(&k) = table.get((r.at + p) as usize)
                    && k != INFLIGHT_NONE
                {
                    inflight.landed = inflight_raise(inflight.landed, k);
                }
            }
        }
    }
    // Every flight still to land was sent after these: what fetches the old
    // way brought up to them concerns none of them.
    if let Some(last) = plans.iter().map(|p| p.sent_as).max() {
        inflight.brought.retain(|b| b.3 > last);
    }
}

/// Of the line's things in order (a flight: the value signalled on the
/// host's fence behind it; a write or a signal behind a flight: `None`):
/// how many, from the front, can be landed when the card has reached
/// `completed`: everything ahead of the first flight it has not run. A
/// later flight never lands before an earlier one, and what follows a
/// flight (its bottom-of-pipe writes, signals) never before the flight.
fn landable(line: &[Option<u64>], completed: u64) -> usize {
    line.iter().position(|f| f.is_some_and(|f| f > completed)).unwrap_or(line.len())
}

/// For one plan of copies in the card's memory, with [`relay`]: `flags`
/// holds, for every piece of every range of the plan, the copy, the
/// piece's number in the copy and whether the compare flagged it there (or
/// did not run). The pieces to fetch, under whichever range: all of them
/// flagged under any. Two ranges of a plan may hold parts of the same
/// 64 KiB piece; the compare hashes the whole piece for each and stores
/// the hash, so the second of them may find it stored already and not
/// flag it.
fn flagged_pieces(flags: &[(usize, u32, bool)]) -> std::collections::HashSet<(usize, u32)> {
    flags.iter().filter(|f| f.2).map(|f| (f.0, f.1)).collect()
}

/// Whether pieces fetched on a copy queue beside later flights are fetched
/// again when those flights land (`VICTORY_QUEUE2_REFETCH=0`: not, as
/// before; on by default with `VICTORY_QUEUE2` bit 1).
fn refetch() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_QUEUE2_REFETCH").is_none_or(|v| v != "0"))
}

/// With fetches on a copy queue (`crate::queue2`, bit 1): the pieces (copy,
/// piece number) a fetch brought down while later submissions may have been
/// running, each with the number of the last submission started when the
/// fetch had run ([`STARTED`]).
///
/// Such a fetch stands behind the flights being landed and nothing else,
/// so it runs beside whatever was sent after them. A later flight that
/// writes the piece (a buffer cleared and filled again) may be half way
/// through it: the bytes that come down are neither the earlier flight's
/// nor the later one's, and they go into title memory. If the later flight
/// leaves the piece as the earlier one left it (a still scene does, every
/// frame), its own compare finds the stored hash unchanged and flags
/// nothing: title memory kept the half-written bytes until the piece next
/// changed. On the direct queue a fetch only ever sees whole flights.
///
/// So a flight that declared such a piece and had started before the fetch
/// had run fetches it at its own landing whatever its compare says
/// ([`fetched_beside`]); that fetch stands behind the flight, and is noted
/// here in turn if still later flights were in the air. Between the two
/// landings title memory may hold the half-written bytes, as it does on
/// the console, where the title reads the memory the card is writing: what
/// the title is promised is the piece as the flight left it once that
/// flight's fence is signalled, and the landing comes before the signal.
static BESIDE: Mutex<Option<std::collections::HashMap<(usize, u32), u64>>> = Mutex::new(None);

/// Since the last frame-rate line: pieces fetched because an earlier fetch
/// of them ran beside their flight ([`BESIDE`]), and pieces noted there.
static BESIDE_STATS: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// [`BESIDE`]: whether the flight numbered `number` (among the submissions
/// started) must fetch a piece that an earlier landing's fetch brought
/// down when submissions up to `started` had been started: it had started
/// by then, so it may have been writing the piece under that fetch.
fn fetched_beside(started: Option<u64>, number: u64) -> bool {
    started.is_some_and(|started| started >= number)
}

/// [`BESIDE`], after a landing of flights up to number `landed` whose
/// fetch brought `fetched` down while submissions up to `started` had
/// been started: what is noted of the pieces afterwards, and how many were
/// noted now. A note is done with once every submission it names has
/// landed (each fetched the piece again if it declared it); the pieces
/// just fetched are noted if anything later than the flights landed had
/// started.
fn note_beside(noted: &mut std::collections::HashMap<(usize, u32), u64>, fetched: &[(usize, u32)], landed: u64, started: u64) -> u64 {
    noted.retain(|_, upto| *upto > landed);
    if started <= landed {
        return 0;
    }
    for piece in fetched {
        noted.insert(*piece, started);
    }
    fetched.len() as u64
}

#[cfg(test)]
mod relay_tests {
    use super::{fetched_beside, flagged_pieces, join_copy_ranges, join_ranges, landable, note_beside, touches_other};

    /// A piece fetched on the copy queue while a later flight was in the
    /// air is fetched again by that flight, and by no flight sent after
    /// the fetch had run; the note goes when its flights have landed.
    #[test]
    fn a_piece_fetched_beside_a_later_flight_is_fetched_again_by_it() {
        let mut noted = std::collections::HashMap::new();
        let (a, p) = (0x1000usize, 7u32);
        // Flight 5 lands; its fetch ran while 6 and 7 had started.
        assert_eq!(note_beside(&mut noted, &[(a, p)], 5, 7), 1);
        assert!(fetched_beside(noted.get(&(a, p)).copied(), 6));
        assert!(fetched_beside(noted.get(&(a, p)).copied(), 7));
        // Flight 8 was sent after the fetch had run: its compare decides.
        assert!(!fetched_beside(noted.get(&(a, p)).copied(), 8));
        // Another piece was never noted.
        assert!(!fetched_beside(noted.get(&(a, p + 1)).copied(), 6));
        // Flight 6 lands and fetches it again, with 7 still in the air and
        // nothing newer: the note stays for 7.
        assert_eq!(note_beside(&mut noted, &[(a, p)], 6, 7), 1);
        assert!(fetched_beside(noted.get(&(a, p)).copied(), 7));
        // Flight 7 lands with nothing behind it: nothing is noted any more.
        assert_eq!(note_beside(&mut noted, &[(a, p)], 7, 7), 0);
        assert!(noted.is_empty());
        // A fetch with nothing later in the air notes nothing.
        assert_eq!(note_beside(&mut noted, &[(a, p)], 9, 9), 0);
        assert!(noted.is_empty());
    }

    /// Two flights and what follows each: nothing of the second lands
    /// before the card has run it, and nothing behind a flight lands
    /// before the flight.
    #[test]
    fn flights_land_in_the_order_they_were_sent() {
        // Flight (fence 5), its write, a signal, flight (fence 6), its write.
        let line = [Some(5), None, None, Some(6), None];
        assert_eq!(landable(&line, 4), 0);
        assert_eq!(landable(&line, 5), 3);
        assert_eq!(landable(&line, 6), 5);
        assert_eq!(landable(&line, u64::MAX), 5);
        // Signals with no flight ahead land at once; behind one they wait.
        assert_eq!(landable(&[None, None], 0), 2);
        assert_eq!(landable(&[None, Some(9), None], 8), 1);
        assert_eq!(landable(&[], 3), 0);
        // After the first three landed, the rest when its turn comes.
        assert_eq!(landable(&line[3..], 5), 0);
        assert_eq!(landable(&line[3..], 6), 2);
    }

    /// A piece flagged under one range of a plan is fetched under every
    /// range that holds a part of it; pieces of other copies are not.
    #[test]
    fn a_piece_flagged_under_one_range_is_fetched_under_all() {
        let (a, b) = (0x1000usize, 0x2000usize);
        // Copy a: piece 3 under two ranges, flagged under the first only;
        // piece 4 not flagged; copy b: piece 3 not flagged, piece 9 flagged.
        let flags = [(a, 3, true), (a, 4, false), (a, 3, false), (b, 3, false), (b, 9, true)];
        let fetch = flagged_pieces(&flags);
        assert!(fetch.contains(&(a, 3)));
        assert!(!fetch.contains(&(a, 4)));
        assert!(!fetch.contains(&(b, 3)));
        assert!(fetch.contains(&(b, 9)));
        assert_eq!(fetch.len(), 2);
    }

    /// Refreshing the same host copy that declared an in-flight shader write
    /// needs no CPU wait (its GPU buffer receives the write in queue order);
    /// refreshing an overlapping range of another host copy, or a range a
    /// read-back writes (`owner == 0`), must wait until that flight lands.
    #[test]
    fn refreshes_wait_only_for_writes_from_another_copy_or_readback() {
        let (copy_a, copy_b) = (0xaaaa_0000usize, 0xbbbb_0000usize);
        let mut written = Vec::new();
        let mut by_copy = Vec::new();
        join_ranges(
            &mut written,
            [(0x1000, 0x1800), (0x1800, 0x2000), (0x4000, 0x4100)],
        );
        join_copy_ranges(
            &mut by_copy,
            [
                (copy_a, 0x1000, 0x1800),
                (copy_a, 0x1600, 0x2000),
                (0, 0x4000, 0x4100),
            ],
        );
        assert_eq!(by_copy, vec![(0, 0x4000, 0x4100), (copy_a, 0x1000, 0x2000)]);
        // Same copy: never blocked by its own pending GPU writes.
        assert!(!touches_other(&written, &by_copy, copy_a, 0x0800, 0x3000));
        // Aliased copy over the same title memory: blocked until copy-back.
        assert!(touches_other(&written, &by_copy, copy_b, 0x1200, 0x1400));
        assert!(!touches_other(&written, &by_copy, copy_b, 0x2000, 0x3000));
        // Read-back (`owner == 0`): blocks every copy.
        assert!(touches_other(&written, &by_copy, copy_a, 0x4000, 0x4080));
        assert!(touches_other(&written, &by_copy, copy_b, 0x4080, 0x4200));
    }
}

/// The most host lists in the air before the flights are settled
/// (`VICTORY_LATE_WAIT_LISTS`, 128; 32 settled the flights three times a
/// frame in run 92 and 64 still 1.3-1.7 times in run 99, with 12,919 tables
/// asked and none refused; watch "tables asked, N refused"): their dynamic
/// descriptor slots, upload
/// memory and scratch chunks are held until then, and a frame's lists held
/// all at once ran the slots out before (run 45).
fn late_most() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("VICTORY_LATE_WAIT_LISTS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(128).max(1)
    })
}

/// Whether a submission with bottom-of-pipe writes may fly
/// (`VICTORY_LATE_WAIT_WRITES=0`: it is settled as soon as it is sent, so
/// the title is told at once, as before; for telling a title that polls
/// such a value from one that does not).
fn late_writes() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LATE_WAIT_WRITES").is_none_or(|v| v != "0"))
}

/// Per host copy, the ranges a submission's shaders may write, as declared
/// (address, end).
type Declared = (std::sync::Arc<crate::shadow::Shadow>, Vec<(usize, usize)>);

/// What [`Host::submit`] hands back for a submission it sent and did not
/// wait for.
struct Sent {
    /// Its number among the submissions started.
    number: u64,
    card: Option<CardPlan>,
    back: Vec<Declared>,
    /// Title memory ranges written by its texture read-backs on landing.
    backs: Vec<(usize, usize)>,
    /// Its bottom-of-pipe writes.
    writes: Vec<(usize, crate::queue::Written, usize)>,
}

/// A submission on the card that has not been waited for ([`late`]): its
/// host lists, taken from the title's (which it may reset meanwhile), and
/// what is to be done once it has run.
struct Flight {
    number: u64,
    /// The value signalled on the host's fence behind it.
    fence: u64,
    lists: Vec<crate::list::Detached>,
    order: Vec<usize>,
    card: Option<CardPlan>,
    back: Vec<Declared>,
    backs: Vec<(usize, usize)>,
}

/// One thing in the line of what waits for the card, in order.
enum Late {
    Run(Flight),
    /// A bottom-of-pipe write of the flight before it: address, what, bytes.
    Write(usize, crate::queue::Written, usize),
    /// A fence (its address) signalled behind the flights before it.
    Signal(usize, u64),
}

struct Line {
    line: Vec<Late>,
    /// The title memory the flights' shaders and read-backs may write
    /// (address, end), in order and joined.
    written: Vec<(usize, usize)>,
    /// `written` by owner (`Shadow` pointer, or `0` for read-backs), joined
    /// per owner so an aliased `Shadow` refresh sees writes of other copies.
    by_copy: Vec<(usize, usize, usize)>,
    /// Host lists in the flights.
    lists: usize,
    /// The value signalled on the host's fence behind the last flight.
    target: u64,
    /// A settle has taken the line and is not done: a signal still goes
    /// behind it.
    settling: bool,
    /// Something was settled away from a queue's own calls: the helper
    /// thread looks at what queues hold.
    poke: bool,
    /// Host lists of settled flights, reset and given back outside the
    /// submission lock ([`land`]: a settle may run under a caller's own
    /// locks).
    landed: Vec<(Vec<crate::list::Detached>, Vec<usize>)>,
    /// With [`relay`]: the bottom-of-pipe writes of the flights being
    /// landed (taken out of `line`, not done yet), for [`late_met`].
    in_hand: Vec<(usize, crate::queue::Written, usize)>,
}

impl Line {
    /// Title memory ranges of bottom-of-pipe writes still in flight or being
    /// landed, in order and joined.
    fn immed_writes(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let all = self
            .in_hand
            .iter()
            .map(|&(at, _, n)| (at, at.saturating_add(n)))
            .chain(self.line.iter().filter_map(|item| match *item {
                Late::Write(at, _, n) => Some((at, at.saturating_add(n))),
                _ => None,
            }));
        join_ranges(&mut out, all);
        out
    }
}

// SAFETY: D3D12 objects (free-threaded) and host lists nothing records
// into; touched under the lock.
unsafe impl Send for Line {}

static LINE: Mutex<Line> = Mutex::new(Line {
    line: Vec::new(),
    written: Vec::new(),
    by_copy: Vec::new(),
    lists: 0,
    target: 0,
    settling: false,
    poke: false,
    landed: Vec::new(),
    in_hand: Vec::new(),
});
/// Told when a flight joins [`LINE`] or it is poked.
static LINE_MOVED: std::sync::Condvar = std::sync::Condvar::new();

fn line() -> std::sync::MutexGuard<'static, Line> {
    LINE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Why flights were settled ([`LATE_STATS`]' places from 2 on).
const WHY_SYNC: usize = 2;
const WHY_CARD_DONE: usize = 3;
const WHY_VALUE: usize = 4;
const WHY_STALE: usize = 5;
const WHY_READ: usize = 6;
const WHY_RUN: usize = 7;
const WHY_MOST: usize = 8;
const WHY_WRITES: usize = 9;
const WHY_FENCE: usize = 10;

/// Since the last frame-rate line: submissions sent without a wait;
/// settles; settles by reason ([`WHY_SYNC`]..); flights settled;
/// microseconds settles waited for the card; value waits met by a flight's
/// bottom-of-pipe write; fence signals set behind flights.
/// Then: the settles for title memory read, by what read it (a buffer
/// copy, a fill of upload memory, a texture, another host copy).
static LATE_STATS: [std::sync::atomic::AtomicU64; 19] = [const { std::sync::atomic::AtomicU64::new(0) }; 19];

/// Since the last frame-rate line: the microseconds settles waited for the
/// card, by why they settled (the `WHY_` places).
static LATE_WAITED: [std::sync::atomic::AtomicU64; 11] = [const { std::sync::atomic::AtomicU64::new(0) }; 11];

/// Since the last frame-rate line, by why they settled (the `WHY_`
/// places): settles that waited 8 ms or more, and the longest wait (microseconds).
static LATE_LONG: [std::sync::atomic::AtomicU64; 11] = [const { std::sync::atomic::AtomicU64::new(0) }; 11];
static LATE_MAX: [std::sync::atomic::AtomicU64; 11] = [const { std::sync::atomic::AtomicU64::new(0) }; 11];

/// A settle for `why` waited `us` microseconds for the card.
fn note_waited(why: usize, us: u64) {
    let why = why.min(10);
    LATE_STATS[12].fetch_add(us, Ordering::Relaxed);
    LATE_WAITED[why].fetch_add(us, Ordering::Relaxed);
    LATE_LONG[why].fetch_add(u64::from(us >= 8_000), Ordering::Relaxed);
    LATE_MAX[why].fetch_max(us, Ordering::Relaxed);
}

/// Whether `[from, to)` touches one of `ranges` (in order, joined).
fn touches(ranges: &[(usize, usize)], from: usize, to: usize) -> bool {
    if from >= to {
        return false;
    }
    let at = ranges.partition_point(|r| r.1 <= from);
    ranges.get(at).is_some_and(|r| r.0 < to)
}

/// Whether `[from, to)` of `copy` overlaps an in-flight write belonging to
/// another host copy or to a read-back (`owner == 0`).
fn touches_other(
    written: &[(usize, usize)],
    by_copy: &[(usize, usize, usize)],
    copy: usize,
    from: usize,
    to: usize,
) -> bool {
    touches(written, from, to)
        && by_copy
            .iter()
            .any(|&(owner, a, b)| owner != copy && a < to && b > from)
}

/// `ranges` with `more`, in order, joined where they overlap or touch.
fn join_ranges(ranges: &mut Vec<(usize, usize)>, more: impl IntoIterator<Item = (usize, usize)>) {
    ranges.extend(more.into_iter().filter(|r| r.0 < r.1));
    ranges.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for &(from, to) in ranges.iter() {
        match out.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    *ranges = out;
}

/// `(owner, from, to)` ranges joined per `owner` where they overlap or touch.
fn join_copy_ranges(
    ranges: &mut Vec<(usize, usize, usize)>,
    more: impl IntoIterator<Item = (usize, usize, usize)>,
) {
    ranges.extend(more.into_iter().filter(|r| r.1 < r.2));
    ranges.sort_unstable();
    let mut out: Vec<(usize, usize, usize)> = Vec::with_capacity(ranges.len());
    for &(owner, from, to) in ranges.iter() {
        match out.last_mut() {
            Some(last) if last.0 == owner && from <= last.2 => last.2 = last.2.max(to),
            _ => out.push((owner, from, to)),
        }
    }
    *ranges = out;
}

/// Whether anything is in the air.
fn flying() -> bool {
    if !late() {
        return false;
    }
    let line = line();
    // (With the relay: also while flights are being landed.)
    line.settling || line.line.iter().any(|l| matches!(l, Late::Run(_)))
}

/// With flights in the air ([`late`]): why `runs` cannot be prepared
/// before they are settled, if so. Preparing reads title memory on the
/// CPU (the title's buffer copies, fills of upload memory, textures, or
/// another host copy over the same bytes) and runs steps of ours that may
/// read or write it anywhere; what the flights write is not there yet.
fn late_blocked(runs: &[&HostList]) -> Option<usize> {
    let (written, by_copy, immed) = {
        let line = line();
        let immed = line.immed_writes();
        if line.written.is_empty() && immed.is_empty() {
            return None;
        }
        (line.written.clone(), line.by_copy.clone(), immed)
    };
    blocked_by(&written, &by_copy, &immed, runs, true)
}

/// Whether a small fill of upload memory (a constant buffer) goes ahead of
/// flights that declared they may write where it reads
/// (`VICTORY_FILL_FREE=1`; off by default). One such fill a frame, of 480
/// bytes, waits about 40 ms for flights that declared a heap of 174 MB whole.
/// One in 8 of those let through is looked at again when the flights have
/// landed ([`fills_landed`]): whether the bytes it read are others then.
/// shortcut: that looks, it does not repair: a constant buffer a shader does
/// write is read as it was before. Not for the title's buffer copies: let
/// ahead in the same way they lost the card at world load, twice of two.
fn fill_free() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FILL_FREE").is_some_and(|v| v == "1"))
}

/// Fills let through and looked at again: the last submission started then,
/// where they read, and the bytes there.
static FILLS_FREE: Mutex<Vec<(u64, usize, Vec<u8>)>> = Mutex::new(Vec::new());

fn fill_through(src: usize, len: usize) {
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if !COUNT.fetch_add(1, Ordering::Relaxed).is_multiple_of(8) {
        return;
    }
    let mut fills = FILLS_FREE.lock().unwrap_or_else(|e| e.into_inner());
    if fills.len() < 4096 {
        let mut bytes = vec![0u8; len];
        copy_mapped(bytes.as_mut_ptr(), src as *const u8, len, false);
        fills.push((STARTED.load(Ordering::Acquire), src, bytes));
    }
}

/// The flights up to `last` have landed: see [`fill_free`].
fn fills_landed(last: u64) {
    static SEEN: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];
    if !fill_free() {
        return;
    }
    let done: Vec<_> = {
        let mut fills = FILLS_FREE.lock().unwrap_or_else(|e| e.into_inner());
        let (done, left) = std::mem::take(&mut *fills).into_iter().partition(|f| f.0 <= last);
        *fills = left;
        done
    };
    for (_, src, was) in done {
        let mut now = vec![0u8; was.len()];
        copy_mapped(now.as_mut_ptr(), src as *const u8, now.len(), false);
        let seen = SEEN[0].fetch_add(1, Ordering::Relaxed) + 1;
        let other = SEEN[1].fetch_add(u64::from(now != was), Ordering::Relaxed) + u64::from(now != was);
        if seen.is_multiple_of(64) {
            tracing::info!("fills let ahead of flights, looked at: {seen}, of them {other} read bytes that were others once the flights had landed");
        }
    }
}

/// [`late_blocked`] against `written` (joined), `by_copy` (per owner), and
/// `immed` (bottom-of-pipe writes); `count` whether the kind of read is
/// counted for the frame line.
fn blocked_by(
    written: &[(usize, usize)],
    by_copy: &[(usize, usize, usize)],
    immed: &[(usize, usize)],
    runs: &[&HostList],
    count: bool,
) -> Option<usize> {
    if written.is_empty() && immed.is_empty() {
        return None;
    }
    let any_written = |from: usize, to: usize| touches(written, from, to) || touches(immed, from, to);
    for list in runs {
        for step in &list.deferred().copies {
            match step {
                &Step::Copy(dst, src, len) => {
                    if any_written(src, src + len) || any_written(dst, dst + len) {
                        // A copy that only READS where flights may write is
                        // not waited for either, and checked like a fill
                        // (`fill_ahead`): run 210 looked at 197 such waits
                        // (20,480 bytes out of the 174 MB buffer into a small
                        // buffer of its own, four a frame, 43 to 60 ms a
                        // frame) and neither side had other bytes after
                        // one. A copy INTO such a range still waits.
                        if !any_written(dst, dst + len) && fill_ahead::on() {
                            if count {
                                fill_ahead::read(src, len);
                            }
                            continue;
                        }
                        if count {
                            static SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                            if SEEN.fetch_add(1, Ordering::Relaxed) % 64 == 0 {
                                tracing::info!("a copy waits for flights: {src:#x} to {dst:#x}, {len:#x} bytes");
                            }
                            LATE_STATS[15].fetch_add(1, Ordering::Relaxed);
                            #[cfg(feature = "diagnostics")]
                            fill_check::blocked_copy(written, dst, src, len);
                        }
                        return Some(WHY_READ);
                    }
                }
                &Step::Fill(_, src, len) => {
                    if any_written(src, src + len) {
                        if fill_free() && len <= 1 << 16 {
                            if count {
                                fill_through(src, len);
                            }
                            continue;
                        }
                        // Not waited for, and checked when the flights land
                        // (`fill_ahead`).
                        if fill_ahead::on() {
                            if count {
                                fill_ahead::read(src, len);
                            }
                            continue;
                        }
                        if count {
                            LATE_STATS[16].fetch_add(1, Ordering::Relaxed);
                            #[cfg(feature = "diagnostics")]
                            fill_check::blocked(written, src, len);
                        }
                        return Some(WHY_READ);
                    }
                }
                Step::Texture(staging) => {
                    let (at, len) = staging.memory();
                    if any_written(at, at + len) {
                        if count {
                            LATE_STATS[17].fetch_add(1, Ordering::Relaxed);
                        }
                        return Some(WHY_READ);
                    }
                }
                Step::Refresh(shadow, address, len) => {
                    let (from, to) = (*address, *address + *len);
                    let copy = std::sync::Arc::as_ptr(shadow) as usize;
                    if touches_other(written, by_copy, copy, from, to) || touches(immed, from, to) {
                        if count {
                            LATE_STATS[18].fetch_add(1, Ordering::Relaxed);
                        }
                        return Some(WHY_READ);
                    }
                }
                // A title copy that says where it reads and writes holds the
                // submission back only as the title's buffer copies do.
                &Step::Run(_, Some((dst, src, len))) if crate::memcopy::known_writes() => {
                    if any_written(src, src.saturating_add(len)) || any_written(dst, dst.saturating_add(len)) {
                        if count {
                            LATE_STATS[15].fetch_add(1, Ordering::Relaxed);
                        }
                        return Some(WHY_READ);
                    }
                    if count {
                        crate::memcopy::add(crate::memcopy::M::RunsNotBlocking, 1);
                    }
                }
                Step::Run(..) => return Some(WHY_RUN),
                Step::GpuWrite(..) | Step::Host(_) => {}
            }
        }
    }
    None
}

/// `VICTORY_READFIX=1` (off by default): refreshes of a host copy over
/// bytes another copy's flights write are served on the card instead of
/// waiting for those flights ([`crate::readfix`]). Needs the relay with
/// per-flight read waits (`VICTORY_SYNC_PRECISE=1` or `VICTORY_RELAY=2`).
pub(crate) fn readfix() -> bool {
    // The ask is live (`victory_core::live`, `VICTORY_AB`; flipped only
    // with every flight settled, so no redirect is in the air); what it
    // needs (the relay and its waits) is boot-only.
    static ABLE: OnceLock<bool> = OnceLock::new();
    static ASKED: victory_core::live::Switch = victory_core::live::Switch::new("READFIX", 0);
    static SAID: std::sync::Once = std::sync::Once::new();
    let asked = ASKED.on();
    if !asked {
        return false;
    }
    let on = *ABLE.get_or_init(|| late() && relay() && (relay_exact() || sync_precise()));
    SAID.call_once(|| {
        if on {
            tracing::info!("refreshes under another copy's shader writes are served on the card (VICTORY_READFIX=1)");
        } else {
            tracing::warn!("VICTORY_READFIX=1 needs VICTORY_RELAY=1 with VICTORY_SYNC_PRECISE=1 (or VICTORY_RELAY=2): off");
        }
    });
    on
}

/// Since the last frame-rate line ([`readfix`]): 0 refreshes served on the
/// card, 1 pieces copied, 2 their bytes, 3 read waits still taken, 4 their
/// microseconds, 5 later steps that waited for a redirect still in the air;
/// then from 6 on the refreshes not served, by [`crate::readfix::Refuse`];
/// then the read waits still taken by what read: 15 a buffer copy, 16 a
/// fill, 17 a texture, 18 a refresh, 19 a step of ours.
static READFIX_STATS: [std::sync::atomic::AtomicU64; 20] = [const { std::sync::atomic::AtomicU64::new(0) }; 20];

/// The first decisions, logged with their ranges ([`readfix`]).
static READFIX_LOGGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const READFIX_LOG_MOST: u64 = 64;

/// The copies whose shaders flights not finished declared written, by
/// flight number ([`readfix`]): the sources a refresh is served from (the
/// line no longer holds the flights being landed).
static READFIX_OWNERS: Mutex<Vec<(u64, std::sync::Arc<crate::shadow::Shadow>)>> = Mutex::new(Vec::new());

/// Redirects not finished: (copy, from, to, the submission that copies).
/// A later CPU write of the copy there, or a snapshot of it for its own
/// shader writes, waits for the copy on the card first.
static READFIX_PENDING: Mutex<Vec<(usize, usize, usize, u64)>> = Mutex::new(Vec::new());

/// Lists of ours that copied pieces, until their submission finished.
type ReadfixList = (u64, Prelude, Vec<std::sync::Arc<crate::shadow::Shadow>>);
static READFIX_LISTS: Mutex<Vec<ReadfixList>> = Mutex::new(Vec::new());

/// One piece served on the card: `from`'s bytes `[at, end)` (title
/// addresses) copied into `to`.
struct Redirect {
    from: std::sync::Arc<crate::shadow::Shadow>,
    to: std::sync::Arc<crate::shadow::Shadow>,
    at: usize,
    end: usize,
}

fn readfix_note(why: usize) {
    READFIX_STATS[6 + why.min(crate::readfix::REFUSALS - 1)].fetch_add(1, Ordering::Relaxed);
}

/// Logs one of the first decisions ([`readfix`]).
fn readfix_log(what: &str, step: &str, from: usize, to: usize, detail: impl std::fmt::Display) {
    if READFIX_LOGGED.fetch_add(1, Ordering::Relaxed) < READFIX_LOG_MOST {
        tracing::info!(
            "VICTORY_READFIX {what}: {step} {from:#x}..{to:#x} ({} bytes): {detail}",
            to.saturating_sub(from)
        );
    }
}

/// The in-flight writes overlapping `[from, to)`, described for the log.
fn readfix_overlaps(
    by_copy: &[(usize, usize, usize)],
    immed: &[(usize, usize)],
    owners: &[(u64, std::sync::Arc<crate::shadow::Shadow>)],
    from: usize,
    to: usize,
) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    for &(o, a, b) in by_copy.iter().filter(|&&(_, a, b)| a < to && b > from) {
        let kind = if o == 0 {
            "read-back".to_owned()
        } else {
            match owners.iter().find(|(_, s)| std::sync::Arc::as_ptr(s) as usize == o) {
                Some((n, sh)) => {
                    let (st, len) = sh.span();
                    format!(
                        "{} {st:#x}+{len:#x} flight {n}",
                        if sh.is_window() { "window" } else { "buffer copy" }
                    )
                }
                None => format!("copy {o:#x} (landing)"),
            }
        };
        let _ = write!(s, "[{a:#x}..{b:#x} {kind}] ");
    }
    for &(a, b) in immed.iter().filter(|&&(a, b)| a < to && b > from) {
        let _ = write!(s, "[{a:#x}..{b:#x} bottom-of-pipe write] ");
    }
    s
}

/// [`readfix`]: whether `runs` can be prepared now, the refreshes over
/// other copies' in-flight writes served on the card (what comes back), or
/// the reason to wait first (`WHY_` place). Against the line as it is.
fn readfix_decide(runs: &[&HostList]) -> Result<Vec<Redirect>, usize> {
    use crate::readfix::{Refuse, covered, plan_refresh};
    use std::sync::Arc;
    let finished = FINISHED.load(Ordering::Acquire);
    let (written, by_copy, immed) = {
        let line = line();
        (line.written.clone(), line.by_copy.clone(), line.immed_writes())
    };
    let owners: Vec<(u64, Arc<crate::shadow::Shadow>)> = {
        let mut o = READFIX_OWNERS.lock().unwrap_or_else(|e| e.into_inner());
        o.retain(|(n, _)| *n > finished);
        o.clone()
    };
    let pending: Vec<(usize, usize, usize, u64)> = {
        let mut p = READFIX_PENDING.lock().unwrap_or_else(|e| e.into_inner());
        p.retain(|e| e.3 > finished);
        p.clone()
    };
    if written.is_empty() && immed.is_empty() && pending.is_empty() {
        return Ok(Vec::new());
    }
    let pending_over = |copy: usize, a: usize, b: usize| pending.iter().filter(move |e| e.0 == copy && e.1 < b && e.2 > a);
    let any_written = |from: usize, to: usize| touches(&written, from, to) || touches(&immed, from, to);
    // What this submission's shaders write, per copy.
    let mut declared: Vec<(usize, usize, usize)> = Vec::new();
    for list in runs {
        for step in &list.deferred().copies {
            if let Step::GpuWrite(shadow, address, len) = step {
                declared.push((Arc::as_ptr(shadow) as usize, *address, *address + *len));
            }
        }
    }
    let mut out: Vec<Redirect> = Vec::new();
    let wait = |place: usize, step: &str, from: usize, to: usize, detail: String| {
        READFIX_STATS[place].fetch_add(1, Ordering::Relaxed);
        readfix_log("waits", step, from, to, detail);
        Err(WHY_READ)
    };
    for list in runs {
        for step in &list.deferred().copies {
            match step {
                &Step::Copy(dst, src, len) => {
                    if any_written(src, src + len) || any_written(dst, dst + len) {
                        let d = readfix_overlaps(&by_copy, &immed, &owners, src.min(dst), src.max(dst) + len);
                        return wait(15, "a buffer copy", src, src + len, format!("to {dst:#x}; {d}"));
                    }
                }
                &Step::Fill(_, src, len) => {
                    if any_written(src, src + len) {
                        let d = readfix_overlaps(&by_copy, &immed, &owners, src, src + len);
                        return wait(16, "a fill of upload memory", src, src + len, d);
                    }
                }
                Step::Texture(staging) => {
                    let (at, len) = staging.memory();
                    if any_written(at, at + len) {
                        let d = readfix_overlaps(&by_copy, &immed, &owners, at, at + len);
                        return wait(17, "a texture", at, at + len, d);
                    }
                }
                &Step::Run(_, Some((dst, src, len))) if crate::memcopy::known_writes() => {
                    if any_written(src, src.saturating_add(len)) || any_written(dst, dst.saturating_add(len)) {
                        return wait(15, "a title copy", src, src.saturating_add(len), format!("to {dst:#x}"));
                    }
                }
                Step::Run(..) if !written.is_empty() || !immed.is_empty() => {
                    READFIX_STATS[19].fetch_add(1, Ordering::Relaxed);
                    return Err(WHY_RUN);
                }
                Step::GpuWrite(shadow, address, len) => {
                    // Its snapshot is taken from the copy's CPU side, which a
                    // redirect still in the air has not written yet.
                    let copy = Arc::as_ptr(shadow) as usize;
                    if pending_over(copy, *address, *address + *len).next().is_some() {
                        READFIX_STATS[5].fetch_add(1, Ordering::Relaxed);
                        return wait(18, "shader writes of a copy served on the card", *address, *address + *len, String::new());
                    }
                }
                Step::Refresh(shadow, address, len) => {
                    let (from, to) = (*address, *address + *len);
                    let copy = Arc::as_ptr(shadow) as usize;
                    let blocked = touches_other(&written, &by_copy, copy, from, to) || touches(&immed, from, to);
                    let earlier: Vec<(usize, usize)> = pending_over(copy, from, to).map(|e| (e.1.max(from), e.2.min(to))).collect();
                    if !blocked && earlier.is_empty() {
                        continue;
                    }
                    let refuse = |why: Refuse, detail: String| {
                        readfix_note(why as usize);
                        wait(18, "a refresh", from, to, format!("{why:?}; {detail}"))
                    };
                    let pieces = match plan_refresh(copy, from, to, &by_copy, &immed) {
                        Ok(p) => p,
                        Err(why) => return refuse(why, readfix_overlaps(&by_copy, &immed, &owners, from, to)),
                    };
                    // An earlier redirect into the copy here, not run yet,
                    // would land after the CPU's refresh of it: only if this
                    // one copies the same bytes again after it.
                    if earlier.iter().any(|&(a, b)| !covered(&pieces, a, b)) {
                        READFIX_STATS[5].fetch_add(1, Ordering::Relaxed);
                        return refuse(Refuse::Pending, String::new());
                    }
                    if pieces.is_empty() {
                        continue;
                    }
                    if shadow.carded() || shadow.in_place() {
                        return refuse(Refuse::Kind, String::new());
                    }
                    if pieces
                        .iter()
                        .any(|p| declared.iter().any(|&(c, a, b)| c == copy && a < p.to && b > p.from))
                    {
                        return refuse(Refuse::Declared, String::new());
                    }
                    for p in &pieces {
                        let Some((_, source)) = owners.iter().find(|(_, s)| Arc::as_ptr(s) as usize == p.owner) else {
                            return refuse(Refuse::NoSource, format!("{:#x}..{:#x}", p.from, p.to));
                        };
                        if !source.took_current(p.from, p.to) {
                            return refuse(Refuse::TitleWrote, readfix_overlaps(&by_copy, &immed, &owners, p.from, p.to));
                        }
                        if !out.iter().any(|r| Arc::ptr_eq(&r.to, shadow) && r.at == p.from && r.end == p.to) {
                            out.push(Redirect { from: source.clone(), to: shadow.clone(), at: p.from, end: p.to });
                        }
                    }
                    readfix_log(
                        "serves on the card",
                        "a refresh",
                        from,
                        to,
                        readfix_overlaps(&by_copy, &immed, &owners, from, to),
                    );
                }
                Step::Run(..) | Step::Host(_) => {}
            }
        }
    }
    Ok(out)
}

/// [`readfix`]: until `runs` can be prepared, waits for flights to land a
/// landing at a time (counted as one settle for title memory read), or
/// settles them for a step of ours; then the refreshes to serve on the card.
fn readfix_settle(host: &Host, submitted: &mut u64, runs: &[&HostList]) -> Vec<Redirect> {
    let started = std::time::Instant::now();
    let mut counted = false;
    loop {
        match readfix_decide(runs) {
            Ok(redirects) => {
                if counted {
                    let us = started.elapsed().as_micros() as u64;
                    READFIX_STATS[4].fetch_add(us, Ordering::Relaxed);
                }
                return redirects;
            }
            Err(why) if why == WHY_READ => {
                if !counted {
                    counted = true;
                    READFIX_STATS[3].fetch_add(1, Ordering::Relaxed);
                    LATE_STATS[1].fetch_add(1, Ordering::Relaxed);
                    LATE_STATS[WHY_READ].fetch_add(1, Ordering::Relaxed);
                }
                let seen = LANDINGS.load(Ordering::Acquire);
                let was_working = working(false);
                let waiting = std::time::Instant::now();
                {
                    let mut line = line();
                    while LANDINGS.load(Ordering::Acquire) == seen && !(line.line.is_empty() && !line.settling) {
                        line = LINE_LANDED.wait(line).unwrap_or_else(|e| e.into_inner());
                    }
                }
                stood(waiting);
                working(was_working);
                let us = waiting.elapsed().as_micros() as u64;
                note_waited(WHY_READ, us);
                SUBMISSION_US[1].fetch_add(us, Ordering::Relaxed);
                // Nothing left in the air, and no landing counted: the
                // line has no more to give (a copy-back done beside it).
                let empty = {
                    let line = line();
                    line.line.is_empty() && !line.settling
                };
                if empty && LANDINGS.load(Ordering::Acquire) == seen {
                    host.settle(submitted, WHY_READ);
                }
            }
            Err(why) => {
                host.settle(submitted, why);
                return Vec::new();
            }
        }
    }
}

impl Host {
    /// [`readfix`]: copies the pieces on the card, after the flights before
    /// and before the submission `number`'s lists.
    fn readfix_copy(&self, redirects: &[Redirect], number: u64) {
        use std::sync::Arc;
        if redirects.is_empty() {
            return;
        }
        let finished = FINISHED.load(Ordering::Acquire);
        let reused = {
            let mut lists = READFIX_LISTS.lock().unwrap_or_else(|e| e.into_inner());
            let at = lists.iter().position(|(n, ..)| *n <= finished);
            at.map(|i| lists.swap_remove(i).1)
        };
        let Some((allocator, list)) = reused.or_else(|| self.make_prelude()) else {
            fail(format_args!("no host list to copy pieces on the card (VICTORY_READFIX=1)"));
        };
        let mut bytes = 0u64;
        // SAFETY: the list's last run finished (its submission did); live
        // buffers (held below until this one finishes), the ranges in them.
        let recorded = unsafe {
            (|| -> windows::core::Result<()> {
                allocator.Reset()?;
                list.Reset(&allocator, None)?;
                for r in redirects {
                    let n = (r.end - r.at) as u64;
                    list.CopyBufferRegion(
                        &r.to.buffer,
                        r.to.offset(r.at as u64),
                        &r.from.buffer,
                        r.from.offset(r.at as u64),
                        n,
                    );
                    bytes += n;
                }
                list.Close()?;
                self.queue.ExecuteCommandLists(&[Some(list.cast()?)]);
                Ok(())
            })()
        };
        if let Err(e) = recorded {
            fail(format_args!("pieces could not be copied on the card (VICTORY_READFIX=1: {e})"));
        }
        let mut held: Vec<Arc<crate::shadow::Shadow>> = Vec::with_capacity(2 * redirects.len());
        {
            let mut pending = READFIX_PENDING.lock().unwrap_or_else(|e| e.into_inner());
            for r in redirects {
                r.to.card_filled(r.at, r.end);
                pending.push((Arc::as_ptr(&r.to) as usize, r.at, r.end, number));
                held.push(r.from.clone());
                held.push(r.to.clone());
            }
        }
        READFIX_LISTS.lock().unwrap_or_else(|e| e.into_inner()).push((number, (allocator, list), held));
        let refreshes = {
            let mut copies: Vec<usize> = redirects.iter().map(|r| Arc::as_ptr(&r.to) as usize).collect();
            copies.sort_unstable();
            copies.dedup();
            copies.len() as u64
        };
        READFIX_STATS[0].fetch_add(refreshes, Ordering::Relaxed);
        READFIX_STATS[1].fetch_add(redirects.len() as u64, Ordering::Relaxed);
        READFIX_STATS[2].fetch_add(bytes, Ordering::Relaxed);
    }
}

/// The frame line of [`readfix`].
fn readfix_line(frames: f64) {
    if !readfix() {
        return;
    }
    let s = READFIX_STATS.each_ref().map(|a| a.swap(0, Ordering::Relaxed) as f64 / frames);
    tracing::info!(
        "refreshes served on the card per frame (VICTORY_READFIX=1): {:.1} copies in {:.1} pieces, {:.2} MB; read waits still taken {:.1} \
         ({:.1} ms; by a buffer copy {:.1}, a fill {:.1}, a texture {:.1}, a refresh {:.1}; settles for a step of ours {:.1}), \
         {:.1} for a redirect still in the air; refreshes not served: bottom-of-pipe write {:.1}, read-back {:.1}, two writers {:.1}, \
         own writes {:.1}, card or in-place copy {:.1}, title wrote it {:.1}, written by the submission {:.1}, redirect in the air {:.1}, \
         source landing {:.1}",
        s[0],
        s[1],
        s[2] / 1e6,
        s[3],
        s[4] / 1e3,
        s[15],
        s[16],
        s[17],
        s[18],
        s[19],
        s[5],
        s[6],
        s[7],
        s[8],
        s[9],
        s[10],
        s[11],
        s[12],
        s[13],
        s[14],
    );
}

/// `VICTORY_SYNC_PRECISE`, after a submission's waits: nothing `runs`
/// read or write on the CPU while they are prepared may still be written
/// by a flight in the air. If something is, the dependency model missed
/// it: counted, and logged once.
fn precise_check(runs: &[&HostList]) {
    let hazard = {
        let line = line();
        let immed = line.immed_writes();
        blocked_by(&line.written, &line.by_copy, &immed, runs, false)
    };
    if let Some(why) = hazard {
        crate::texture::SYNC_STATS[2].fetch_add(1, Ordering::Relaxed);
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(
                why,
                "VICTORY_SYNC_PRECISE hazard: a submission is prepared while a flight in the air may still write what it reads"
            );
        });
    }
}

/// Whether any texture staging buffer refreshed by `runs` is still being
/// read on the GPU by an unfinished flight (`in_flight > finished`):
/// refreshing it now would overwrite its mapped upload buffer mid-copy.
fn staging_in_flight_at(runs: &[&HostList], finished: u64) -> bool {
    runs.iter().any(|list| {
        list.deferred().copies.iter().any(|step| {
            matches!(step, Step::Texture(staging) if staging.in_flight() > finished)
        })
    })
}

fn staging_in_flight(runs: &[&HostList]) -> bool {
    staging_in_flight_at(runs, FINISHED.load(Ordering::Acquire))
}

/// Fills of upload memory (a constant buffer a table names, copied into
/// the list's own memory when the list is submitted) that read title memory
/// flights "may write" are no longer waited for; what the flights then
/// write is checked against them, and the first fill that read bytes a
/// flight changed afterwards turns the waiting back on for good.
///
/// Why: a flight declares every range its shaders can store to, and the
/// title has an unordered view over the whole of its 174 MB buffer, whose
/// copy is in the card's memory. Its per-draw constants lie in the same
/// buffer, at new addresses every frame. So the first such fill of a frame
/// waited until those flights had landed: one wait a frame, 56 to 95 ms of
/// the submitting thread standing still (runs 202 and 206; 145 ms while
/// the world loaded in 208), the largest single wait of the frame. Run 208
/// looked at every one of them: 44 fills of 480 bytes, and after the wait
/// the bytes were the same 44 times.
///
/// What it risks, and how it is bounded: a fill that reads bytes a flight
/// in the air is about to change takes the old bytes, and the lists of
/// that one submission draw with them. `wrote` sees it when the flight
/// lands (the bytes going into title memory are compared with the ones
/// there, over every fill that read ahead of them) and from then on fills
/// wait as before. `VICTORY_FILL_WAIT=1`: they wait from the start.
mod fill_ahead {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    use std::sync::{Mutex, OnceLock};

    /// Fills are looked up by the 64 KiB of title memory they read.
    const PIECE: usize = 1 << 16;
    /// A fill is checked against flights that land this many frames after
    /// it at most (flights are settled at 256 lists in the air, and by
    /// every present that waits).
    const KEEP: u64 = 16;

    /// A flight changed bytes a fill had read ahead of it: fills wait.
    static WAIT: AtomicBool = AtomicBool::new(false);
    /// Since the last frame line: fills that read ahead; landings' writes
    /// that touched one's piece. Since the start: fills that read bytes a
    /// flight then changed.
    static COUNT: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

    #[derive(Default)]
    struct Piece {
        /// The frame its old fills were last dropped in.
        pruned: u64,
        /// From, to, frame, and the number of the last submission started
        /// when it read: the flights in the air then have that number or
        /// a lower one, and only their writes can be ones it missed.
        fills: Vec<(usize, usize, u64, u64)>,
    }

    /// The lowest submission number among the flights the landing thread
    /// is landing now; 0: not known (a landing by another way), and then
    /// every write counts. (Run 211: a read of frame 1381 was taken for
    /// missed because shaders wrote the same 80 bytes in frame 1397, in a
    /// flight sent sixteen frames after it.)
    static LANDING: AtomicU64 = AtomicU64::new(0);

    pub(super) fn landing(first: u64) {
        LANDING.store(first, Relaxed);
    }

    static READ: Mutex<Option<HashMap<usize, Piece>>> = Mutex::new(None);

    pub(super) fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("VICTORY_FILL_WAIT").is_none_or(|v| v != "1")) && !WAIT.load(Relaxed)
    }

    /// A fill reads `len` bytes at `src` ahead of flights that may write
    /// them.
    pub(super) fn read(src: usize, len: usize) {
        if len == 0 {
            return;
        }
        COUNT[0].fetch_add(1, Relaxed);
        let frame = crate::frame::current();
        let mut read = READ.lock().unwrap_or_else(|e| e.into_inner());
        let map = read.get_or_insert_with(HashMap::new);
        for piece in src / PIECE..=(src + len - 1) / PIECE {
            let p = map.entry(piece).or_default();
            if p.pruned != frame {
                p.pruned = frame;
                p.fills.retain(|f| f.2 + KEEP >= frame);
            }
            p.fills.push((src, src + len, frame, super::STARTED.load(Relaxed)));
        }
    }

    /// A landing is about to put `new` into title memory at `at`: bytes
    /// shaders wrote, merged into the title's.
    pub(crate) fn wrote(at: usize, new: &[u8]) {
        if new.is_empty() || WAIT.load(Relaxed) {
            return;
        }
        let frame = crate::frame::current();
        let landing = LANDING.load(Relaxed);
        let hits: Vec<(usize, usize, u64)> = {
            let read = READ.lock().unwrap_or_else(|e| e.into_inner());
            let Some(map) = read.as_ref() else { return };
            let end = at + new.len();
            (at / PIECE..=(end - 1) / PIECE)
                .filter_map(|piece| map.get(&piece))
                .flat_map(|p| p.fills.iter().copied())
                .filter(|f| f.0 < end && f.1 > at && f.2 + KEEP >= frame && (landing == 0 || landing <= f.3))
                .map(|f| (f.0, f.1, f.2))
                .collect()
        };
        if hits.is_empty() {
            return;
        }
        COUNT[1].fetch_add(1, Relaxed);
        let mut now = Vec::new();
        for (from, to, read_in) in hits {
            let (a, b) = (from.max(at), to.min(at + new.len()));
            now.clear();
            now.resize(b - a, 0);
            // Title memory as it is, which is what the fill read unless the
            // title wrote it since; not there: nothing to say.
            if !super::read_into(a as *const u8, &mut now) || now[..] == new[a - at..b - at] {
                continue;
            }
            let n = COUNT[2].fetch_add(1, Relaxed) + 1;
            let first = now.iter().zip(&new[a - at..b - at]).position(|(x, y)| x != y).unwrap_or(0);
            if !WAIT.swap(true, Relaxed) {
                tracing::warn!(
                    "a fill of upload memory read {} bytes at {from:#x} in frame {read_in} ahead of a flight whose shaders changed \
                     them (first at {:#x}, landed in frame {frame}): the lists of that submission drew with the bytes from before. \
                     Fills wait for flights again from here on, as with VICTORY_FILL_WAIT=1 ({n} such fills)",
                    to - from,
                    a + first
                );
            }
            return;
        }
    }

    /// With the frame line.
    pub(super) fn line(frames: f64) {
        let [ahead, looked] = [0, 1].map(|i| COUNT[i].swap(0, Relaxed));
        if ahead == 0 && looked == 0 {
            return;
        }
        let kept = {
            let frame = crate::frame::current();
            let mut read = READ.lock().unwrap_or_else(|e| e.into_inner());
            read.as_mut().map_or(0, |map| {
                map.retain(|_, p| {
                    p.fills.retain(|f| f.2 + KEEP >= frame);
                    !p.fills.is_empty()
                });
                map.values().map(|p| p.fills.len()).sum::<usize>()
            })
        };
        tracing::info!(
            "fills of upload memory that read title memory flights may write, not waited for (VICTORY_FILL_WAIT=1 waits): {:.1} per \
             frame; {:.1} landings' writes per frame fell in the same 64 KiB as one and were compared with it; fills that read \
             bytes a flight then changed, since the start: {}{}; {kept} fills kept to check",
            ahead as f64 / frames,
            looked as f64 / frames,
            COUNT[2].load(Relaxed),
            if WAIT.load(Relaxed) { " (fills wait again)" } else { "" }
        );
    }
}

pub(crate) use fill_ahead::wrote as shaders_write;

/// Diagnostics: the fills of upload memory that made a submission wait for
/// flights (`blocked_by`), which one they are, and whether the wait was
/// needed: the bytes the fill reads are kept when it blocks and compared
/// with what it reads once the flights have landed.
///
/// Runs 202 and 206: one such wait a frame, 56 to 95 ms of the submitting
/// thread standing still, the largest single wait of the frame. The ranges
/// flights "may write" are whole views shaders can store to; if the bytes
/// a fill reads never change under the wait, the wait is for nothing.
#[cfg(feature = "diagnostics")]
mod fill_check {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    /// The fill waited for now: source, length, its bytes when it blocked.
    static PENDING: Mutex<Option<(usize, usize, Vec<u8>)>> = Mutex::new(None);
    /// Fills that blocked by (source page, length): how often.
    static SEEN: Mutex<Vec<(usize, usize, u64)>> = Mutex::new(Vec::new());
    /// Fills checked after their wait; those whose bytes had changed; the
    /// bytes that had.
    static CHECKED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

    /// A fill of `len` bytes at `src` blocks on `written`.
    pub(super) fn blocked(written: &[(usize, usize)], src: usize, len: usize) {
        let at = written.partition_point(|r| r.1 <= src);
        let touched = written.get(at).copied().unwrap_or((0, 0));
        let key = (src & !0xfff, len);
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        let known = seen.iter().position(|s| (s.0, s.1) == key);
        match known {
            Some(k) => seen[k].2 += 1,
            None if seen.len() < 256 => {
                seen.push((key.0, key.1, 1));
                if seen.len() <= 12 {
                    tracing::info!(
                        "a fill of upload memory waits for flights: {len} bytes at {src:#x}; the range shaders may write that it                          touches: {:#x}+{:#x} ({} ranges in the air); frame {}",
                        touched.0,
                        touched.1 - touched.0,
                        written.len(),
                        crate::frame::current()
                    );
                }
            }
            None => {}
        }
        drop(seen);
        let mut bytes = vec![0u8; len.min(1 << 20)];
        if super::read_into(src as *const u8, &mut bytes) {
            *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((src, len, bytes));
        }
    }

    /// The fill of `len` bytes at `src` runs now.
    pub(super) fn runs(src: usize, len: usize) {
        let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        if !pending.as_ref().is_some_and(|p| (p.0, p.1) == (src, len)) {
            return;
        }
        let Some((_, _, before)) = pending.take() else { return };
        drop(pending);
        let mut now = vec![0u8; before.len()];
        if !super::read_into(src as *const u8, &mut now) {
            return;
        }
        let differ = before.iter().zip(&now).filter(|(a, b)| a != b).count() as u64;
        let n = CHECKED[0].fetch_add(1, Relaxed) + 1;
        if differ > 0 {
            let bad = CHECKED[1].fetch_add(1, Relaxed) + 1;
            CHECKED[2].fetch_add(differ, Relaxed);
            if bad <= 12 {
                let first = before.iter().zip(&now).position(|(a, b)| a != b).unwrap_or(0);
                tracing::info!(
                    "a fill of upload memory that waited for flights read other bytes after the wait: {differ} of {len} bytes at                      {src:#x} changed, the first at +{first:#x} (check {n}, frame {})",
                    crate::frame::current()
                );
            }
        }
    }

    /// The title buffer copy waited for now: destination, source, length,
    /// the bytes of both when it blocked.
    static PENDING_COPY: Mutex<Option<(usize, usize, usize, Vec<u8>, Vec<u8>)>> = Mutex::new(None);
    /// Copies checked after their wait; those whose source had changed;
    /// those whose destination had.
    static COPIES: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
    /// Copies that blocked by (source page, destination page, length).
    static SEEN_COPIES: Mutex<Vec<(usize, usize, usize, u64)>> = Mutex::new(Vec::new());

    /// A title buffer copy of `len` bytes from `src` to `dst` blocks on
    /// `written`.
    pub(super) fn blocked_copy(written: &[(usize, usize)], dst: usize, src: usize, len: usize) {
        let by_src = super::touches(written, src, src + len);
        let by_dst = super::touches(written, dst, dst + len);
        let key = (src & !0xfff, dst & !0xfff, len);
        let mut seen = SEEN_COPIES.lock().unwrap_or_else(|e| e.into_inner());
        let known = seen.iter().position(|s| (s.0, s.1, s.2) == key);
        match known {
            Some(k) => seen[k].3 += 1,
            None if seen.len() < 256 => {
                seen.push((key.0, key.1, key.2, 1));
                if seen.len() <= 16 {
                    let range = |at: usize| {
                        let k = written.partition_point(|r| r.1 <= at);
                        written.get(k).copied().unwrap_or((0, 0))
                    };
                    let (s, d) = (range(src), range(dst));
                    tracing::info!(
                        "a title buffer copy waits for flights: {len} bytes from {src:#x} to {dst:#x}; source in a range shaders may                          write: {by_src} ({:#x}+{:#x}); destination: {by_dst} ({:#x}+{:#x}); {} ranges in the air; frame {}",
                        s.0,
                        s.1 - s.0,
                        d.0,
                        d.1 - d.0,
                        written.len(),
                        crate::frame::current()
                    );
                }
            }
            None => {}
        }
        drop(seen);
        let n = len.min(1 << 20);
        let (mut a, mut b) = (vec![0u8; n], vec![0u8; n]);
        if super::read_into(src as *const u8, &mut a) && super::read_into(dst as *const u8, &mut b) {
            *PENDING_COPY.lock().unwrap_or_else(|e| e.into_inner()) = Some((dst, src, len, a, b));
        }
    }

    /// The title buffer copy runs now.
    pub(super) fn copy_runs(dst: usize, src: usize, len: usize) {
        let mut pending = PENDING_COPY.lock().unwrap_or_else(|e| e.into_inner());
        if !pending.as_ref().is_some_and(|p| (p.0, p.1, p.2) == (dst, src, len)) {
            return;
        }
        let Some((_, _, _, src_before, dst_before)) = pending.take() else { return };
        drop(pending);
        let (mut a, mut b) = (vec![0u8; src_before.len()], vec![0u8; dst_before.len()]);
        if !super::read_into(src as *const u8, &mut a) || !super::read_into(dst as *const u8, &mut b) {
            return;
        }
        let n = COPIES[0].fetch_add(1, Relaxed) + 1;
        let (src_changed, dst_changed) = (a != src_before, b != dst_before);
        if src_changed {
            COPIES[1].fetch_add(1, Relaxed);
        }
        if dst_changed {
            COPIES[2].fetch_add(1, Relaxed);
        }
        if (src_changed || dst_changed) && COPIES[1].load(Relaxed) + COPIES[2].load(Relaxed) <= 16 {
            let differ = |x: &[u8], y: &[u8]| x.iter().zip(y).filter(|(p, q)| p != q).count();
            tracing::info!(
                "a title buffer copy that waited for flights found other bytes after the wait: {len} bytes from {src:#x} to {dst:#x};                  {} source bytes changed, {} destination bytes changed (check {n}, frame {})",
                differ(&a, &src_before),
                differ(&b, &dst_before),
                crate::frame::current()
            );
        }
    }

    /// With the frame line.
    pub(super) fn line() {
        let copies = COPIES[0].load(Relaxed);
        if copies > 0 {
            let seen = SEEN_COPIES.lock().unwrap_or_else(|e| e.into_inner());
            let mut most: Vec<_> = seen.clone();
            most.sort_unstable_by_key(|s| std::cmp::Reverse(s.3));
            let most: Vec<String> =
                most.iter().take(6).map(|s| format!("{:#x}->{:#x}+{:#x} {}x", s.0, s.1, s.2, s.3)).collect();
            tracing::info!(
                "title buffer copies that waited for flights, since the start: {copies} checked after their wait; the source had                  other bytes after it {} times, the destination {} times; {} distinct copies, the most frequent: {}",
                COPIES[1].load(Relaxed),
                COPIES[2].load(Relaxed),
                seen.len(),
                most.join(", ")
            );
        }
        let n = CHECKED[0].load(Relaxed);
        if n == 0 {
            return;
        }
        let seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        let mut most: Vec<_> = seen.clone();
        most.sort_unstable_by_key(|s| std::cmp::Reverse(s.2));
        let most: Vec<String> = most.iter().take(6).map(|s| format!("{:#x}+{:#x} {}x", s.0, s.1, s.2)).collect();
        tracing::info!(
            "fills of upload memory that waited for flights, since the start: {n} checked after their wait, {} of them read other              bytes than before it ({} bytes in all); {} distinct fills (page, length), the most frequent: {}",
            CHECKED[1].load(Relaxed),
            CHECKED[2].load(Relaxed),
            seen.len(),
            most.join(", ")
        );
    }
}

/// With flights in the air, after the submission's write-watch asking:
/// whether `runs` refresh a range of a host copy that a flight's shaders
/// may write and that the title wrote since it was last copied. The
/// refresh leaves ranges shaders are writing alone (`Shadow::update`), so
/// the title's bytes there would reach the card a submission late.
fn stale_under_flights(runs: &[&HostList]) -> bool {
    use std::sync::Arc;
    // Per copy, the span of what is refreshed (a flush brings that much
    // up to date at most).
    let mut spans: Vec<(Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
    for list in runs {
        for step in &list.deferred().copies {
            if let Step::Refresh(shadow, address, len) = step
                && shadow.writes_pending()
            {
                match spans.iter_mut().find(|s| Arc::ptr_eq(&s.0, shadow)) {
                    Some(s) => (s.1, s.2) = (s.1.min(*address), s.2.max(*address + *len)),
                    None => spans.push((shadow.clone(), *address, *address + *len)),
                }
            }
        }
    }
    if late_exact() {
        // The title's bytes are put in beside the flights' writes; a
        // settle is still due where that could not be done. At most this
        // many changed runs go up to copies in the card's memory, each a
        // copy of its own there.
        // (`VICTORY_MERGE_RUNS`: the cap. Run 218 had 500 to 5,400 changed
        // runs a frame of a few bytes each, and the cap of 512 settled the
        // flights twice a frame, 50 ms of the submitting thread.)
        // Live (`victory_core::live`, `VICTORY_AB`).
        static RUNS: victory_core::live::Switch = victory_core::live::Switch::new("MERGE_RUNS", 512);
        let mut runs_left = (RUNS.get() as usize).clamp(1, 1 << 16);
        return spans.iter().any(|(shadow, from, to)| shadow.merge_under_writes(*from, *to - *from, &mut runs_left));
    }
    spans.iter().any(|(shadow, from, to)| shadow.stale_under_writes(*from, *to - *from))
}

/// The host copies `runs` refresh that have shader writes pending
/// ([`read_settle_precise`]).
fn refreshed_copies(runs: &[&HostList]) -> Vec<std::sync::Arc<crate::shadow::Shadow>> {
    let mut out: Vec<std::sync::Arc<crate::shadow::Shadow>> = Vec::new();
    for list in runs {
        for step in &list.deferred().copies {
            if let Step::Refresh(shadow, ..) = step
                && shadow.writes_pending()
                && !out.iter().any(|s| std::sync::Arc::ptr_eq(s, shadow))
            {
                out.push(shadow.clone());
            }
        }
    }
    out
}

/// Whether the list that brings copies in the card's memory up to date
/// records its work in three steps over all the copies: every changed
/// range copied up, then every state change in one call, then every hash
/// pass (`VICTORY_CARD_SYNC_BATCH=1`; fork pull request #1227, where it is
/// on by default. Here: OFF unless set until a run of ours has carried it;
/// `alt`: twenty seconds with, twenty without, in turn, each change said in
/// the log).
///
/// Copy by copy, each with its copies, its state change and its hash
/// passes, the card went from copying to compute and back for every copy
/// of the list: about 300 times a frame with every title buffer's copy in
/// the card's memory (`VICTORY_CARD_BUFFERS_MIN_KB=64`), where the
/// send-and-hash lists took 24 to 28 ms of the card a frame for 28 MB sent
/// up and 600 pieces hashed. The same work in three steps took 5.3 to 5.7.
/// Nothing a hash pass reads is written by a later copy of the list: each
/// copy's ranges and pieces are its own buffer's.
fn sync_batched() -> bool {
    // Live (`victory_core::live`, `VICTORY_AB`): only how the next
    // send-and-hash list is recorded changes. `alt` is read at boot.
    static SWITCH: victory_core::live::Switch = victory_core::live::Switch::new("CARD_SYNC_BATCH", 0);
    static ALT: OnceLock<bool> = OnceLock::new();
    let alt = *ALT.get_or_init(|| std::env::var("VICTORY_CARD_SYNC_BATCH").as_deref() == Ok("alt"));
    match if alt { 2 } else { u8::from(SWITCH.on()) } {
        0 => false,
        1 => true,
        _ => {
            static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
            let on = (EPOCH.get_or_init(std::time::Instant::now).elapsed().as_secs() / 20) % 2 == 1;
            static LAST: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(2);
            if LAST.swap(u8::from(on), Ordering::Relaxed) != u8::from(on) {
                tracing::info!(
                    "the send-and-hash list records its copies, one state change and its hash passes in three steps from now (VICTORY_CARD_SYNC_BATCH=alt): {}",
                    if on { "ON" } else { "off, copy by copy as usual" }
                );
            }
            on
        }
    }
}

/// A fence signal on a queue ([`late`]): goes behind the flights in the
/// air, if there are any, and is set when they are settled. Whether it
/// did; if not the caller sets the fence now.
pub(crate) fn late_signal(fence: usize, value: u64) -> bool {
    if !late() {
        return false;
    }
    let mut line = line();
    if line.line.is_empty() && !line.settling {
        return false;
    }
    line.line.push(Late::Signal(fence, value));
    LATE_STATS[14].fetch_add(1, Ordering::Relaxed);
    drop(line);
    // A signal alone (behind a settle under way) has no flight to wake
    // the helper thread for it.
    LINE_MOVED.notify_one();
    RELAY_MOVED.notify_one();
    true
}

/// A queue stands at `wait`, which memory does not meet now ([`late`]):
/// whether it is met once the flights in the air have run. A flight's
/// bottom-of-pipe write of a value that meets it says yes without a wait
/// for the card: the part behind the wait goes to the card behind that
/// flight, which is the order the wait asks for. If the flights may write
/// the value in a way not known here (shaders, a timestamp, another
/// width), they are settled and memory is asked again.
pub(crate) fn late_met(wait: &crate::list::ValueWait) -> bool {
    let (address, bytes) = wait.place();
    {
        let line = line();
        if line.line.is_empty() && line.in_hand.is_empty() {
            return false;
        }
        let mut unsure = touches(&line.written, address, address + bytes);
        // A label the title writes only at the bottom of the pipe: declared
        // shader writes over it are not what writes it.
        if unsure && memwait_samequeue() && bytes == 8 && is_label(address) {
            RE1_STATS[1].fetch_add(1, Ordering::Relaxed);
            unsure = false;
        }
        // (With the relay: first the writes of flights being landed.)
        let writes = line.in_hand.iter().map(|(at, what, n)| (at, what, n)).chain(line.line.iter().filter_map(|item| match item {
            Late::Write(at, what, n) => Some((at, what, n)),
            _ => None,
        }));
        for (at, what, n) in writes {
            if *at >= address + bytes || address >= *at + *n {
                continue;
            }
            match what {
                crate::queue::Written::Value(v) if *at == address && *n >= bytes => {
                    if wait.holds_for(*v) {
                        LATE_STATS[13].fetch_add(1, Ordering::Relaxed);
                        return true;
                    }
                }
                _ => unsure = true,
            }
        }
        if !unsure {
            return false;
        }
    }
    if read_settle_precise() {
        // Until nothing in the air, nor a copy-back not done, writes its bytes.
        let started = std::time::Instant::now();
        let end = address + bytes;
        relay_wait(WHY_VALUE, |line| {
            !touches(&line.written, address, end)
                && !line.in_hand.iter().any(|&(at, _, n)| at < end && address < at + n)
                && !line.line.iter().any(|item| matches!(*item, Late::Write(at, _, n) if at < end && address < at + n))
        });
        RE1_STATS[2].fetch_add(1, Ordering::Relaxed);
        RE1_STATS[3].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        return wait.met();
    }
    settle_now(WHY_VALUE);
    wait.met()
}

/// Settles the flights in the air, if any ([`Host::settle`]), for reason
/// `why` (one of the `WHY_` places). Not from inside a submission.
pub(crate) fn settle_now(why: usize) {
    if !late() {
        return;
    }
    let Some(host) = host() else {
        return;
    };
    let mut submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
    host.settle(&mut submitted, why);
}

/// [`settle_now`] at a present, and for a fence the title blocks on.
pub(crate) fn settle_for_present() {
    settle_now(WHY_SYNC);
}
pub(crate) fn settle_for_fence() {
    settle_now(WHY_FENCE);
}

/// `VICTORY_FENCE_PRECISE=1` (off by default; with the relay,
/// `VICTORY_RELAY`): a title thread that blocks on a fence value a signal
/// in the line will set waits only until that signal is reached (the
/// flights before it have landed), not for everything in the air; later
/// flights keep running. Without such a signal it settles as before.
pub(crate) fn fence_precise() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_FENCE_PRECISE").is_some_and(|v| v == "1");
        if on {
            tracing::info!(relay = relay(), "a blocking fence wait waits only for its signal (VICTORY_FENCE_PRECISE=1)");
        }
        on
    })
}

/// Since the last frame-rate line (`VICTORY_FENCE_PRECISE`): fence waits
/// that waited for their signal only, microseconds they waited, fence
/// waits settled as before (no signal of the value in the line).
pub(crate) static FENCE_STATS: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// A title thread blocks until fence `fence` (the title's object) reaches
/// `value` ([`fence_precise`]): if a signal that does stands in the line,
/// waits until `reached` says so, looked at each time flights land.
/// Whether it waited that way; if not the caller settles.
pub(crate) fn wait_fence_signal(fence: usize, value: u64, reached: impl Fn() -> bool) -> bool {
    if !fence_precise() || !relay() {
        return false;
    }
    let queued = line()
        .line
        .iter()
        .any(|item| matches!(item, Late::Signal(f, v) if *f == fence && *v >= value));
    if !queued {
        FENCE_STATS[2].fetch_add(1, Ordering::Relaxed);
        return false;
    }
    let started = std::time::Instant::now();
    relay_wait(WHY_FENCE, |_| reached());
    // A ring of one, read in the same frame, might have been taken for a
    // ring of three ([`copy_back_async`]).
    async_drain_all();
    FENCE_STATS[0].fetch_add(1, Ordering::Relaxed);
    FENCE_STATS[1].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    true
}


/// `VICTORY_MEMWAIT_SAMEQUEUE=1` (off by default; with [`late`]): a queue's
/// wait whose writer was already sent to the one in-order host queue is
/// passed instead of held or settled for.
///
/// - A fence wait (`Wait`) whose fence a signal in the line will set to the
///   value: the signal goes behind flights already sent, so what follows
///   the wait, sent now, runs after them on the card anyway (FRAMELOOP
///   §1.3: every title queue lands on one host queue in submission order).
/// - A wait for a memory value at an address the title only ever writes
///   with bottom-of-pipe writes (its cross-queue labels, slots 0x238/0x240):
///   a flight's shaders said to write a range holding it (a whole heap
///   declared) do not make the answer unsure. The flights' own
///   bottom-of-pipe writes still answer as before.
pub(crate) fn memwait_samequeue() -> bool {
    // Live (`victory_core::live`, `VICTORY_AB`): decided per wait.
    static ON: victory_core::live::Switch = victory_core::live::Switch::new("MEMWAIT_SAMEQUEUE", 0);
    late() && live_said(&ON, "waits whose writer was sent ahead on the host queue are passed")
}

/// Since the last frame-rate line: 0 fence waits passed behind a signal in
/// the line, 1 memory-value waits not settled for (a label under declared
/// shader writes), 2 waits settled only until their writers landed
/// (`VICTORY_READ_SETTLE_PRECISE`), 3 microseconds of those, 4 refreshes
/// under shader writes waited for their copies only, 5 microseconds of
/// those, 6 copy-backs done beside the landing ([`copy_back_async`]), 7
/// their bytes, 8 microseconds they took on their thread, 9 waits for them,
/// 10 microseconds of those waits, 11 copy-backs not done yet at the line.
pub(crate) static RE1_STATS: [std::sync::atomic::AtomicU64; 12] = [const { std::sync::atomic::AtomicU64::new(0) }; 12];

/// Title memory written with bottom-of-pipe writes of a value
/// ([`memwait_samequeue`]): the title's labels.
static LABELS: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

/// A list's bottom-of-pipe write of a value at `at` ([`memwait_samequeue`]).
pub(crate) fn note_label(at: usize) {
    if memwait_samequeue() {
        LABELS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(at);
    }
}

fn is_label(at: usize) -> bool {
    LABELS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|l| l.contains(&at))
}

/// Whether a signal in the line sets `fence` to `value` or more
/// ([`memwait_samequeue`]): what waits for it may be sent behind it.
pub(crate) fn late_signal_queued(fence: usize, value: u64) -> bool {
    if !memwait_samequeue() {
        return false;
    }
    let queued = line().line.iter().any(|item| matches!(item, Late::Signal(f, v) if *f == fence && *v >= value));
    if queued {
        RE1_STATS[0].fetch_add(1, Ordering::Relaxed);
    }
    queued
}

/// `VICTORY_READ_SETTLE_PRECISE=1` (off by default; with the relay): the
/// settles `VICTORY_SYNC_PRECISE` and `VICTORY_RELAY=2` leave whole wait
/// only for the flights that write what they look at.
///
/// - A wait for a memory value the flights may write in a way not known
///   ([`late_met`]: shaders, another width) waits until no flight in the
///   air (nor a copy-back not done) writes the value's bytes, then asks
///   memory: before, everything in the air was settled (WHY_VALUE).
/// - A submission refreshing title memory a flight's shaders may write
///   while the title wrote it too (WHY_STALE) waits until the copies it
///   refreshes have no shader writes pending, then looks again; before,
///   everything in the air was settled.
///
/// Read settles of buffer copies, fills and textures are per flight already
/// with `VICTORY_RELAY=2` or `VICTORY_SYNC_PRECISE=1` (with the relay); a
/// step of ours run on the CPU (WHY_RUN) may touch anything and still
/// settles everything.
pub(crate) fn read_settle_precise() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = relay() && std::env::var_os("VICTORY_READ_SETTLE_PRECISE").is_some_and(|v| v == "1");
        if on {
            tracing::info!("value and stale-copy settles wait only for the flights that write what they read (VICTORY_READ_SETTLE_PRECISE=1)");
        }
        on
    })
}

/// Landings and copy-backs done so far ([`read_settle_precise`]'s waits
/// look again at each).
static LANDINGS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// [`read_settle_precise`] for WHY_STALE: waits until none of `copies` has
/// shader writes pending, a landing at a time. False if it could not tell
/// (nothing left in the air and still pending): the caller settles.
fn wait_copies_landed(copies: &[std::sync::Arc<crate::shadow::Shadow>]) -> bool {
    let started = std::time::Instant::now();
    loop {
        if copies.iter().all(|c| !c.writes_pending()) {
            break;
        }
        let seen = LANDINGS.load(Ordering::Acquire);
        let empty = std::cell::Cell::new(false);
        relay_wait(WHY_STALE, |line| {
            empty.set(line.line.is_empty() && !line.settling && async_idle());
            LANDINGS.load(Ordering::Acquire) != seen || empty.get()
        });
        if empty.get() && copies.iter().any(|c| c.writes_pending()) {
            return false;
        }
    }
    RE1_STATS[4].fetch_add(1, Ordering::Relaxed);
    RE1_STATS[5].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    true
}

/// `VICTORY_COPY_BACK_ASYNC=1` with the relay ([`crate::readback::async_on`]).
pub(crate) fn copy_back_async() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| relay() && crate::readback::async_on())
}

/// Presents before which the copy-backs of a flight are done
/// (`VICTORY_COPY_BACK_ASYNC_LAG`, 1: a flight that landed before present
/// N is copied back before present N + 1 goes out, so the title, reading
/// the slot written two frames earlier, finds it there).
fn async_lag() -> u64 {
    static LAG: OnceLock<u64> = OnceLock::new();
    *LAG.get_or_init(|| {
        std::env::var("VICTORY_COPY_BACK_ASYNC_LAG").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(1).min(2)
    })
}

/// One copy-back left to [`async_worker`]: the copy, its declared ranges
/// joined over the flights landed together, and each flight's ranges (for
/// `copy_back_done`, flight by flight).
#[derive(Default)]
struct AsyncWork {
    copy: Option<std::sync::Arc<crate::shadow::Shadow>>,
    declared: Vec<(usize, usize)>,
    flights: Vec<Vec<(usize, usize)>>,
}

static ASYNC: Mutex<victory_core::readring::Backs<AsyncWork>> = Mutex::new(victory_core::readring::Backs::new());
/// Told when a copy-back is queued (the worker) or done (who waits).
static ASYNC_MOVED: std::sync::Condvar = std::sync::Condvar::new();
static ASYNC_DONE: std::sync::Condvar = std::sync::Condvar::new();
/// Presents made, for [`async_present`].
static PRESENTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn async_backs() -> std::sync::MutexGuard<'static, victory_core::readring::Backs<AsyncWork>> {
    ASYNC.lock().unwrap_or_else(|e| e.into_inner())
}

fn async_idle() -> bool {
    !copy_back_async() || async_backs().idle()
}

/// Waits until copy-back `id` and all before it are done.
fn async_wait(id: Option<u64>) {
    let Some(id) = id else {
        return;
    };
    let mut backs = async_backs();
    if backs.is_done(id) {
        return;
    }
    let started = std::time::Instant::now();
    while !backs.is_done(id) {
        backs = ASYNC_DONE.wait(backs).unwrap_or_else(|e| e.into_inner());
    }
    RE1_STATS[9].fetch_add(1, Ordering::Relaxed);
    RE1_STATS[10].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
}

/// Before memory `[from, to)` is let go: its copy-backs are done.
pub(crate) fn async_drain_range(from: usize, to: usize) {
    if copy_back_async() {
        let due = async_backs().due_for_range(from, to);
        async_wait(due);
    }
}

/// Every copy-back is done (a blocking fence wait: a ring of one read in
/// the same frame might have been taken for a ring of three).
pub(crate) fn async_drain_all() {
    if copy_back_async() {
        let due = async_backs().due_all();
        async_wait(due);
    }
}

/// At a present ([`copy_back_async`]): the copy-backs of flights that
/// landed [`async_lag`] presents ago or earlier are done first.
pub(crate) fn async_present() {
    let now = PRESENTS.fetch_add(1, Ordering::Relaxed) + 1;
    if copy_back_async() {
        let due = async_backs().due_before_present(now, async_lag());
        async_wait(due);
    }
}

/// Queues the copy-backs of `ring` (each copy's ranges joined, and per
/// flight) for [`async_worker`]; under no lock of the line.
fn async_queue(ring: Vec<AsyncWork>) {
    static WORKER: std::sync::Once = std::sync::Once::new();
    WORKER.call_once(|| {
        if let Err(e) = std::thread::Builder::new().name("victory-copy-back".into()).spawn(async_worker) {
            fail(format_args!("no thread for copy-backs beside landing (VICTORY_COPY_BACK_ASYNC): {e}"));
        }
    });
    let present = PRESENTS.load(Ordering::Relaxed);
    let mut backs = async_backs();
    for work in ring {
        let Some(copy) = &work.copy else {
            continue;
        };
        let owner = std::sync::Arc::as_ptr(copy) as usize;
        let ranges = work.declared.clone();
        backs.push(present, owner, ranges, work);
    }
    drop(backs);
    ASYNC_MOVED.notify_all();
}

/// The thread that copies back the read-back rings ([`copy_back_async`]):
/// in the order queued, each as the landing would have (`relay_hand_back`,
/// then the ranges let go flight by flight), then the line's written ranges
/// are made again without it and waiters are told.
fn async_worker() {
    crate::cores::keep_on_fast_cores("victory-copy-back");
    loop {
        let back = {
            let mut backs = async_backs();
            loop {
                if let Some(b) = backs.take() {
                    break b;
                }
                backs = ASYNC_MOVED.wait(backs).unwrap_or_else(|e| e.into_inner());
            }
        };
        let started = std::time::Instant::now();
        let before = COPY_BACK[1].load(Ordering::Relaxed);
        if let Some(copy) = &back.work.copy {
            relay_hand_back(&[(copy.clone(), back.work.declared.clone())]);
            for ranges in &back.work.flights {
                copy.copy_back_done(ranges);
            }
        }
        RE1_STATS[6].fetch_add(1, Ordering::Relaxed);
        RE1_STATS[7].fetch_add(COPY_BACK[1].load(Ordering::Relaxed).saturating_sub(before), Ordering::Relaxed);
        RE1_STATS[8].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        async_backs().finish(back.id);
        ASYNC_DONE.notify_all();
        {
            let mut line = line();
            rewrite_written(&mut line);
        }
        LANDINGS.fetch_add(1, Ordering::Release);
        LINE_MOVED.notify_all();
        LINE_LANDED.notify_all();
    }
}

/// The line's `written` and `by_copy` made again from the flights in the
/// air, and the copy-backs not done yet ([`copy_back_async`]).
fn rewrite_written(line: &mut Line) {
    let Line { line: items, written, by_copy, lists, .. } = line;
    written.clear();
    by_copy.clear();
    *lists = 0;
    let mut ranges = Vec::new();
    let mut copy_ranges = Vec::new();
    for item in items.iter() {
        if let Late::Run(flight) = item {
            *lists += flight.order.len().max(1);
            for (shadow, r) in &flight.back {
                let owner = std::sync::Arc::as_ptr(shadow) as usize;
                ranges.extend(r.iter().copied());
                copy_ranges.extend(r.iter().map(|&(a, b)| (owner, a, b)));
            }
            ranges.extend(flight.backs.iter().copied());
            copy_ranges.extend(flight.backs.iter().map(|&(a, b)| (0, a, b)));
        }
    }
    if copy_back_async() {
        for (owner, a, b) in async_backs().written() {
            ranges.push((a, b));
            copy_ranges.push((owner, a, b));
        }
    }
    join_ranges(written, ranges);
    join_copy_ranges(by_copy, copy_ranges);
}

/// The frame line of the switches above.
fn re1_line(frames: f64) {
    let s = RE1_STATS.each_ref().map(|a| a.swap(0, Ordering::Relaxed) as f64 / frames);
    if !(memwait_samequeue() || read_settle_precise() || copy_back_async()) {
        return;
    }
    let left = if copy_back_async() { async_backs().len() } else { 0 };
    tracing::info!(
        "fewer settles per frame: fence waits passed behind a signal in the line {:.1}, value waits not settled for a \
         label {:.1} (VICTORY_MEMWAIT_SAMEQUEUE={}); value waits settled for their writers only {:.1} in {:.2} ms, \
         refreshes waiting for their copies only {:.1} in {:.2} ms (VICTORY_READ_SETTLE_PRECISE={}); read-back \
         copy-backs beside the landing {:.1} of {:.2} MB, {:.2} ms on their thread, {:.1} waits for them {:.2} ms, \
         {left} not done now (VICTORY_COPY_BACK_ASYNC={}, lag {} presents)",
        s[0],
        s[1],
        u8::from(memwait_samequeue()),
        s[2],
        s[3] / 1e3,
        s[4],
        s[5] / 1e3,
        u8::from(read_settle_precise()),
        s[6],
        s[7] / 1e6,
        s[8] / 1e3,
        s[9],
        s[10] / 1e3,
        u8::from(copy_back_async()),
        async_lag(),
    );
}

/// Resets the host lists of settled flights and keeps them for reuse, as
/// a queue does for the lists it ran (`crate::list::recycle`).
fn land() {
    let landed = std::mem::take(&mut line().landed);
    for (lists, order) in landed {
        crate::list::recycle(lists, &order);
    }
}

/// The thread that settles the flights once the card has run them all
/// ([`late`]): without it a bottom-of-pipe write would reach the title only
/// at the next call that settles, and a title thread that polls such a
/// value while the submitting thread waits for that thread would never
/// see it.
fn late_helper() {
    let Some(host) = host() else {
        return;
    };
    crate::cores::keep_on_fast_cores("victory-late-wait");
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        tracing::warn!("no event for the thread that settles submissions sent ahead");
        return;
    };
    loop {
        let target = {
            let mut line = line();
            loop {
                if line.poke {
                    line.poke = false;
                    break None;
                }
                // (With the relay the flights are another thread's.)
                if !relay() && !line.line.is_empty() && !line.settling {
                    break Some(line.target);
                }
                line = LINE_MOVED.wait(line).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some(target) = target else {
            land();
            crate::queue::release_held(false);
            continue;
        };
        // The card first, without the submission lock: the submitting
        // thread prepares and sends meanwhile.
        // SAFETY: our fence and event.
        unsafe {
            if host.fence.SetEventOnCompletion(target, event).is_ok() {
                WaitForSingleObject(event, INFINITE);
            }
        }
        {
            let mut submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
            // Something newer went up meanwhile: the card is waited for
            // again, the submitting thread is not held up for it.
            if line().target != target {
                continue;
            }
            host.settle(&mut submitted, WHY_CARD_DONE);
        }
        land();
        crate::queue::release_held(false);
    }
}

/// Starts the threads of [`late`], once: the one that settles flights when
/// the card has run them and looks at what queues hold, and with [`relay`]
/// the one that lands flights.
fn late_threads() {
    static HELPER: std::sync::Once = std::sync::Once::new();
    HELPER.call_once(|| {
        if let Err(e) = std::thread::Builder::new().name("victory-late-wait".into()).spawn(late_helper) {
            tracing::warn!(error = %e, "no thread to settle submissions sent ahead: they are settled by the calls that follow");
        }
        tracing::info!(
            most = late_most(),
            writes = late_writes(),
            "submissions are sent to the card without a wait for each (VICTORY_LATE_WAIT=1)"
        );
        if relay() {
            // Without it nothing would ever land.
            if let Err(e) = std::thread::Builder::new().name("victory-relay".into()).spawn(relay_worker) {
                fail(format_args!("no thread to land submissions sent ahead (VICTORY_RELAY=1): {e}"));
            }
            tracing::info!(
                exact = relay_exact(),
                "what shaders wrote is copied back by a thread of its own, beside the submitting thread (VICTORY_RELAY=1)"
            );
        }
    });
}

/// With [`relay`]: waits until `done` says so of the line, looked at each
/// time flights have landed. Counted as a settle for `why`, and its time
/// as waiting for the card. The caller may hold the submission lock: the
/// thread that lands flights never takes it.
fn relay_wait(why: usize, done: impl Fn(&Line) -> bool) {
    let mut line = line();
    if done(&line) {
        return;
    }
    LATE_STATS[1].fetch_add(1, Ordering::Relaxed);
    LATE_STATS[why].fetch_add(1, Ordering::Relaxed);
    let was_working = working(false);
    let waiting = std::time::Instant::now();
    while !done(&line) {
        line = LINE_LANDED.wait(line).unwrap_or_else(|e| e.into_inner());
    }
    drop(line);
    stood(waiting);
    working(was_working);
    let us = waiting.elapsed().as_micros() as u64;
    note_waited(why, us);
    submission_us(1, us);
}

/// What the thread that lands flights fetches card copies' pieces with
/// ([`relay`]): a list, a fence and an event of its own (the host's are
/// behind the submission lock), and the memory the pieces come down to.
struct RelayFetch {
    list: Prelude,
    fence: ID3D12Fence,
    event: HANDLE,
    /// The last value signalled on `fence`.
    sent: u64,
    /// Buffer, mapping, bytes.
    buffer: Option<(ID3D12Resource, usize, u64)>,
    /// A copy queue of the fetches' own ([`copy_fetch`]); `list` is of its type.
    queue: Option<ID3D12CommandQueue>,
}

/// Whether the relay fetches what shaders wrote on a copy queue of its own
/// (`VICTORY_COPY_FETCH=1`; off by default). On the queue everything else
/// runs on, a fetch stood behind all the work sent after the flights it
/// fetches for (169 ms of a 276 ms frame waiting for 98 MB), and with it
/// the landing, what waits for it, and the fences the title waits for.
///
/// A fetch here may run beside later work: it reads bytes that a later
/// flight's shaders or sends write meanwhile. Later shader writes merged
/// early are found merged by their own flight's landing, as when a fetch
/// stood behind them. Title bytes: only the sends of the flights that have
/// run are known to have arrived, so every later send's ranges are left
/// out of the merge ([`CardPlan::sent_as`]).
fn copy_fetch() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COPY_FETCH").is_some_and(|v| v == "1"))
}

/// The thread that lands flights ([`relay`]): as soon as the card has run
/// the first flights of the line it takes them, with what follows them,
/// and does what they left to do ([`Host::relay_land`]); else it waits for
/// the card, or for a flight. It takes neither the submission lock nor
/// anything a caller of [`Host::settle`] may hold: resetting the landed
/// lists and looking at what queues hold are left to [`late_helper`].
fn relay_worker() {
    let Some(host) = host() else {
        return;
    };
    crate::cores::keep_on_fast_cores("victory-relay");
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        fail(format_args!("no event for the thread that lands submissions sent ahead (VICTORY_RELAY=1)"));
    };
    let mut fetch: Option<RelayFetch> = None;
    loop {
        let next = {
            let mut line = line();
            loop {
                if line.line.is_empty() {
                    line = RELAY_MOVED.wait(line).unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                let fences: Vec<Option<u64>> = line
                    .line
                    .iter()
                    .map(|l| match l {
                        Late::Run(flight) => Some(flight.fence),
                        _ => None,
                    })
                    .collect();
                // SAFETY: a plain query.
                let completed = unsafe { host.fence.GetCompletedValue() };
                let n = landable(&fences, completed);
                if n == 0 {
                    // The first thing is a flight the card has not run.
                    break Err(fences[0].unwrap_or(0));
                }
                line.settling = true;
                let items: Vec<Late> = line.line.drain(..n).collect();
                line.in_hand = items
                    .iter()
                    .filter_map(|l| match l {
                        Late::Write(at, what, bytes) => Some((*at, *what, *bytes)),
                        _ => None,
                    })
                    .collect();
                break Ok(items);
            }
        };
        match next {
            Ok(items) => host.relay_land(items, &mut fetch),
            Err(target) => {
                let waiting = std::time::Instant::now();
                // SAFETY: the host's fence and this thread's event.
                let waited = unsafe {
                    host.fence.SetEventOnCompletion(target, event).map(|()| {
                        WaitForSingleObject(event, INFINITE);
                    })
                };
                if waited.is_err() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                RELAY_STATS[2].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
        }
    }
}

/// [`relay`]: what shaders wrote to the declared ranges `back` of copies
/// in the PC's memory goes into title memory. As [`hand_back`], each byte
/// once, the pieces that differ found on several threads; but the merge is
/// `Shadow::relay_merge`, which does not write the copy, and the ranges
/// are not done with here (each flight's are, afterwards).
fn relay_hand_back(back: &[Declared]) {
    let mut work: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
    for (shadow, declared) in back {
        if shadow.carded() {
            continue;
        }
        let mut spans: Vec<(usize, usize)> = Vec::new();
        join_ranges(&mut spans, declared.iter().copied());
        for &(from, to) in &spans {
            shadow.relay_compared(from, to - from);
        }
        work.extend(spans.into_iter().map(|(from, to)| (shadow.clone(), from, to)));
    }
    if work.is_empty() {
        return;
    }
    #[cfg(feature = "diagnostics")]
    let scanning = std::time::Instant::now();
    // Looked through without a lock: a piece that changes meanwhile (a
    // later flight writing, the title's bytes going in) is found now or by
    // that flight's landing, and the merge looks again under the lock.
    let found = crate::shadow::find_all(&work);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::BackScanUs, scanning.elapsed().as_micros() as u64);
    let (mut now, mut title) = (Vec::new(), Vec::new());
    for ((shadow, from, to), found) in work.iter().zip(found) {
        let mut written = 0;
        for (at, n) in found {
            written += shadow.relay_merge(at, n, &mut now, &mut title);
        }
        copy_back(1, written as u64);
        #[cfg(feature = "diagnostics")]
        crate::copystat::compared(*from, to - from, written);
        let _ = (from, to);
    }
}

/// For the frame-rate line, with [`late`]: what the line of flights did
/// per frame since the last one.
fn late_line(frames: f64) {
    if !late() {
        return;
    }
    #[cfg(feature = "diagnostics")]
    fill_check::line();
    fill_ahead::line(frames);
    let s: Vec<f64> = LATE_STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    tracing::info!(
        "submissions sent ahead per frame: {:.1} in {:.1} settles ({:.1} flights each time), {:.1} ms waiting for the card in them; \
         settled for: a synchronous submission or present {:.1}, the card had run them {:.1}, a wait for a memory value {:.1}, \
         title memory changed under shader writes {:.1}, title memory read {:.1} (by a buffer copy {:.1}, a fill of upload memory {:.1}, \
         a texture {:.1}, another copy {:.1}), a step of ours {:.1}, {} lists in the air {:.1}, \
         bottom-of-pipe writes (VICTORY_LATE_WAIT_WRITES=0) {:.1}, a blocking fence wait {:.1}; \
         {:.1} waits for memory values met by a flight's bottom-of-pipe write, {:.1} fence signals set behind flights",
        s[0],
        s[1],
        if s[1] > 0.0 { s[11] / s[1] } else { 0.0 },
        s[12] / 1000.0,
        s[WHY_SYNC],
        s[WHY_CARD_DONE],
        s[WHY_VALUE],
        s[WHY_STALE],
        s[WHY_READ],
        s[15],
        s[16],
        s[17],
        s[18],
        s[WHY_RUN],
        late_most(),
        s[WHY_MOST],
        s[WHY_WRITES],
        s[WHY_FENCE],
        s[13],
        s[14],
    );
    {
        let w: Vec<f64> = LATE_WAITED.iter().map(|w| w.swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames).collect();
        tracing::info!(
            "of the waiting for the card in settles, per frame, by what settled: a synchronous submission or present {:.1} ms, \
             the card had run them {:.1}, a wait for a memory value {:.1}, title memory changed under shader writes {:.1}, \
             title memory read {:.1}, a step of ours {:.1}, lists in the air {:.1}, bottom-of-pipe writes {:.1}, a blocking fence wait {:.1}",
            w[WHY_SYNC],
            w[WHY_CARD_DONE],
            w[WHY_VALUE],
            w[WHY_STALE],
            w[WHY_READ],
            w[WHY_RUN],
            w[WHY_MOST],
            w[WHY_WRITES],
            w[WHY_FENCE],
        );
        // Which settles are the long ones: per reason, the settles a frame
        // that waited 8 ms or more and the longest wait since the last line.
        let long: Vec<f64> = LATE_LONG.iter().map(|n| n.swap(0, Ordering::Relaxed) as f64 / frames).collect();
        let most: Vec<f64> = LATE_MAX.iter().map(|n| n.swap(0, Ordering::Relaxed) as f64 / 1000.0).collect();
        let names = [
            (WHY_SYNC, "sync/present"),
            (WHY_CARD_DONE, "card done"),
            (WHY_VALUE, "value wait"),
            (WHY_STALE, "stale"),
            (WHY_READ, "read"),
            (WHY_RUN, "step of ours"),
            (WHY_MOST, "lists in the air"),
            (WHY_WRITES, "bottom-of-pipe writes"),
            (WHY_FENCE, "fence wait"),
        ];
        let parts: Vec<String> = names
            .iter()
            .filter(|(why, _)| most[*why] > 0.0)
            .map(|&(why, name)| format!("{name} {:.1} long, longest {:.1} ms", long[why], most[why]))
            .collect();
        tracing::info!("settles of 8 ms or more per frame, by what settled: {}", parts.join("; "));
    }
    if relay() {
        let r: Vec<f64> = RELAY_STATS.iter().map(|r| r.swap(0, Ordering::Relaxed) as f64 / frames).collect();
        let [again, noted] = [0, 1].map(|i| BESIDE_STATS[i].swap(0, Ordering::Relaxed) as f64 / frames);
        tracing::info!(
            "the relay per frame (VICTORY_RELAY={}): {:.1} flights landed in {:.1} landings by the thread that copies back, \
             beside the submitting thread; that thread waited {:.1} ms for the card with nothing to land and {:.1} ms for \
             fetches of card copies' pieces, and was {:.1} ms landing in all (the frame line's \"copying back\" is its time; \
             \"settles\" above are waits of other threads for it); pieces fetched on the copy queue beside later \
             submissions: {noted:.1}, pieces those fetched again at their own landing whatever the compare said: {again:.1} \
             (VICTORY_QUEUE2_REFETCH)",
            if relay_exact() { 2 } else { 1 },
            r[1],
            r[0],
            r[2] / 1000.0,
            r[3] / 1000.0,
            r[4] / 1000.0,
        );
        if inflight_fetch() {
            let i: Vec<f64> = INFLIGHT_STATS.iter().map(|i| i.swap(0, Ordering::Relaxed) as f64 / frames).collect();
            tracing::info!(
                "card copies' pieces fetched inside their flights per frame (VICTORY_CARD_FETCH_INFLIGHT=1): {:.1} pieces \
                 ({:.2} MB) merged with no fetch of their own; {:.1} landings fetched the old way (the wait above), {:.1} of \
                 them because a flight's pieces overflowed the slots",
                i[0],
                i[1] / (1 << 20) as f64,
                i[3],
                i[2],
            );
        }
    }
    if sync_precise() {
        let s: Vec<f64> =
            crate::texture::SYNC_STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
        tracing::info!(
            "precise waits per frame (VICTORY_SYNC_PRECISE=1): {:.1} submissions went on past staging buffers in the air \
             (each a wait for every flight before), {:.1} staging buffer writes waited for the submission copying from \
             them, {:.1} ms; hazards {:.1}",
            s[3],
            s[0],
            s[1] / 1000.0,
            s[2],
        );
    }
    if fence_precise() {
        let f: Vec<f64> = FENCE_STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
        tracing::info!(
            "fence waits per frame (VICTORY_FENCE_PRECISE=1): {:.1} waited for their signal only, {:.1} ms; {:.1} settled \
             everything (no signal in the line)",
            f[0],
            f[1] / 1000.0,
            f[2],
        );
    }
    if late_exact() {
        let [pages, runs, bytes] = crate::shadow::merged_stats().map(|n| n as f64 / frames);
        tracing::info!(
            "title bytes put in under flights' shader writes per frame (VICTORY_LATE_WAIT=2): {pages:.0} pages looked at, \
             {runs:.0} changed runs, {:.2} MB",
            bytes / (1 << 20) as f64
        );
        let w = crate::shadow::stale_why_stats().map(|n| n as f64 / frames);
        tracing::info!(
            "stale-copy settles per frame by why: not write-watched {:.1}, shader writes kept in the copy {:.1}, \
             journal could not say {:.1}, no snapshot {:.1}, changed-run cap {:.1}; kept-in-copy ranges put in \
             without a settle {:.1} (VICTORY_PIPE_OWNED_MERGE={})",
            w[0],
            w[1],
            w[2],
            w[3],
            w[4],
            w[5],
            crate::shadow::PIPE_OWNED_MERGE.get()
        );
    }
    crate::narrow::line(frames);
}

#[cfg(test)]
mod late_tests {
    use super::{continues, join_ranges, touches};

    /// Copies join only when both ends continue and the whole does not
    /// overlap itself.
    #[test]
    fn copies_join_only_end_to_end_and_apart() {
        // (destination, source, bytes)
        assert!(continues((1000, 5000, 16), (1016, 5016, 32)));
        assert!(continues((5000, 1000, 16), (5016, 1016, 32)));
        // A gap, or only one side continues.
        assert!(!continues((1000, 5000, 16), (1020, 5016, 32)));
        assert!(!continues((1000, 5000, 16), (1016, 5020, 32)));
        assert!(!continues((1000, 5000, 16), (1000, 5000, 16)));
        // Together they would overlap: the second reads what the first wrote.
        assert!(!continues((1000, 1016, 16), (1016, 1032, 16)));
        assert!(!continues((1016, 1000, 16), (1032, 1016, 16)));
        // They touch without overlapping.
        assert!(continues((1000, 1032, 16), (1016, 1048, 16)));
        assert!(!continues((usize::MAX - 8, 0, 8), (0, 8, 8)));
    }

    /// Ranges are found by what they hold, not by where they start, and a
    /// range that only touches another's end does not touch it.
    #[test]
    fn ranges_touch_where_they_overlap() {
        let mut ranges = Vec::new();
        join_ranges(&mut ranges, [(40, 50), (10, 20), (20, 25), (45, 60), (7, 7)]);
        assert_eq!(ranges, [(10, 25), (40, 60)]);
        join_ranges(&mut ranges, [(25, 40)]);
        assert_eq!(ranges, [(10, 60)]);
        let ranges = [(10usize, 25usize), (40, 60), (100, 101)];
        for (from, to, hit) in [
            (0, 10, false),
            (0, 11, true),
            (24, 25, true),
            (25, 40, false),
            (25, 41, true),
            (59, 200, true),
            (60, 100, false),
            (100, 101, true),
            (101, 500, false),
            (30, 30, false),
            (50, 50, false),
        ] {
            assert_eq!(touches(&ranges, from, to), hit, "{from}..{to}");
        }
        assert!(!touches(&[], 0, usize::MAX));
    }

    /// A texture staging buffer marked in flight for submission N blocks
    /// later flights until `FINISHED` reaches N.
    #[test]
    fn staging_in_flight_tracks_submission_completion() {
        let Some(host) = super::host() else { return };
        let heap_words: [u32; 5] = [1, 0, 0, 1, 1];
        let desc_words: [u32; 14] = [3, 0, 0, 0, 16, 0, 4, 65537, 28, 1, 0, 256, 0, 0];
        // SAFETY: plain repr(C) structs matching D3D12 layouts.
        let (heap, desc) = unsafe {
            (
                std::mem::transmute::<[u32; 5], crate::resource::HeapProperties>(heap_words),
                std::mem::transmute::<[u32; 14], crate::resource::ResourceDesc>(desc_words),
            )
        };
        let iid = crate::object::guid(
            0x696442be,
            0xa72e,
            0x4059,
            [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
        );
        let mut resource = std::ptr::null();
        assert_eq!(
            crate::resource::create_committed(
                std::ptr::null(),
                &heap,
                0,
                &desc,
                0,
                std::ptr::null(),
                &iid,
                &mut resource,
            ),
            crate::object::S_OK
        );
        let tex = crate::resource::host_resource(resource).expect("host texture");
        let list = host.new_list().expect("host list");
        crate::texture::upload(&list, resource, tex);
        assert!(!super::staging_in_flight_at(&[&list], 0));
        for step in &list.deferred().copies {
            if let super::Step::Texture(staging) = step {
                staging.mark_in_flight(10);
            }
        }
        assert!(super::staging_in_flight_at(&[&list], 9));
        assert!(!super::staging_in_flight_at(&[&list], 10));
        assert!(!super::staging_in_flight_at(&[&list], 11));
        list.close();
        drop(list);
        let release = crate::object::slot::<extern "system" fn(crate::object::This) -> u32>(
            resource, 0x10,
        );
        assert_eq!(release(resource), 0);
    }
}

/// Whether a title buffer copy that continues the one recorded before it
/// in its list becomes one step with it (`VICTORY_JOIN_COPIES=1`; off by
/// default).
///
/// Each copy is done at submission through the kernel, which reports a
/// page that is not there instead of faulting (`copy_mapped`), tells the
/// write watch and the guards, and ends the refreshes gathered so far: a
/// call into the kernel for some 480 bytes, 14,042 times a frame in run 86
/// (6.8 MB; "moves 19 ms" with the refreshes' own reads). How many of them
/// continue the one before is counted either way and said on its own line
/// with the frame rate; the switch is worth that many kernel calls.
///
/// Joined, the same bytes move: [`continues`] joins only copies whose
/// ranges, taken together, do not overlap, and nothing is recorded between
/// two steps that follow each other. What differs: a page that is not
/// mapped now fails the joined copy as a whole (it then goes page by page,
/// zeros for what cannot be read, as a single copy does), where before
/// only the copy that held it did.
fn join_copies() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_JOIN_COPIES").is_some_and(|v| v == "1"))
}

/// Since the last frame-rate line: title buffer copies recorded, and those
/// that continue the one before ([`join_copies`]).
static COPIES_JOINED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Since the last frame-rate line, what [`copy_mapped`] moved: bytes and
/// calls into host copies of ours ([0], [1]), and into title memory ([2],
/// [3]). The frame line's "copies ... MB" is the time of the first; this
/// says how the bytes are cut into calls.
pub(crate) static COPY_MOVED: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Whether the copy `next` (destination, source, bytes) goes on exactly
/// where `last` ended, source and destination both, and the two as one do
/// not overlap (source over destination): then one copy of both moves the
/// bytes the two would move in turn.
fn continues(last: (usize, usize, usize), next: (usize, usize, usize)) -> bool {
    let ((d, s, n), (dst, src, len)) = (last, next);
    let Some(all) = n.checked_add(len) else {
        return false;
    };
    d.checked_add(n) == Some(dst)
        && s.checked_add(n) == Some(src)
        && (d.checked_add(all).is_some_and(|end| end <= s) || s.checked_add(all).is_some_and(|end| end <= d))
}

/// How far apart two refreshes of one title buffer may be and still become
/// one copy (`RefreshRanges::merge`): a call moves 11 to 21 KB here, so 64 KB of extra
/// bytes is cheaper than one more kernel call.
const COPY_GAP: usize = 64 << 10;

/// The ranges the shaders of `runs` may write, per host copy, as declared.
/// One list's lock at a time: a submission may name a list twice.
fn declared_writes(runs: &[&HostList]) -> Vec<Declared> {
    let mut back: Vec<Declared> = Vec::new();
    // (copy, start, end) already in `back`: a set instead of a scan of the
    // copy's ranges for each of thousands of steps (Belobug PR #29).
    let mut seen: std::collections::HashSet<(usize, usize, usize)> = std::collections::HashSet::new();
    for list in runs {
        for step in &list.deferred().copies {
            if let Step::GpuWrite(shadow, address, len) = step {
                let range = (*address, *address + *len);
                if !seen.insert((std::sync::Arc::as_ptr(shadow) as usize, range.0, range.1)) {
                    continue;
                }
                match back.iter_mut().find(|(s, _)| std::sync::Arc::ptr_eq(s, shadow)) {
                    Some((_, ranges)) => ranges.push(range),
                    None => back.push((shadow.clone(), vec![range])),
                }
            }
        }
    }
    back
}

/// The walls trace (`VICTORY_WALLS_TRACE=1` or `2`; off by default): log
/// lines that say what the title's CPU was given while it decided whether
/// the camera is in a room. The values of the small read-backs
/// ([`read_backs`]), the frame's main pass by scissor rectangle with its
/// culling dispatches (`draw.rs`, `walls_census`), the shader-written
/// buffers merged into title memory (`shadow.rs`, `walls_merged`), host
/// copies found stale after their refresh (`shadow.rs`; only with
/// `VICTORY_REFRESH_CHECK=1` as well: it costs about 100 ms a frame), and, in the
/// runtime, the read requests that fail (with 2: every request).
pub(crate) fn walls_trace() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WALLS_TRACE").is_some_and(|v| v == "1"))
}

/// After `list` has run: the texels its read-backs copied into host
/// buffers of ours go to the title's memory (`HostList::defer_read_back`).
///
/// A submission that is waited for did this itself. One sent ahead
/// ([`late`]) did not, at any stage: the flights' lists were settled
/// without it, so the title read a buffer nobody wrote, as before read-backs
/// were made (the exposure metering's two small float targets, every frame).
fn read_backs(list: &HostList) {
    for (buffer, at, [row, rows, pitch]) in &list.deferred().backs {
        let mut data = std::ptr::null_mut();
        // SAFETY: a read-back buffer of ours, written by the list that has
        // run; the title's destination holds every row (checked when the
        // copy was recorded).
        unsafe {
            if buffer.Map(0, None, Some(&mut data)).is_ok() && !data.is_null() {
                // Copies compared with title memory (`VICTORY_MEM2_GPU_ONLY_NOSNAP`)
                // keep the bytes from before this write.
                if *rows > 0 {
                    crate::shadow::title_will_change(*at, (*rows - 1) * *pitch + *row);
                }
                let from = data.cast::<u8>().cast_const();
                for line in 0..*rows {
                    std::ptr::copy_nonoverlapping(from.add(line * row.next_multiple_of(256)), (*at as *mut u8).add(line * pitch), *row);
                }
                // The walls trace: what the title is handed (one row of at
                // most 24 values: its 5x1 and 1x1 float targets).
                if walls_trace() && *rows == 1 && *row <= 96 {
                    let mut values = [0f32; 24];
                    std::ptr::copy_nonoverlapping(from, values.as_mut_ptr().cast::<u8>(), *row & !3);
                    tracing::info!(
                        frame = crate::frame::current(),
                        to = format_args!("{:#x}", *at),
                        bytes = *row,
                        values = ?&values[..*row / 4],
                        "walls trace: a read-back lands in title memory"
                    );
                }
                buffer.Unmap(0, None);
            }
        }
    }
}

/// After the card ran what declared `back`: what shaders wrote there goes
/// into title memory, and the ranges are done with.
fn hand_back(back: &[Declared]) {
    if crate::shadow::wide_back() {
        // Every range first (the pieces that differ, found on several
        // threads), then the merges, in order, as below.
        let mut work: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
        for (shadow, declared) in back {
            if shadow.carded() {
                continue;
            }
            let mut sorted = declared.clone();
            sorted.sort_unstable();
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for (from, to) in sorted {
                match spans.last_mut() {
                    Some(last) if from <= last.1 => last.1 = last.1.max(to),
                    _ => spans.push((from, to)),
                }
            }
            work.extend(spans.into_iter().map(|(from, to)| (shadow.clone(), from, to)));
        }
        #[cfg(feature = "diagnostics")]
        let scanning = std::time::Instant::now();
        let found = crate::shadow::find_all(&work);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::BackScanUs, scanning.elapsed().as_micros() as u64);
        for ((shadow, from, to), found) in work.iter().zip(found) {
            let written = shadow.copy_back_with(*from, to - from, Some(found));
            copy_back(1, written as u64);
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(*from, to - from, written);
        }
        for (shadow, declared) in back {
            shadow.copy_back_done(declared);
        }
        return;
    }
    for (shadow, declared) in back {
        if shadow.carded() {
            // Done before (`Host::card_back`).
            shadow.copy_back_done(declared);
            continue;
        }
        // Each byte compared once: the world declares dozens of views
        // of its 174 MB heap that overlap almost entirely, and comparing
        // every one whole was gigabytes a frame (0.3-0.5 s) for the few
        // MB shaders wrote. Only ranges that overlap or touch are
        // joined: what lies between two was not given to shaders (the
        // snapshot does not hold it).
        let mut sorted = declared.clone();
        sorted.sort_unstable();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for (from, to) in sorted {
            match spans.last_mut() {
                Some(last) if from <= last.1 => last.1 = last.1.max(to),
                _ => spans.push((from, to)),
            }
        }
        for (from, to) in spans {
            let written = shadow.copy_back(from, to - from);
            copy_back(1, written as u64);
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(from, to - from, written);
        }
        shadow.copy_back_done(declared);
    }
}

impl Host {
    /// Whether the host runs mesh shaders at shader model 6.6 (translated
    /// mesh shaders need both); logged once.
    pub(crate) fn mesh_shaders(&self) -> bool {
        static MESH: OnceLock<bool> = OnceLock::new();
        *MESH.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
            let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
                HighestShaderModel: D3D_SHADER_MODEL_6_6,
            };
            // SAFETY: each query writes its own structure of the given size.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS7,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
                    && self
                        .device
                        .CheckFeatureSupport(
                            D3D12_FEATURE_SHADER_MODEL,
                            (&raw mut model).cast(),
                            size_of_val(&model) as u32,
                        )
                        .is_ok()
            };
            let mesh = ok
                && options.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED
                && model.HighestShaderModel.0 >= D3D_SHADER_MODEL_6_6.0;
            tracing::info!(mesh, "host mesh shader support");
            mesh
        })
    }

    /// A new list, open for recording.
    pub(crate) fn new_list(&self) -> windows::core::Result<HostList> {
        crate::census::made(8, 0);
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let before = private_bytes();
        let list = self.make_list();
        LISTS[0].fetch_add(1, Ordering::Relaxed);
        // (May shrink while a list is made, on another thread's account:
        // added as it is, the sum is what counts.)
        #[cfg(feature = "diagnostics")]
        LISTS[2].fetch_add(private_bytes().wrapping_sub(before), Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            add(C::HostLists, 1);
            add(C::HostListNs, started.elapsed().as_nanos() as u64);
        }
        list
    }

    /// An allocator and a closed list for a prelude.
    fn make_prelude(&self) -> Option<Prelude> {
        // SAFETY: plain object creation.
        let made: windows::core::Result<Prelude> = unsafe {
            (|| {
                let allocator: ID3D12CommandAllocator =
                    self.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                let list: ID3D12GraphicsCommandList =
                    self.device
                        .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                list.Close()?;
                Ok((allocator, list))
            })()
        };
        LISTS[1].fetch_add(1, Ordering::Relaxed);
        match made {
            Ok(prelude) => Some(prelude),
            Err(e) => {
                tracing::warn!(error = %e, "no host list for a prelude");
                None
            }
        }
    }

    fn make_list(&self) -> windows::core::Result<HostList> {
        // SAFETY: plain object creation.
        unsafe {
            let allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None,
            )?;
            let prelude = OnceLock::new();
            if !lazy_prelude() {
                let _ = prelude.set(self.make_prelude());
            }
            Ok(HostList {
                allocator,
                list,
                deferred: Mutex::default(),
                scratch: Mutex::default(),
                states: Mutex::default(),
                twins: Mutex::default(),
                aliases: Mutex::default(),
                broken: std::sync::atomic::AtomicBool::new(false),
                prelude,
                pipelines: Mutex::default(),
                budget: OnceLock::new(),
                card_budget: OnceLock::new(),
                indirect_args: Mutex::default(),
                uav_pending: std::sync::atomic::AtomicBool::new(true),
                hazards: Mutex::new(victory_core::sync::Hazards::new()),
                held_barriers: Mutex::default(),
                has_held_barriers: std::sync::atomic::AtomicBool::new(false),
                writes: Default::default(),
                clamp_chunks: Mutex::default(),
                _alive: Counted::new(),
                meter: AtomicU32::new(u32::MAX),
                gather: Mutex::default(),
                #[cfg(feature = "diagnostics")]
                notes: Mutex::default(),
                #[cfg(feature = "diagnostics")]
                stamps: OnceLock::new(),
            })
        }
    }

    /// The card's clock ([`CARD_CLOCK_US`]), made at the first send.
    fn card_clock(&self) -> Option<&'static CardClock> {
        static CLOCK: OnceLock<Option<CardClock>> = OnceLock::new();
        static WATCH: std::sync::Once = std::sync::Once::new();
        let clock = CLOCK
            .get_or_init(|| {
                // SAFETY: a plain fence on our device.
                let fence = unsafe { self.device.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE) }.ok()?;
                Some(CardClock {
                    fence,
                    state: Mutex::new((0, None)),
                    sent: std::sync::Condvar::new(),
                })
            })
            .as_ref()?;
        WATCH.call_once(|| {
            if let Err(e) = std::thread::Builder::new().name("victory-card-clock".into()).spawn(move || card_clock_watch(clock)) {
                tracing::warn!(error = %e, "no thread for the card's clock: the frame line's idle times stay 0");
            }
        });
        Some(clock)
    }

    /// Lists of ours just went to the queue: the clock's signal goes
    /// behind them, and an idle stretch of the card, if one was under way,
    /// ends here ([`CARD_CLOCK_US`]).
    fn card_sent(&self) {
        let Some(clock) = self.card_clock() else {
            return;
        };
        let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0 += 1;
        // SAFETY: our queue and the clock's fence.
        let _ = unsafe { self.queue.Signal(&clock.fence, state.0) };
        if let Some((since, worked_then)) = state.1.take() {
            card_clock_us(0, since.elapsed().as_micros() as u64);
            card_clock_us(1, worked().saturating_sub(worked_then));
        }
        drop(state);
        clock.sent.notify_one();
    }

    /// Whether the host has what copies in the card's memory need (three
    /// lists and the compare); made at the first ask. A copy goes to the
    /// card's memory only if it has.
    pub(crate) fn card_ready(&self) -> bool {
        static READY: OnceLock<bool> = OnceLock::new();
        *READY.get_or_init(|| match self.make_card_work() {
            Ok(work) => {
                *CARD_WORK.lock().unwrap_or_else(|e| e.into_inner()) = Some(work);
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "no host copies in the card's memory: the compare could not be made");
                false
            }
        })
    }

    fn make_card_work(&self) -> windows::core::Result<CardWork> {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let mut code: Option<ID3DBlob> = None;
        let source = format!("#define CARD_COMPARE_WIDE {}\n{CARD_COMPARE}", u8::from(compare_wide()));
        // SAFETY: the source and names live through the call; the out
        // pointer is ours.
        unsafe {
            D3DCompile(
                source.as_ptr().cast(),
                source.len(),
                windows::core::s!("card compare"),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )?;
        }
        let code = code.ok_or_else(windows::core::Error::empty)?;
        let list = || self.make_prelude().ok_or_else(windows::core::Error::empty);
        // A megabyte of results: 262,144 pieces, 16 GB of declared ranges
        // in one submission.
        const RESULTS: u64 = 1 << 20;
        let (differs, data) = self.shared_buffer(RESULTS).ok_or_else(windows::core::Error::empty)?;
        name(&differs, || "card compare results".into());
        // SAFETY: the blob's own bytes; the root signature is inside the
        // shader.
        unsafe {
            let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
            let root: ID3D12RootSignature = self.device.CreateRootSignature(0, bytes)?;
            let pso: ID3D12PipelineState = self.device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: bytes.as_ptr().cast(),
                    BytecodeLength: bytes.len(),
                },
                ..Default::default()
            })?;
            Ok(CardWork {
                sync: vec![list()?],
                check: vec![list()?],
                fetch: list()?,
                root,
                pso,
                differs: (differs, data as usize, RESULTS),
                fetched: None,
                taken: 0,
                out: 0,
                inflight: if inflight_fetch() { self.make_card_inflight(RESULTS) } else { None },
            })
        }
    }

    /// [`CardInflight`] for `results` bytes of the compare's results; `None`
    /// (and the old fetch) if the shader or the memory cannot be had.
    fn make_card_inflight(&self, results: u64) -> Option<CardInflight> {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let made = (|| -> windows::core::Result<CardInflight> {
            let mut code: Option<ID3DBlob> = None;
            // SAFETY: the source and names live through the call; the out
            // pointer is ours.
            unsafe {
                D3DCompile(
                    CARD_FETCH.as_ptr().cast(),
                    CARD_FETCH.len(),
                    windows::core::s!("card fetch in flight"),
                    None,
                    None,
                    windows::core::s!("main"),
                    windows::core::s!("cs_5_1"),
                    0,
                    0,
                    &mut code,
                    None,
                )?;
            }
            let code = code.ok_or_else(windows::core::Error::empty)?;
            let slots = inflight_slots(inflight_fetch_bytes());
            let (table, table_data) = self.shared_buffer(4 + results).ok_or_else(windows::core::Error::empty)?;
            name(&table, || "card fetch in flight: tickets".into());
            let fetched_bytes = u64::from(slots) * INFLIGHT_SLOT;
            let (fetched, fetched_data) = self.shared_buffer(fetched_bytes).ok_or_else(windows::core::Error::empty)?;
            name(&fetched, || "card fetch in flight: pieces".into());
            // SAFETY: the mapping holds the counter; nothing of ours ran
            // on the card with it yet.
            unsafe { table_data.cast::<u32>().write_volatile(0) };
            // SAFETY: the blob's own bytes; the root signature is inside
            // the shader.
            unsafe {
                let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
                let root: ID3D12RootSignature = self.device.CreateRootSignature(0, bytes)?;
                let pso: ID3D12PipelineState = self.device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: bytes.as_ptr().cast(),
                        BytecodeLength: bytes.len(),
                    },
                    ..Default::default()
                })?;
                Ok(CardInflight {
                    root,
                    pso,
                    table: (table, table_data as usize, 4 + results),
                    fetched: (fetched, fetched_data as usize, slots),
                    landed: 0,
                    brought: Vec::new(),
                })
            }
        })();
        match made {
            Ok(inflight) => {
                tracing::info!(
                    slots = inflight.fetched.2,
                    "card copies' flagged pieces come down inside their flights (VICTORY_CARD_FETCH_INFLIGHT=1)"
                );
                Some(inflight)
            }
            Err(e) => {
                tracing::warn!(error = %e, "no fetch inside flights (VICTORY_CARD_FETCH_INFLIGHT=1): pieces are fetched the old way");
                None
            }
        }
    }

    /// Before a submission's lists run, for copies in the card's memory
    /// ([`crate::shadow::cards`]): what changed on their CPU side since
    /// the last submission goes to their card side (a list of ours, run
    /// now; the submission's lists follow it on the queue), and the ranges
    /// this submission's shaders may write get a compare to run after its
    /// lists. `None` if there is nothing to do.
    ///
    /// `flown`: the submission is sent ahead with [`relay`]. Its compare
    /// then stores the hashes it makes (see [`flagged_pieces`] and
    /// `relay`).
    ///
    /// `sent`: the last value signalled on the host's fence; the caller
    /// signals the next one behind this submission's lists.
    ///
    /// With `VICTORY_QUEUE2` bit 2 (`crate::queue2`; off by default) the
    /// changed bytes cross from the PC's memory to the card's on a copy
    /// queue, on the card's copy engine, while the direct queue still runs
    /// what was sent before: they go into a landing buffer in the card's
    /// memory that nothing else uses. The direct queue is told to wait for
    /// that copy and then moves the bytes from the landing buffer into the
    /// copies, inside the card's memory, in the same place of its order as
    /// before. So the copies change exactly when they did on one queue; what
    /// leaves the direct queue is the crossing from the PC's memory.
    fn card_before(&self, runs: &[&HostList], flown: bool, sent: u64) -> Option<CardPlan> {
        use std::sync::Arc;
        if !crate::shadow::cards() {
            return None;
        }
        // The ranges this submission's shaders may write, per copy.
        let mut declared: Vec<(Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)> = Vec::new();
        let mut seen: std::collections::HashSet<(usize, usize, usize)> = std::collections::HashSet::new();
        for list in runs {
            for step in &list.deferred().copies {
                if let Step::GpuWrite(shadow, address, len) = step
                    && shadow.carded()
                {
                    let range = (*address, *address + *len);
                    if !seen.insert((Arc::as_ptr(shadow) as usize, range.0, range.1)) {
                        continue;
                    }
                    match declared.iter_mut().find(|(s, _)| Arc::ptr_eq(s, shadow)) {
                        Some((_, ranges)) => ranges.push(range),
                        None => declared.push((shadow.clone(), vec![range])),
                    }
                }
            }
        }
        // Experiment (`VICTORY_SKIP_WC_COMPARE=1`): copies of write-combining
        // title memory are left out of the hashing and the compare; nothing
        // of them is fetched back. Pending ranges are still closed by the
        // flights' own declared writes (`declared_writes`).
        if crate::census::skip_wc_compare() {
            let before = declared.len();
            declared.retain(|(shadow, _)| !crate::census::is_wc(shadow.title_start()));
            crate::census::UNCOMPARED.fetch_add((before - declared.len()) as u64, Ordering::Relaxed);
        }
        // With the relay: the order of the queue is held from here until
        // what is taken is on the queue, and the taking is numbered.
        let mut order = relay().then(|| QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()));
        let sent_as = match order.as_mut() {
            Some(order) => {
                **order += 1;
                **order
            }
            None => 0,
        };
        let mut dirty = crate::shadow::card_dirty(sent_as);
        // With fetches on a copy queue (`crate::queue2`, bit 1 of
        // `VICTORY_QUEUE2`): the send's number is signalled on the direct
        // queue behind the send, so that a fetch knows which sends the card
        // has finished (`Second::order_done`).
        let numbered = || {
            if sent_as != 0
                && let Some(second) = crate::queue2::second().filter(|s| s.fetches())
            {
                second.sent_up(&self.queue, sent_as);
            }
        };
        if declared.is_empty() && dirty.is_empty() {
            numbered();
            return None;
        }
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let work = work.as_mut()?;
        // A copy whose shader writes are declared for the first time gets
        // its bytes from before, whole.
        let mut whole: Vec<Arc<crate::shadow::Shadow>> = Vec::new();
        for (shadow, _) in &declared {
            if shadow.card_make_before(|size| {
                let before = self.uav_buffer(crate::shadow::card_hash_bytes(size))?;
                name(&before, || "title buffer on the card, hashes of its pieces".into());
                Some(before)
            }) {
                whole.push(shadow.clone());
                if !dirty.iter().any(|(s, _)| Arc::ptr_eq(s, shadow)) {
                    dirty.push((shadow.clone(), Vec::new()));
                }
            }
        }
        let mut used: Vec<(bool, Prelude)> = Vec::new();
        // The changed bytes through the copy queue (`VICTORY_QUEUE2` bit
        // 2): the landing buffer they are in, in the order of `dirty`'s
        // ranges, once the direct queue has waited. `None`: straight from
        // the PC's memory on the direct queue, as before.
        let mut landed: Option<ID3D12Resource> = None;
        if let Some(second) = crate::queue2::second().filter(|s| s.uploads()) {
            let bytes: u64 = dirty
                .iter()
                .filter(|(shadow, _)| shadow.carded())
                .flat_map(|(_, ranges)| ranges.iter().map(|&(a, b)| (b - a) as u64))
                .sum();
            // SAFETY: a plain query.
            let completed = unsafe { self.fence.GetCompletedValue() };
            // Held until the card has passed this submission's signal: the
            // direct queue's copy out of it runs ahead of that.
            if bytes > 0
                && let Some(landing) = second.landing(bytes, completed, sent + 1)
            {
                // SAFETY: an open copy list of the lane's; live buffers,
                // and the ranges lie in them; the host's queue.
                let sent_up = unsafe {
                    (|| -> windows::core::Result<()> {
                        let copy = second.up.list()?;
                        let mut at = 0u64;
                        for (shadow, ranges) in &dirty {
                            let Some((staging, _, _)) = shadow.card_sides() else {
                                continue;
                            };
                            for &(a, b) in ranges {
                                copy.1.CopyBufferRegion(&landing, at, staging, a as u64, (b - a) as u64);
                                at += (b - a) as u64;
                            }
                        }
                        let job = second.up.send(copy, &[], bytes)?;
                        // Nothing sent to the direct queue from here on
                        // runs before the bytes have landed.
                        second.hold(&self.queue, job)
                    })()
                };
                if let Err(e) = sent_up {
                    fail(format_args!(
                        "host copies in the card's memory could not be brought up to date through the copy queue ({e}, VICTORY_QUEUE2)"
                    ));
                }
                landed = Some(landing);
            }
        }
        if !dirty.is_empty() {
            let Some(sync) = work.sync.pop().or_else(|| self.make_prelude()) else {
                fail(format_args!("no host list to bring host copies in the card's memory up to date"));
            };
            let (allocator, list) = &sync;
            // SAFETY: the list's last run finished (it ran ahead of a
            // submission that was waited for, and came back after that);
            // live buffers, and the ranges lie in them.
            let recorded = unsafe {
                (|| -> windows::core::Result<()> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    let timed = crate::cardmeter::begin(list, crate::cardmeter::Kind::Sync);
                    // Where the next range lies in the landing buffer.
                    let mut at = 0u64;
                    // In three steps over all the copies ([`sync_batched`]):
                    // every changed range copied up, then every state
                    // change in one call, then every hash pass.
                    struct Synced<'a> {
                        shadow: &'a Arc<crate::shadow::Shadow>,
                        ranges: &'a [(usize, usize)],
                        before: Option<ID3D12Resource>,
                        size: u64,
                    }
                    let batched = sync_batched();
                    let mut synced: Vec<Synced> = Vec::with_capacity(dirty.len());
                    // No twin (it cost the card the copy's size again, 399 MB
                    // in run 75): `before` holds a 128-bit hash of each 64 KiB
                    // piece. The pieces just sent up are hashed again, all of
                    // them the first time; the compare after the lists hashes
                    // the declared pieces and flags those whose hash is another.
                    let hashes = |list: &ID3D12GraphicsCommandList, group: &[Synced]| {
                        // (a copy promoted the buffer to a copy destination)
                        let moves: Vec<_> = group
                            .iter()
                            .filter(|w| w.before.is_some() && !w.ranges.is_empty())
                            .map(|w| transition(&w.shadow.buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE))
                            .collect();
                        if !moves.is_empty() {
                            list.ResourceBarrier(&moves);
                        }
                        let mut bound = false;
                        for w in group {
                            let Some(before) = &w.before else {
                                continue;
                            };
                            if !bound {
                                bound = true;
                                list.SetComputeRootSignature(&work.root);
                                list.SetPipelineState(&work.pso);
                                list.SetComputeRootUnorderedAccessView(3, work.differs.0.GetGPUVirtualAddress());
                            }
                            list.SetComputeRootShaderResourceView(1, w.shadow.buffer.GetGPUVirtualAddress());
                            list.SetComputeRootUnorderedAccessView(2, before.GetGPUVirtualAddress());
                            let runs: Vec<crate::shadow::CardSpan> = if whole.iter().any(|s| Arc::ptr_eq(s, w.shadow)) {
                                vec![w.shadow.card_run(0, usize::MAX)]
                            } else {
                                w.ranges.iter().map(|&(a, b)| w.shadow.card_run(a, b)).collect()
                            };
                            // The same pieces in fewer passes (`rehash_runs`).
                            let runs = if rehash_runs() { crate::shadow::joined_piece_runs(runs) } else { runs };
                            CARD_PASSES[0].fetch_add(runs.len() as u64, Ordering::Relaxed);
                            CARD_PASSES[1].fetch_add(runs.iter().map(|r| u64::from(r.pieces)).sum::<u64>(), Ordering::Relaxed);
                            for run in runs {
                                dispatch_card_compare(list, [run.first, run.skew, w.size as u32, run.pieces, 0, 0]);
                            }
                        }
                    };
                    for (shadow, ranges) in &dirty {
                        let Some((staging, before, size)) = shadow.card_sides() else {
                            continue;
                        };
                        for &(a, b) in ranges {
                            match &landed {
                                Some(landing) => list.CopyBufferRegion(&shadow.buffer, a as u64, landing, at, (b - a) as u64),
                                None => list.CopyBufferRegion(&shadow.buffer, a as u64, staging, a as u64, (b - a) as u64),
                            }
                            at += (b - a) as u64;
                            crate::shadow::CARD_STATS[0].fetch_add((b - a) as u64, Ordering::Relaxed);
                        }
                        crate::shadow::CARD_STATS[1].fetch_add(ranges.len() as u64, Ordering::Relaxed);
                        crate::census::sent(
                            shadow.title_start(),
                            ranges.iter().map(|&(a, b)| (b - a) as u64).sum(),
                            ranges.len() as u64,
                        );
                        synced.push(Synced { shadow, ranges, before, size });
                        if !batched {
                            // As before: this copy's hash passes behind its copies.
                            hashes(list, &synced[synced.len() - 1..]);
                        }
                    }
                    if batched {
                        hashes(list, &synced);
                    }
                    crate::cardmeter::end(list, timed);
                    list.Close()?;
                    self.queue.ExecuteCommandLists(&[Some(list.cast()?)]);
                    Ok(())
                })()
            };
            if let Err(e) = recorded {
                fail(format_args!("host copies in the card's memory could not be brought up to date ({e})"));
            }
            used.push((false, sync));
        }
        numbered();
        drop(order);
        let mut plan = CardPlan {
            ranges: Vec::new(),
            check: None,
            _synced: dirty.into_iter().map(|(shadow, _)| shadow).collect(),
            lists: used,
            sent_as,
            inflight: false,
            inflight_landed: 0,
        };
        // Each byte once, as the copy-back joins the declared ranges. The
        // results go behind those of plans still in the air.
        let mut at = work.taken;
        for (shadow, ranges) in &declared {
            let mut sorted = ranges.clone();
            sorted.sort_unstable();
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for (from, to) in sorted {
                match spans.last_mut() {
                    Some(last) if from <= last.1 => last.1 = last.1.max(to),
                    _ => spans.push((from, to)),
                }
            }
            let twin = shadow.card_sides().is_some_and(|(_, before, _)| before.is_some());
            for (from, to) in spans {
                let Some(span) = shadow.card_span(from, to) else {
                    continue;
                };
                let compared = twin && u64::from(at + span.pieces) * 4 <= work.differs.2;
                plan.ranges.push(CardRange {
                    shadow: shadow.clone(),
                    declared: (from, to),
                    span,
                    at,
                    compared,
                });
                if compared {
                    at += span.pieces;
                }
            }
        }
        work.taken = at;
        if plan.ranges.iter().any(|r| r.compared) {
            let Some(check) = work.check.pop().or_else(|| self.make_prelude()) else {
                fail(format_args!("no host list for the compare of host copies in the card's memory"));
            };
            let (allocator, list) = &check;
            // SAFETY: the list's last run finished (its submission was
            // waited for, and it came back after that); live buffers; the
            // compare's own root signature.
            let recorded = unsafe {
                (|| -> windows::core::Result<ID3D12CommandList> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    let timed = crate::cardmeter::begin(list, crate::cardmeter::Kind::Check);
                    // What the submission's shaders wrote, first.
                    list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                        Anonymous: D3D12_RESOURCE_BARRIER_0 {
                            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                                pResource: std::mem::ManuallyDrop::new(None),
                            }),
                        },
                    }]);
                    list.SetComputeRootSignature(&work.root);
                    list.SetPipelineState(&work.pso);
                    list.SetComputeRootUnorderedAccessView(3, work.differs.0.GetGPUVirtualAddress());
                    for r in plan.ranges.iter().filter(|r| r.compared) {
                        let (Some((_, Some(before), _)), size) = (r.shadow.card_sides(), r.shadow.card_hash_size()) else {
                            continue;
                        };
                        let constants = [r.span.first, r.span.skew, size as u32, r.span.pieces, r.at * 4, if flown { 2 } else { 1 }];
                        list.SetComputeRootShaderResourceView(1, r.shadow.buffer.GetGPUVirtualAddress());
                        list.SetComputeRootUnorderedAccessView(2, before.GetGPUVirtualAddress());
                        dispatch_card_compare(&list, constants);
                        CARD_PASSES[2].fetch_add(1, Ordering::Relaxed);
                    }
                    // The fetch inside the flight (`inflight_fetch`): behind
                    // every compare of the list, the pieces they flagged go
                    // to slots in the PC's memory.
                    if flown && let Some(inflight) = &work.inflight {
                        list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                                    pResource: std::mem::ManuallyDrop::new(None),
                                }),
                            },
                        }]);
                        list.SetComputeRootSignature(&inflight.root);
                        list.SetPipelineState(&inflight.pso);
                        list.SetComputeRootUnorderedAccessView(2, work.differs.0.GetGPUVirtualAddress());
                        list.SetComputeRootUnorderedAccessView(3, inflight.table.0.GetGPUVirtualAddress());
                        list.SetComputeRootUnorderedAccessView(4, inflight.fetched.0.GetGPUVirtualAddress());
                        let mut every = true;
                        for r in plan.ranges.iter().filter(|r| r.compared) {
                            let (Some((_, Some(_), _)), size) = (r.shadow.card_sides(), r.shadow.card_hash_size()) else {
                                every = false;
                                continue;
                            };
                            let groups = r.span.pieces.clamp(1, 65535);
                            let constants =
                                [r.span.first, r.span.skew, size as u32, r.span.pieces, r.at * 4, inflight.landed, inflight.fetched.2, groups];
                            list.SetComputeRoot32BitConstants(0, 8, constants.as_ptr().cast(), 0);
                            list.SetComputeRootShaderResourceView(1, r.shadow.buffer.GetGPUVirtualAddress());
                            list.Dispatch(groups, 1, 1);
                        }
                        plan.inflight = every;
                        plan.inflight_landed = inflight.landed;
                    }
                    crate::cardmeter::end(list, timed);
                    list.Close()?;
                    list.cast()
                })()
            };
            match recorded {
                Ok(list) => plan.check = Some(list),
                Err(e) => fail(format_args!("the compare of host copies in the card's memory could not be recorded ({e})")),
            }
            plan.lists.push((true, check));
        }
        work.out += 1;
        Some(plan)
    }

    /// With [`relay`]: whether the compare's result area is half taken by
    /// plans in the air. It starts from the beginning only when no plan is
    /// out, so the caller lets the flights land first.
    fn card_results_low(&self) -> bool {
        CARD_WORK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|work| u64::from(work.taken) * 8 > work.differs.2)
    }

    /// The plan's submission has run and its results were read
    /// ([`Host::card_back`]): its lists are free for the next plan. Called
    /// when nothing of ours is on the card, so the compare's results start
    /// from the beginning again.
    fn card_done(&self, plan: CardPlan) {
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(work) = work.as_mut() else {
            return;
        };
        for (check, list) in plan.lists {
            if check { work.check.push(list) } else { work.sync.push(list) }
        }
        work.out = work.out.saturating_sub(1);
        // With the relay other plans may be on the card, their results
        // not read yet: the area starts again only when none is out.
        if !relay() || work.out == 0 {
            work.taken = 0;
        }
    }

    /// The relay's own list, fence, event for [`Host::relay_card_back`].
    fn make_relay_fetch(&self) -> Option<RelayFetch> {
        // SAFETY: plain objects of our own.
        let (list, queue) = if copy_fetch() {
            unsafe {
                let kind = D3D12_COMMAND_LIST_TYPE_COPY;
                let desc = D3D12_COMMAND_QUEUE_DESC { Type: kind, ..Default::default() };
                let queue: ID3D12CommandQueue = self.device.CreateCommandQueue(&desc).ok()?;
                let allocator: ID3D12CommandAllocator = self.device.CreateCommandAllocator(kind).ok()?;
                let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(0, kind, &allocator, None).ok()?;
                list.Close().ok()?;
                ((allocator, list), Some(queue))
            }
        } else {
            (self.make_prelude()?, None)
        };
        // SAFETY: plain objects of our own.
        unsafe {
            let fence: ID3D12Fence = self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            let event = CreateEventW(None, false, false, None).ok()?;
            Some(RelayFetch {
                list,
                fence,
                event,
                sent: 0,
                buffer: None,
                queue,
            })
        }
    }

    /// [`relay`], on the thread that lands flights: for the plans of the
    /// flights being landed (the card has run them), the pieces their
    /// compares flagged are fetched, once each, with a list and a wait of
    /// this thread's own, and what shaders wrote in them goes into title
    /// memory (`Shadow::card_merge_relay`). As [`Host::card_back`], but:
    /// no submission lock; a piece flagged under one range of a plan is
    /// fetched under all ([`flagged_pieces`]); the fetch runs behind
    /// whatever was sent meanwhile, so it may bring a later flight's
    /// bytes, which that flight's landing then finds merged already; and
    /// nothing is sent back up.
    ///
    /// With [`inflight_fetch`] the pieces are first looked for where the
    /// flights' own compare lists put them ([`Host::relay_card_inflight`]);
    /// only a landing that cannot take them from there fetches, as here.
    /// `behind`: the value signalled on the host's fence behind the last of
    /// the flights. With `VICTORY_QUEUE2` bit 1 (`crate::queue2`) the fetch
    /// goes to a copy queue, behind that value and nothing else: it does
    /// not stand behind what the submitting thread sent meanwhile (run
    /// 118: 114-157 ms a frame of this thread's time). It then reads the
    /// card's side while later flights may be writing it: a byte is the
    /// old one or the new one, and the flight that writes it has a compare
    /// and a landing of its own behind it.
    ///
    /// `plans` come with their flights' numbers and `landed` is the number
    /// of the last flight being landed: a piece an earlier fetch on the
    /// copy queue brought down beside one of these flights is fetched
    /// whatever the compare says ([`BESIDE`]).
    fn relay_card_back(&self, plans: &[(u64, &CardPlan)], fetch: &mut Option<RelayFetch>, behind: u64, landed: u64) {
        if inflight_fetch() {
            let only: Vec<&CardPlan> = plans.iter().map(|p| p.1).collect();
            let taken = self.relay_card_inflight(&only);
            inflight_tickets_landed(&only);
            if taken {
                return;
            }
        }
        let lane = crate::queue2::second().filter(|s| s.fetches());
        // The pieces fetched beside flights that had started, as noted by
        // earlier landings: looked at for these plans, renewed below.
        let beside = lane.is_some() && refetch();
        let noted: std::collections::HashMap<(usize, u32), u64> = if beside {
            BESIDE.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default()
        } else {
            Default::default()
        };
        let mut again = 0u64;
        // Every piece fetched now: (copy, piece number).
        let mut numbers: Vec<(usize, u32)> = Vec::new();
        let differs = {
            let work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
            work.as_ref().map(|work| (work.differs.1, (work.differs.2 / 4) as usize))
        };
        let mut pieces: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize, u64)> = Vec::new();
        let mut total = 0u64;
        if let Some((differs, dwords)) = differs {
            // SAFETY: the mapping of the results. The compares of these
            // plans have run, and their dwords are given to no other plan
            // before these are done with (`card_done`).
            let differs = unsafe { std::slice::from_raw_parts(differs as *const u32, dwords) };
            let mut seen = std::collections::HashSet::new();
            for &(number, plan) in plans {
                let mut flags: Vec<(usize, u32, bool)> = plan
                    .ranges
                    .iter()
                    .flat_map(|r| {
                        (0..r.span.pieces).map(move |p| {
                            let flagged = !r.compared || differs.get((r.at + p) as usize).is_none_or(|&d| d != 0);
                            (std::sync::Arc::as_ptr(&r.shadow) as usize, r.span.first + p, flagged)
                        })
                    })
                    .collect();
                // A piece an earlier fetch brought down while this flight
                // may have been writing it: fetched, flagged or not.
                for flag in flags.iter_mut().filter(|f| !f.2) {
                    if fetched_beside(noted.get(&(flag.0, flag.1)).copied(), number) {
                        flag.2 = true;
                        again += 1;
                    }
                }
                let fetched = flagged_pieces(&flags);
                numbers.extend(fetched.iter().copied());
                for r in &plan.ranges {
                    let copy = std::sync::Arc::as_ptr(&r.shadow) as usize;
                    for p in 0..r.span.pieces {
                        if !fetched.contains(&(copy, r.span.first + p)) {
                            continue;
                        }
                        let (lo, hi) = r.shadow.card_piece(&r.span, p);
                        if lo < hi
                            && !crate::census::skip_back(r.shadow.title_start(), (hi - lo) as u64)
                            && seen.insert((copy, lo, hi))
                        {
                            pieces.push((r.shadow.clone(), lo, hi, total));
                            total += (hi - lo) as u64;
                        }
                    }
                    crate::shadow::CARD_STATS[2].fetch_add(u64::from(r.span.pieces), Ordering::Relaxed);
                    crate::census::compared(r.shadow.title_start(), u64::from(r.span.pieces));
                }
            }
        }
        BESIDE_STATS[0].fetch_add(again, Ordering::Relaxed);
        if pieces.is_empty() {
            if inflight_fetch() || copy_fetch() {
                // The next merge may be of a flight in the air now, with
                // bytes from when it ran: only sends up to these plans' are
                // older than it.
                if let Some(last) = plans.iter().map(|p| p.1.sent_as).max() {
                    crate::shadow::relay_fetched(last);
                }
                return;
            }
            if beside {
                // Notes whose flights have all landed are done with.
                let mut all = BESIDE.lock().unwrap_or_else(|e| e.into_inner());
                note_beside(all.get_or_insert_with(Default::default), &[], landed, landed);
            }
            // Nothing to fetch: every send so far is older than the next
            // fetch (on a copy queue: every send the card has finished).
            crate::shadow::relay_fetched(match lane {
                Some(second) => second.order_done(),
                None => *QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()),
            });
            return;
        }
        if inflight_fetch() {
            INFLIGHT_STATS[3].fetch_add(1, Ordering::Relaxed);
        }
        if fetch.is_none() {
            *fetch = self.make_relay_fetch();
        }
        let Some(f) = fetch.as_mut() else {
            fail(format_args!("no list, fence or event to fetch what shaders wrote to the card's memory (VICTORY_RELAY=1)"));
        };
        if f.buffer.as_ref().is_none_or(|b| b.2 < total) {
            let size = total.next_multiple_of(16 << 20);
            f.buffer = self.shared_buffer(size).map(|(buffer, data)| (buffer, data as usize, size));
        }
        let Some((buffer, data, _)) = f.buffer.clone() else {
            fail(format_args!("no memory to fetch what shaders wrote to the card's memory ({total} bytes, VICTORY_RELAY=1)"));
        };
        f.sent += 1;
        let ((allocator, list), fence, event, sent) = (f.list.clone(), f.fence.clone(), f.event, f.sent);
        let own_queue = f.queue.clone();
        // What the landed flights were sent behind has run.
        let arrived = plans.iter().map(|p| p.1.sent_as).max().unwrap_or(0);
        let waiting = std::time::Instant::now();
        // SAFETY: the list's last run was waited for by this thread; live
        // buffers (the copies are held by the plans); the fence and the
        // event are this thread's.
        let ran = unsafe {
            (|| -> windows::core::Result<u64> {
                if let Some(second) = lane {
                    // The sends of title bytes the card has finished are in
                    // what comes down. A later one may be too, if the card
                    // runs it before this fetch: the merge leaves its bytes
                    // out as a send that came after, and they are the
                    // title's either way.
                    let after = second.order_done();
                    let copy = second.down.list()?;
                    for (shadow, lo, hi, at) in &pieces {
                        copy.1.CopyBufferRegion(&buffer, *at, &shadow.buffer, *lo as u64, (*hi - *lo) as u64);
                    }
                    let job = second.down.send(copy, &[(&self.fence, behind)], total)?;
                    second.down.wait(job, event)?;
                    if beside {
                        // Whatever had started by now may have been
                        // writing these pieces while they came down
                        // (counted before a submission's lists go to the
                        // queue, `Host::submit`: nothing later ran yet).
                        let started = STARTED.load(Ordering::Acquire);
                        let mut all = BESIDE.lock().unwrap_or_else(|e| e.into_inner());
                        let noted = note_beside(all.get_or_insert_with(Default::default), &numbers, landed, started);
                        BESIDE_STATS[1].fetch_add(noted, Ordering::Relaxed);
                    }
                    second.down.stats.waited_us.fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
                    return Ok(after);
                }
                allocator.Reset()?;
                list.Reset(&allocator, None)?;
                for (shadow, lo, hi, at) in &pieces {
                    list.CopyBufferRegion(&buffer, *at, &shadow.buffer, *lo as u64, (*hi - *lo) as u64);
                }
                list.Close()?;
                let lists = [Some(list.cast::<ID3D12CommandList>()?)];
                if let Some(queue) = &own_queue {
                    queue.ExecuteCommandLists(&lists);
                    queue.Signal(&fence, sent)?;
                    fence.SetEventOnCompletion(sent, event)?;
                    WaitForSingleObject(event, INFINITE);
                    return Ok(arrived);
                }
                // In the order of the queue: the sends of title bytes so
                // far are ahead of this fetch, every later one behind it.
                let order = QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner());
                self.queue.ExecuteCommandLists(&lists);
                let after = *order;
                drop(order);
                self.card_sent();
                self.queue.Signal(&fence, sent)?;
                fence.SetEventOnCompletion(sent, event)?;
                WaitForSingleObject(event, INFINITE);
                Ok(after)
            })()
        };
        let after = match ran {
            Ok(after) => after,
            Err(e) => fail(format_args!("what shaders wrote to the card's memory could not be fetched ({e}, VICTORY_RELAY=1)")),
        };
        // This fetch may bring bytes of flights in the air now: their own
        // slots are older than what it puts in the CPU's side there
        // (`relay_card_inflight`). Sends up to `after` are numbered, and the
        // submission numbered `after` may not have its lists on the queue
        // yet; every one before it has (the submission lock).
        if inflight_fetch()
            && after > 1
            && let Some(inflight) = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|w| w.inflight.as_mut())
        {
            inflight.brought.extend(pieces.iter().map(|(shadow, lo, hi, _)| (shadow.clone(), *lo, *hi, after - 1)));
        }
        let us = waiting.elapsed().as_micros() as u64;
        RELAY_STATS[3].fetch_add(us, Ordering::Relaxed);
        {
            use crate::shadow::CARD_STATS;
            CARD_STATS[3].fetch_add(pieces.len() as u64, Ordering::Relaxed);
            CARD_STATS[4].fetch_add(total, Ordering::Relaxed);
            CARD_STATS[5].fetch_add(1, Ordering::Relaxed);
            CARD_STATS[6].fetch_add(us, Ordering::Relaxed);
            if crate::census::on() {
                for (shadow, lo, hi, _) in &pieces {
                    crate::census::fetched(shadow.title_start(), (hi - lo) as u64);
                }
            }
        }
        let mut title = Vec::new();
        let mut written = 0;
        for (shadow, lo, hi, at) in &pieces {
            // SAFETY: the mapping of the fetched bytes, `total` of them;
            // the copy into it has run.
            let gpu = unsafe { std::slice::from_raw_parts((data + *at as usize) as *const u8, hi - lo) };
            let merged = shadow.card_merge_relay(*lo, gpu, after, &mut title);
            shadow.walls_merged(gpu.len(), merged);
            written += merged;
        }
        copy_back(1, written as u64);
        // Sends up to `after` are of no use to any later merge; with the
        // fetch inside flights only those up to these plans' are
        // (`fallback_fetched_mark`).
        let landed = plans.iter().map(|p| p.1.sent_as).max();
        crate::shadow::relay_fetched(fallback_fetched_mark(inflight_fetch(), after, landed));
    }

    /// [`Host::relay_card_back`] with [`inflight_fetch`]: the pieces these
    /// plans' compares flagged are taken from the slots their own compare
    /// lists copied them to ([`CARD_FETCH`]) and merged, plan by plan in the
    /// line's order, with no list or wait of ours. `false` (nothing merged)
    /// if any plan cannot land so; the caller then lands them all the old
    /// way, as without the switch.
    ///
    /// The merge's "fetched after" (`Shadow::card_merge_relay`) is the
    /// plan's own `sent_as`. `QUEUE_ORDER` is raised only in
    /// `Host::card_before`, under the submission lock, and that lock is held
    /// until the submission's lists and its compare's list are on the queue
    /// behind its sync list: so when the compare's list ran, the card had
    /// received exactly the sends 1..=`sent_as` of title bytes and no later
    /// one, which is what the merge's skip of later sends asks. (The old
    /// fetch, by contrast, ran whenever it reached the queue, and took the
    /// count at its own ExecuteCommandLists.)
    ///
    /// When a landing goes the old way instead, all of it does: a fetch the
    /// old way brings the card's bytes as they are now, newer than the slots
    /// of plans in the air then, and merging an older slot after it would
    /// put a flight's bytes back over a later flight's. For the same reason
    /// the bytes such a fetch brought (`CardInflight::brought`) are left
    /// out of the slots of flights whose lists were on the queue ahead of
    /// it: the CPU's side holds what the card held after them there. The
    /// rest of their slots is merged, newer than anything merged there
    /// before. (Left out whole, a flight sent before such a fetch would have
    /// had to land the old way, its fetch taking a later count still: one
    /// overflow kept every landing on the old way while flights were in
    /// the air.) Plans land in the line's order, each plan's slot as it was
    /// right behind its own lists, so every merge here is of bytes at least
    /// as new as the CPU's side holds.
    ///
    /// shortcut: all or nothing per landing, and a plan whose pieces overflow
    /// the slots lands the old way for all its pieces, not only the ones
    /// left out.
    fn relay_card_inflight(&self, plans: &[&CardPlan]) -> bool {
        use std::sync::Arc;
        type Piece = (Arc<crate::shadow::Shadow>, usize, usize, usize, u64);
        let mut pieces: Vec<Piece> = Vec::new();
        {
            let work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
            let Some(work) = work.as_ref() else {
                return false;
            };
            let Some(inflight) = &work.inflight else {
                return false;
            };
            let dwords = (work.differs.2 / 4) as usize;
            // SAFETY: the mappings of the results and the tickets. The
            // compares and fetches of these plans have run, and their dwords
            // are given to no other plan before these are done with
            // (`card_done`).
            let (differs, table) = unsafe {
                (
                    std::slice::from_raw_parts(work.differs.1 as *const u32, dwords),
                    std::slice::from_raw_parts((inflight.table.1 as *const u32).add(1), dwords),
                )
            };
            let slots = inflight.fetched.2;
            for plan in plans {
                if plan.ranges.is_empty() {
                    continue;
                }
                if !plan.inflight || plan.ranges.iter().any(|r| !r.compared) {
                    return false;
                }
                // Pieces (copy, number) the compare flagged, with their ticket.
                let mut tickets: std::collections::HashMap<(usize, u32), u32> = std::collections::HashMap::new();
                let mut flags: Vec<(usize, u32, bool)> = Vec::new();
                for r in &plan.ranges {
                    let copy = Arc::as_ptr(&r.shadow) as usize;
                    for p in 0..r.span.pieces {
                        let at = (r.at + p) as usize;
                        let (Some(&flag), Some(&entry)) = (differs.get(at), table.get(at)) else {
                            return false;
                        };
                        if flag != 0 {
                            if entry == INFLIGHT_NONE {
                                return false;
                            }
                            if !inflight_gets_slot(entry, plan.inflight_landed, slots) {
                                INFLIGHT_STATS[2].fetch_add(1, Ordering::Relaxed);
                                return false;
                            }
                            tickets.insert((copy, r.span.first + p), entry);
                        }
                        flags.push((copy, r.span.first + p, flag != 0));
                    }
                }
                let fetched = flagged_pieces(&flags);
                let mut seen = std::collections::HashSet::new();
                for r in &plan.ranges {
                    let copy = Arc::as_ptr(&r.shadow) as usize;
                    for p in 0..r.span.pieces {
                        let piece = r.span.first + p;
                        if !fetched.contains(&(copy, piece)) {
                            continue;
                        }
                        let (lo, hi) = r.shadow.card_piece(&r.span, p);
                        if lo >= hi || !seen.insert((copy, lo, hi)) {
                            continue;
                        }
                        let from = inflight_piece_from(&r.span, p);
                        let Some(&k) = tickets.get(&(copy, piece)) else {
                            return false;
                        };
                        if lo < from || (hi - from) as u64 > INFLIGHT_SLOT {
                            return false;
                        }
                        let slot = inflight.fetched.1 + (k % slots) as usize * INFLIGHT_SLOT as usize;
                        // Bytes a fetch the old way brought after this flight ran.
                        let newer: Vec<(usize, usize)> = inflight
                            .brought
                            .iter()
                            .filter(|b| b.3 >= plan.sent_as && Arc::ptr_eq(&b.0, &r.shadow) && b.1 < hi && b.2 > lo)
                            .map(|b| (b.1, b.2))
                            .collect();
                        for (a, b) in crate::shadow::outside(lo, hi, newer) {
                            pieces.push((r.shadow.clone(), a, b, slot + (a - from), plan.sent_as));
                        }
                    }
                }
            }
        }
        // Outside the lock: no slot of these plans is written over before
        // their tickets are landed (`inflight_tickets_landed`, after this).
        let mut title = Vec::new();
        let (mut written, mut bytes) = (0, 0u64);
        for (shadow, lo, hi, at, after) in &pieces {
            // SAFETY: inside the mapping of the slots (checked above); the
            // copy into it ran in the plan's flight, which the card has run.
            let gpu = unsafe { std::slice::from_raw_parts(*at as *const u8, hi - lo) };
            written += shadow.card_merge_relay(*lo, gpu, *after, &mut title);
            bytes += (hi - lo) as u64;
        }
        for plan in plans {
            for r in &plan.ranges {
                crate::shadow::CARD_STATS[2].fetch_add(u64::from(r.span.pieces), Ordering::Relaxed);
                crate::census::compared(r.shadow.title_start(), u64::from(r.span.pieces));
            }
        }
        copy_back(1, written as u64);
        INFLIGHT_STATS[0].fetch_add(pieces.len() as u64, Ordering::Relaxed);
        INFLIGHT_STATS[1].fetch_add(bytes, Ordering::Relaxed);
        // The next merge is of a later plan, or a fetch the old way sent now.
        if let Some(last) = plans.iter().map(|p| p.sent_as).max() {
            crate::shadow::relay_fetched(last);
        }
        true
    }

    /// [`relay`], on the thread that lands flights: `items` are the first
    /// things of the line, flights the card has run and what follows them.
    /// What their shaders wrote goes into title memory (copies in the
    /// card's memory first, then the others, each byte once whichever
    /// flights declared it); then every flight's ranges are done with;
    /// then, in the line's order, each flight's read-backs, its
    /// bottom-of-pipe writes and the fence signals behind it. Title memory
    /// is complete before the title is told anything.
    fn relay_land(&self, items: Vec<Late>, fetch: &mut Option<RelayFetch>) {
        let landing = std::time::Instant::now();
        {
            let flights: Vec<&Flight> = items
                .iter()
                .filter_map(|l| match l {
                    Late::Run(flight) => Some(flight),
                    _ => None,
                })
                .collect();
            if !flights.is_empty() {
                // SAFETY: a plain query.
                if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                    let runs: Vec<&HostList> = flights.iter().flat_map(|f| f.order.iter().map(|&i| f.lists[i].host())).collect();
                    fail(format_args!(
                        "host GPU device removed after {} submissions sent ahead (VICTORY_RELAY=1) of {} lists{}{}",
                        flights.len(),
                        runs.len(),
                        self.removal_data(&runs),
                        last_notes(&runs)
                    ));
                }
                fill_ahead::landing(flights.iter().map(|f| f.number).min().unwrap_or(0));
                let plans: Vec<(u64, &CardPlan)> = flights.iter().filter_map(|f| f.card.as_ref().map(|card| (f.number, card))).collect();
                let behind = flights.iter().map(|f| f.fence).max().unwrap_or(0);
                let landed = flights.iter().map(|f| f.number).max().unwrap_or(0);
                self.relay_card_back(&plans, fetch, behind, landed);
                // A copy with a copy-back not done yet beside the landing:
                // that one first, so they reach title memory in order.
                if copy_back_async() {
                    let due = {
                        let backs = async_backs();
                        flights
                            .iter()
                            .flat_map(|f| f.back.iter())
                            .filter_map(|(shadow, _)| backs.due_for_owner(std::sync::Arc::as_ptr(shadow) as usize))
                            .max()
                    };
                    async_wait(due);
                }
                let mut back: Vec<Declared> = Vec::new();
                for flight in &flights {
                    for (shadow, ranges) in &flight.back {
                        match back.iter_mut().find(|(s, _)| std::sync::Arc::ptr_eq(s, shadow)) {
                            Some((_, all)) => {
                                for range in ranges {
                                    if !all.contains(range) {
                                        all.push(*range);
                                    }
                                }
                            }
                            None => back.push((shadow.clone(), ranges.clone())),
                        }
                    }
                }
                // Read-back rings of three: copied back beside the landing
                // ([`copy_back_async`]); the ranges stay written (closed to
                // refreshes, waited for by readers) until then.
                let lagged = |shadow: &crate::shadow::Shadow| {
                    copy_back_async() && !shadow.carded() && {
                        let (start, len) = shadow.span();
                        crate::readback::lagged(start, len)
                    }
                };
                let (ring, back): (Vec<Declared>, Vec<Declared>) = back.into_iter().partition(|(s, _)| lagged(s));
                relay_hand_back(&back);
                // Only now, and flight by flight: a range another flight
                // in the air declared too stays closed to refreshes.
                let mut later: Vec<AsyncWork> = ring
                    .into_iter()
                    .map(|(copy, declared)| AsyncWork { copy: Some(copy), declared, flights: Vec::new() })
                    .collect();
                for flight in &flights {
                    for (shadow, ranges) in &flight.back {
                        match later.iter_mut().find(|w| w.copy.as_ref().is_some_and(|c| std::sync::Arc::ptr_eq(c, shadow))) {
                            Some(w) => w.flights.push(ranges.clone()),
                            None => shadow.copy_back_done(ranges),
                        }
                    }
                }
                if !later.is_empty() {
                    async_queue(later);
                }
                copy_back(0, landing.elapsed().as_micros() as u64);
                RELAY_STATS[0].fetch_add(1, Ordering::Relaxed);
                RELAY_STATS[1].fetch_add(flights.len() as u64, Ordering::Relaxed);
            }
        }
        // In the order given: what tells the title the card is done.
        let mut last = 0;
        let mut landed = Vec::new();
        for item in items {
            match item {
                Late::Run(flight) => {
                    last = flight.number;
                    for &i in &flight.order {
                        read_backs(flight.lists[i].host());
                    }
                    if let Some(card) = flight.card {
                        self.card_done(card);
                    }
                    landed.push((flight.lists, flight.order));
                }
                Late::Write(at, what, bytes) => what.write(at as *mut u8, bytes),
                Late::Signal(fence, value) => crate::fence::set(fence as crate::object::This, value),
            }
        }
        if last > 0 {
            fills_landed(last);
            FINISHED.store(last, Ordering::Release);
            bury(last);
        }
        self.log_messages();
        fill_ahead::landing(0);
        RELAY_STATS[4].fetch_add(landing.elapsed().as_micros() as u64, Ordering::Relaxed);
        {
            let mut line = line();
            let Line {
                line: items,
                written,
                by_copy,
                lists,
                settling,
                poke,
                landed: kept,
                in_hand,
                ..
            } = &mut *line;
            kept.extend(landed);
            in_hand.clear();
            let _ = (items, written, by_copy, lists);
            *settling = false;
            // The landed lists are reset, and what queues hold looked at,
            // by the helper thread (they take locks a waiter may hold).
            *poke = true;
            // What is still in the air, from the flights that are (and the
            // copy-backs not done yet).
            rewrite_written(&mut line);
        }
        LANDINGS.fetch_add(1, Ordering::Release);
        LINE_MOVED.notify_all();
        LINE_LANDED.notify_all();
    }

    /// After a submission ran, for copies in the card's memory: the pieces
    /// of the declared ranges that the compare found differing (all of a
    /// range it did not compare) are fetched, with one more wait for the
    /// card, and what shaders wrote in them goes into title memory.
    /// `submitted` is the last value signalled on the fence. `seen` holds
    /// the pieces fetched already since the card was last waited for
    /// (several submissions settled together name the same ones).
    fn card_back(
        &self,
        plan: &CardPlan,
        submitted: &mut u64,
        seen: &mut std::collections::HashSet<(usize, usize, usize)>,
    ) {
        if plan.ranges.is_empty() {
            return;
        }
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(work) = work.as_mut() else {
            return;
        };
        // SAFETY: the mapping of the results, which the compare (it has
        // run) wrote.
        let differs = unsafe { std::slice::from_raw_parts(work.differs.1 as *const u32, (work.differs.2 / 4) as usize) };
        // The pieces to fetch: the range, the offsets in its copy, and
        // where the bytes come down.
        let mut pieces: Vec<(usize, usize, usize, u64)> = Vec::new();
        let mut total = 0u64;
        for (i, r) in plan.ranges.iter().enumerate() {
            for p in 0..r.span.pieces {
                if r.compared && differs[(r.at + p) as usize] == 0 {
                    continue;
                }
                let (lo, hi) = r.shadow.card_piece(&r.span, p);
                if lo < hi
                    && !crate::census::skip_back(r.shadow.title_start(), (hi - lo) as u64)
                    && seen.insert((std::sync::Arc::as_ptr(&r.shadow) as usize, lo, hi))
                {
                    pieces.push((i, lo, hi, total));
                    total += (hi - lo) as u64;
                }
            }
            crate::shadow::CARD_STATS[2].fetch_add(u64::from(r.span.pieces), Ordering::Relaxed);
            crate::census::compared(r.shadow.title_start(), u64::from(r.span.pieces));
        }
        let mut written = vec![0usize; plan.ranges.len()];
        if !pieces.is_empty() {
            if work.fetched.as_ref().is_none_or(|f| f.2 < total) {
                let size = total.next_multiple_of(16 << 20);
                work.fetched = self.shared_buffer(size).map(|(buffer, data)| (buffer, data as usize, size));
            }
            let Some((fetched, data, _)) = &work.fetched else {
                fail(format_args!("no memory to fetch what shaders wrote to the card's memory ({total} bytes)"));
            };
            let waiting = std::time::Instant::now();
            let (allocator, list) = &work.fetch;
            // SAFETY: the list's last run was waited for; live buffers; the
            // fence and event are ours, under the submission lock.
            let ran = unsafe {
                (|| -> windows::core::Result<()> {
                    // With `VICTORY_QUEUE2` bit 1 the fetch goes to the copy
                    // queue here too, behind everything sent to the direct
                    // queue. Nothing is gained (the card has run it all
                    // when this is reached); the path is the same one the
                    // relay uses, in the plainest run there is.
                    if let Some(second) = crate::queue2::second().filter(|s| s.fetches()) {
                        let copy = second.down.list()?;
                        for &(i, lo, hi, at) in &pieces {
                            copy.1.CopyBufferRegion(fetched, at, &plan.ranges[i].shadow.buffer, lo as u64, (hi - lo) as u64);
                        }
                        let job = second.down.send(copy, &[(&self.fence, *submitted)], total)?;
                        let was_working = working(false);
                        second.down.wait(job, self.event)?;
                        stood(waiting);
                        working(was_working);
                        second.down.stats.waited_us.fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
                        return Ok(());
                    }
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    for &(i, lo, hi, at) in &pieces {
                        list.CopyBufferRegion(fetched, at, &plan.ranges[i].shadow.buffer, lo as u64, (hi - lo) as u64);
                    }
                    list.Close()?;
                    self.queue.ExecuteCommandLists(&[Some(list.cast()?)]);
                    self.card_sent();
                    *submitted += 1;
                    self.queue.Signal(&self.fence, *submitted)?;
                    self.fence.SetEventOnCompletion(*submitted, self.event)?;
                    let was_working = working(false);
                    WaitForSingleObject(self.event, INFINITE);
                    stood(waiting);
                    working(was_working);
                    Ok(())
                })()
            };
            if let Err(e) = ran {
                fail(format_args!("what shaders wrote to the card's memory could not be fetched ({e})"));
            }
            {
                use crate::shadow::CARD_STATS;
                CARD_STATS[3].fetch_add(pieces.len() as u64, Ordering::Relaxed);
                CARD_STATS[4].fetch_add(total, Ordering::Relaxed);
                CARD_STATS[5].fetch_add(1, Ordering::Relaxed);
                CARD_STATS[6].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
                if crate::census::on() {
                    for &(i, lo, hi, _) in &pieces {
                        crate::census::fetched(plan.ranges[i].shadow.title_start(), (hi - lo) as u64);
                    }
                }
            }
            if wide_merge() && pieces.len() >= 8 {
                // The pieces are apart from one another (each its own bytes
                // of its copy and of title memory): several at a time.
                let fetched = *data;
                let counts: std::sync::Arc<Vec<std::sync::atomic::AtomicUsize>> =
                    std::sync::Arc::new((0..plan.ranges.len()).map(|_| std::sync::atomic::AtomicUsize::new(0)).collect());
                let each = pieces.len().div_ceil(8);
                let jobs: Vec<Box<dyn FnOnce() + Send>> = pieces
                    .chunks(each)
                    .map(|chunk| {
                        let chunk: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize, usize, u64)> =
                            chunk.iter().map(|&(i, lo, hi, at)| (plan.ranges[i].shadow.clone(), i, lo, hi, at)).collect();
                        let counts = counts.clone();
                        Box::new(move || {
                            let mut title = Vec::new();
                            for (shadow, i, lo, hi, at) in chunk {
                                // SAFETY: the mapping of the fetched bytes;
                                // the copy into it has run, and the caller
                                // waits for this job.
                                let gpu = unsafe { std::slice::from_raw_parts((fetched + at as usize) as *const u8, hi - lo) };
                                let merged = shadow.card_merge(lo, gpu, &mut title);
                                shadow.walls_merged(gpu.len(), merged);
                                counts[i].fetch_add(merged, Ordering::Relaxed);
                            }
                        }) as Box<dyn FnOnce() + Send>
                    })
                    .collect();
                crate::shadow::wide_run(jobs);
                for (written, count) in written.iter_mut().zip(counts.iter()) {
                    *written += count.load(Ordering::Relaxed);
                }
            } else {
                let mut title = Vec::new();
                for &(i, lo, hi, at) in &pieces {
                    // SAFETY: the mapping of the fetched bytes, `total` of
                    // them; the copy into it has run.
                    let gpu = unsafe { std::slice::from_raw_parts((*data + at as usize) as *const u8, hi - lo) };
                    let merged = plan.ranges[i].shadow.card_merge(lo, gpu, &mut title);
                    plan.ranges[i].shadow.walls_merged(gpu.len(), merged);
                    written[i] += merged;
                }
            }
        }
        for (r, written) in plan.ranges.iter().zip(written) {
            copy_back(1, written as u64);
            let (from, to) = r.declared;
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(from, to - from, written);
            let _ = (from, to);
        }
    }

    /// Runs closed lists and waits for them, and with [`late`] for
    /// everything sent to the card before them, which is settled first.
    pub(crate) fn execute(&self, runs: &[&HostList]) {
        // Behind what the queues were given before it and have not run
        // yet (`crate::queue::behind`).
        crate::queue::before_sync();
        let _ = self.submit(runs, false);
    }

    /// A queue's submission with [`late`]: the host lists `lists`, taken
    /// from the title's, go to the card in `order` and are not waited for.
    /// They join the line of flights and are settled, with the others in
    /// the air, at the next point where what they do can be observed
    /// ([`late`] lists those points); their host lists are reset and kept
    /// for reuse then, as a queue does for the lists it ran.
    pub(crate) fn execute_late(&self, lists: Vec<crate::list::Detached>, order: Vec<usize>) {
        if order.is_empty() {
            crate::list::recycle(lists, &order);
            return;
        }
        late_threads();
        let sent = {
            let runs: Vec<&HostList> = order.iter().map(|&i| lists[i].host()).collect();
            self.submit(&runs, true)
        };
        let Some((mut submitted, sent)) = sent else {
            // (Not reached: a submission sent ahead always comes back.)
            crate::list::recycle(lists, &order);
            return;
        };
        let told = !sent.writes.is_empty();
        let most = {
            let mut line = line();
            let Line { line: items, written, by_copy, lists: flying, target, .. } = &mut *line;
            join_ranges(
                written,
                sent.back
                    .iter()
                    .flat_map(|(_, ranges)| ranges.iter().copied())
                    .chain(sent.backs.iter().copied()),
            );
            join_copy_ranges(
                by_copy,
                sent.back
                    .iter()
                    .flat_map(|(shadow, ranges)| {
                        let owner = std::sync::Arc::as_ptr(shadow) as usize;
                        ranges.iter().map(move |&(a, b)| (owner, a, b))
                    })
                    .chain(sent.backs.iter().map(|&(a, b)| (0, a, b))),
            );
            *flying += order.len();
            *target = *submitted;
            items.push(Late::Run(Flight {
                number: sent.number,
                fence: *target,
                lists,
                order,
                card: sent.card,
                back: sent.back,
                backs: sent.backs,
            }));
            items.extend(sent.writes.into_iter().map(|(at, what, bytes)| Late::Write(at, what, bytes)));
            *flying >= late_most()
        };
        LATE_STATS[0].fetch_add(1, Ordering::Relaxed);
        RELAY_MOVED.notify_one();
        if told && !late_writes() {
            self.settle(&mut submitted, WHY_WRITES);
        } else if most && relay_exact() {
            // Until there are fewer, not until there are none.
            relay_wait(WHY_MOST, |line| line.lists < late_most());
        } else if most {
            self.settle(&mut submitted, WHY_MOST);
        } else {
            LINE_MOVED.notify_one();
        }
        drop(submitted);
        land();
    }

    /// Sends `list`, a closed list of ours (the present's copy), as a
    /// flight ([`late_free_present`]): not waited for, settled with the
    /// others. The caller keeps the list and does not reset it before the
    /// submission whose number comes back has finished ([`FINISHED`]); 0 if
    /// it ran and was waited for after all.
    fn execute_own(&self, list: &HostList) -> u64 {
        late_threads();
        let Some((submitted, sent)) = self.submit(&[list], true) else {
            return 0;
        };
        let number = sent.number;
        {
            let mut line = line();
            let Line { line: items, written, by_copy, lists: flying, target, .. } = &mut *line;
            join_ranges(
                written,
                sent.back
                    .iter()
                    .flat_map(|(_, ranges)| ranges.iter().copied())
                    .chain(sent.backs.iter().copied()),
            );
            join_copy_ranges(
                by_copy,
                sent.back
                    .iter()
                    .flat_map(|(shadow, ranges)| {
                        let owner = std::sync::Arc::as_ptr(shadow) as usize;
                        ranges.iter().map(move |&(a, b)| (owner, a, b))
                    })
                    .chain(sent.backs.iter().map(|&(a, b)| (0, a, b))),
            );
            *flying += 1;
            *target = *submitted;
            items.push(Late::Run(Flight {
                number,
                fence: *target,
                lists: Vec::new(),
                order: Vec::new(),
                card: sent.card,
                back: sent.back,
                backs: sent.backs,
            }));
            items.extend(sent.writes.into_iter().map(|(at, what, bytes)| Late::Write(at, what, bytes)));
        }
        LATE_STATS[0].fetch_add(1, Ordering::Relaxed);
        LINE_MOVED.notify_one();
        RELAY_MOVED.notify_one();
        drop(submitted);
        number
    }

    /// Waits for the card to run every flight in the air ([`late`]) and
    /// does what they left to do, as [`Host::execute`] does for one
    /// submission after its wait: the pieces their shaders wrote in copies
    /// in the card's memory are fetched and merged (each once), the ranges
    /// they declared in the other copies are compared and copied back
    /// (each byte once, whichever flights declared it), and then, in the
    /// order they were given: their bottom-of-pipe writes and the fence
    /// signals behind them. `submitted` is the submission lock's value;
    /// `why` one of the `WHY_` places. Nothing happens if nothing flies.
    ///
    /// Everything in the air is settled, never the first flights alone:
    /// the merge writes the copies' CPU side, which a flight still on the
    /// card may be writing too.
    fn settle(&self, submitted: &mut u64, why: usize) {
        if !late() {
            return;
        }
        if relay() {
            // Nothing is settled here: the thread that lands flights does
            // it, in order, and this waits until nothing is in the air and
            // nothing is being landed. Every point that settled before
            // comes here, so each still sees title memory, fences and
            // bottom-of-pipe values as after a settle.
            relay_wait(why, |line| line.line.is_empty() && !line.settling && async_idle());
            return;
        }
        let was_working = working(true);
        let mut counted = false;
        loop {
            let items = {
                let mut line = line();
                if line.line.is_empty() {
                    line.settling = false;
                    break;
                }
                line.settling = true;
                line.written.clear();
                line.by_copy.clear();
                line.lists = 0;
                std::mem::take(&mut line.line)
            };
            let flights = items.iter().filter(|i| matches!(i, Late::Run(_))).count();
            if !counted {
                counted = true;
                LATE_STATS[1].fetch_add(1, Ordering::Relaxed);
                LATE_STATS[why].fetch_add(1, Ordering::Relaxed);
            }
            LATE_STATS[11].fetch_add(flights as u64, Ordering::Relaxed);
            if flights > 0 {
                submitting(2, flights);
                let waiting = std::time::Instant::now();
                // SAFETY: the fence and event are ours, under the
                // submission lock; `submitted` is the last value signalled.
                unsafe {
                    match self.fence.SetEventOnCompletion(*submitted, self.event) {
                        Ok(()) => {
                            working(false);
                            WaitForSingleObject(self.event, INFINITE);
                            stood(waiting);
                            working(true);
                        }
                        Err(e) => tracing::warn!(error = %e, "submissions sent ahead not waited for"),
                    }
                }
                let gpu = waiting.elapsed();
                note_waited(why, gpu.as_micros() as u64);
                submission_us(1, gpu.as_micros() as u64);
                #[cfg(feature = "diagnostics")]
                crate::copystat::card_waited(gpu.as_micros() as u64, || {
                    format!("{flights} submissions sent ahead, settled together")
                });
                // SAFETY: a plain query.
                if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                    let runs: Vec<&HostList> = items
                        .iter()
                        .filter_map(|i| match i {
                            Late::Run(f) => Some(f),
                            _ => None,
                        })
                        .flat_map(|f| f.order.iter().map(|&i| f.lists[i].host()))
                        .collect();
                    fail(format_args!(
                        "host GPU device removed after {flights} submissions sent ahead (VICTORY_LATE_WAIT=1) of {} lists{}{}",
                        runs.len(),
                        self.removal_data(&runs),
                        last_notes(&runs)
                    ));
                }
                submitting(3, flights);
                let copying = std::time::Instant::now();
                let mut seen = std::collections::HashSet::new();
                let mut back: Vec<Declared> = Vec::new();
                for item in &items {
                    let Late::Run(flight) = item else {
                        continue;
                    };
                    if let Some(card) = &flight.card {
                        self.card_back(card, submitted, &mut seen);
                    }
                    for (shadow, ranges) in &flight.back {
                        match back.iter_mut().find(|(s, _)| std::sync::Arc::ptr_eq(s, shadow)) {
                            Some((_, all)) => {
                                for range in ranges {
                                    if !all.contains(range) {
                                        all.push(*range);
                                    }
                                }
                            }
                            None => back.push((shadow.clone(), ranges.clone())),
                        }
                    }
                }
                hand_back(&back);
                copy_back(0, copying.elapsed().as_micros() as u64);
            }
            // In the order given: what tells the title the card is done.
            let mut last = 0;
            let mut landed = Vec::new();
            for item in items {
                match item {
                    Late::Run(flight) => {
                        last = flight.number;
                        for &i in &flight.order {
                            read_backs(flight.lists[i].host());
                        }
                        if let Some(card) = flight.card {
                            self.card_done(card);
                        }
                        landed.push((flight.lists, flight.order));
                    }
                    Late::Write(at, what, bytes) => what.write(at as *mut u8, bytes),
                    Late::Signal(fence, value) => crate::fence::set(fence as crate::object::This, value),
                }
            }
            if last > 0 {
                FINISHED.store(last, Ordering::Release);
                bury(last);
            }
            line().landed.extend(landed);
        }
        if counted {
            self.log_messages();
            submitting(0, 0);
            // What was settled may be what a queue stands at, and the
            // caller may not be one that looks (a read-back, a present).
            if why != WHY_CARD_DONE {
                line().poke = true;
                LINE_MOVED.notify_one();
            }
        }
        working(was_working);
    }

    /// [`Host::execute`], or with `late` the first half of
    /// [`Host::execute_late`]: the lists are prepared and sent, and what is
    /// to be done once they have run comes back with the submission lock
    /// still held.
    fn submit<'a>(&'a self, runs: &[&HostList], late: bool) -> Option<(std::sync::MutexGuard<'a, u64>, Sent)> {
        #[cfg(feature = "diagnostics")]
        let called = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::copystat::submission_begins();
        #[cfg(feature = "diagnostics")]
        crate::fxprof::seen(crate::fxprof::Seen::Submit);
        let profile = prep_profile();
        let mut at = std::time::Instant::now();
        // One submission at a time from preparation to copy-back: snapshots
        // and the ranges shaders write are per host copy, not per submission.
        let mut submitted = self.submitted.lock().unwrap_or_else(|e| e.into_inner());
        let was_working = working(true);
        // `VICTORY_NATIVE_GPUMEM`: the title's page maps since the last
        // submission, as tile mappings ahead of it on the queue.
        crate::native::gpumem::flush(&self.device, &self.queue);
        crate::native::buffers::line();
        submitting(1, runs.len());
        for &list in runs {
            let uploads = list.deferred().builder_uploads.clone();
            for upload in uploads {
                upload.prepare(list);
            }
        }
        // Flights in the air ([`late`]): a submission that is waited for
        // settles them first, one sent ahead only if preparing it reads
        // what they write or would overwrite an in-flight texture staging
        // buffer.
        let mut redirects: Vec<Redirect> = Vec::new();
        if !late {
            self.settle(&mut submitted, WHY_SYNC);
        } else if readfix() {
            redirects = readfix_settle(self, &mut submitted, runs);
        } else if let Some(why) = late_blocked(runs) {
            if (relay_exact() || (sync_precise() && relay())) && why == WHY_READ {
                // Only until the flights that may write what is read have
                // landed: their copy-back's end, not the card's other work.
                relay_wait(why, |line| {
                    let immed = line.immed_writes();
                    blocked_by(&line.written, &line.by_copy, &immed, runs, false).is_none()
                });
            } else {
                self.settle(&mut submitted, why);
            }
        }
        if late && sync_precise() {
            // A staging buffer's write waits for its readers itself
            // (`crate::texture`); counted where this would have waited.
            if staging_in_flight(runs) {
                crate::texture::SYNC_STATS[3].fetch_add(1, Ordering::Relaxed);
            }
            // What is read is no longer written by a flight in the air
            // (but where it is served on the card, `VICTORY_READFIX`).
            if redirects.is_empty() {
                precise_check(runs);
            }
        } else if late && staging_in_flight(runs) {
            if relay_exact() {
                relay_wait(WHY_READ, |_| !staging_in_flight(runs));
            } else {
                self.settle(&mut submitted, WHY_READ);
            }
        }
        // Descriptors the title copied into placed heaps directly, before
        // command lists that index those slots are submitted.
        crate::descriptor::sync_heaps();
        let behind = late && flying();
        // Before this submission counts as started: what the sweep lets go
        // of is named by no list, and nothing runs now (behind flights the
        // sweep waits for a submission that finds none in the air: there is
        // one at every present).
        static SWEPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if !behind {
            sweep();
            SWEPT.store(now_ms(), Ordering::Relaxed);
        } else if late_free_present() && now_ms() > SWEPT.load(Ordering::Relaxed) + 1500 {
            // With the present sent ahead nothing else brings a moment
            // with nothing in the air: one is made for the sweep.
            self.settle(&mut submitted, WHY_SYNC);
            sweep();
            SWEPT.store(now_ms(), Ordering::Relaxed);
        }
        let behind = late && flying();
        // The slots that waited for a texture's host copy, before the
        // lists that may read them run.
        crate::descriptor::make_waiting();
        let this_submission = STARTED.fetch_add(1, Ordering::AcqRel) + 1;
        // The ranges shaders write are counted for it (with the relay).
        crate::shadow::preparing(this_submission);
        let started = std::time::Instant::now();
        if profile {
            prof_since(Prof::Before, &mut at);
            prof(Prof::Submissions, 1);
            prof(Prof::Lists, runs.len() as u64);
        }
        #[cfg(feature = "diagnostics")]
        let lock = (started - called).as_micros() as u64;
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::Submissions, 1);
        let mut lists: Vec<Option<ID3D12CommandList>> = Vec::with_capacity(2 * runs.len());
        // Where each run's lists start in `lists`.
        let mut firsts = Vec::with_capacity(runs.len());
        let mut batch = Batch::default();
        // The time a settle of flights takes inside this preparing: the
        // wait for the card and the copy-back have their own places on the
        // frame line (run 92 showed them twice, "preparing" 250 ms for 135).
        let mut settled = std::time::Duration::ZERO;
        // One write-watch asking for everything the submission refreshes
        // (`VICTORY_ASK_ONCE`): per copy, the span of its refreshed ranges.
        if crate::watch::once() {
            let mut spans: std::collections::HashMap<usize, (usize, usize)> = std::collections::HashMap::new();
            for list in runs {
                for step in &list.deferred().copies {
                    if let Step::Refresh(shadow, address, len) = step
                        && !shadow.unwatched()
                    {
                        let span = spans
                            .entry(std::sync::Arc::as_ptr(shadow) as usize)
                            .or_insert((*address, *address + *len));
                        *span = (span.0.min(*address), span.1.max(*address + *len));
                    }
                }
            }
            let ranges: Vec<(usize, usize)> = spans.into_values().collect();
            crate::watch::ask_once(&ranges);
            // Behind flights: the title wrote memory that a flight's shaders
            // may write too, and this submission refreshes it. The flights
            // are settled, and the asking made again: the copy-back wrote
            // title memory, which other copies of it must see.
            if behind && stale_under_flights(runs) {
                let settling = std::time::Instant::now();
                if !(read_settle_precise() && wait_copies_landed(&refreshed_copies(runs))) {
                    self.settle(&mut submitted, WHY_STALE);
                }
                settled += settling.elapsed();
                crate::watch::ask_once(&ranges);
            }
        } else if behind && stale_under_flights(runs) {
            let settling = std::time::Instant::now();
            if !(read_settle_precise() && wait_copies_landed(&refreshed_copies(runs)) && !stale_under_flights(runs)) {
                self.settle(&mut submitted, WHY_STALE);
            }
            settled += settling.elapsed();
        }
        if profile {
            prof_since(Prof::Ask, &mut at);
        }
        for list in runs {
            if list.broken.load(std::sync::atomic::Ordering::Relaxed) {
                firsts.push(lists.len());
                continue;
            }
            list.run_deferred(&mut batch);
            firsts.push(lists.len());
            if let Some(prelude) = list.prelude() {
                lists.push(Some(prelude));
            }
            lists.push(list.list.cast().ok());
        }
        if profile {
            prof_since(Prof::Deferred, &mut at);
        }
        batch.flush();
        batch.textures_done();
        crate::watch::ask_once_end();
        if profile {
            prof_since(Prof::Flush, &mut at);
        }
        // The snapshots of the ranges shaders may write, if they were left
        // for now (`crate::shadow::wide_back`): the refreshes are done, the
        // lists have not gone to the card.
        {
            #[cfg(feature = "diagnostics")]
            let snapshots = std::time::Instant::now();
            crate::shadow::take_snapshots();
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::SnapshotUs, snapshots.elapsed().as_micros() as u64);
        }
        if profile {
            prof_since(Prof::Snapshots, &mut at);
        }
        // Copies in the card's memory: what changed on their CPU side goes
        // up ahead of the lists, and a compare of what the shaders may
        // write follows them in the same submission.
        let flown = late && relay();
        if flown && self.card_results_low() {
            let settling = std::time::Instant::now();
            self.settle(&mut submitted, WHY_MOST);
            settled += settling.elapsed();
        }
        let card = self.card_before(runs, flown, *submitted);
        if let Some(check) = card.as_ref().and_then(|c| c.check.clone()) {
            lists.push(Some(check));
        }
        if profile {
            prof_since(Prof::Card, &mut at);
        }
        // Served on the card: after the flights before and the copies'
        // own bytes sent up, before the lists (`VICTORY_READFIX`).
        self.readfix_copy(&redirects, this_submission);
        if profile {
            prof_since(Prof::Readfix, &mut at);
        }
        let prepared = std::time::Instant::now();
        if late {
            for list in runs {
                for step in &list.deferred().copies {
                    if let Step::Texture(staging) = step {
                        staging.mark_in_flight(this_submission);
                    }
                }
            }
            *submitted += 1;
            // SAFETY: closed lists; the fence is ours.
            let signalled = unsafe {
                self.queue.ExecuteCommandLists(&lists);
                self.queue.Signal(&self.fence, *submitted)
            };
            self.card_sent();
            if let Err(e) = signalled {
                fail(format_args!("a submission sent ahead could not be followed by a signal ({e})"));
            }
            crate::texture::copies_sent(*submitted);
            let back = declared_writes(runs);
            {
                let declared = back
                    .iter()
                    .flat_map(|(s, ranges)| {
                        let key = std::sync::Arc::as_ptr(s) as usize;
                        ranges.iter().map(move |&(a, b)| (key, a, b))
                    })
                    .collect();
                let left_out = runs.iter().flat_map(|l| l.deferred().narrowed.clone()).collect();
                crate::narrow::flight(declared, left_out);
            }
            if readfix() && !back.is_empty() {
                READFIX_OWNERS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(back.iter().map(|(shadow, _)| (this_submission, shadow.clone())));
            }
            let backs: Vec<(usize, usize)> = runs
                .iter()
                .flat_map(|l| {
                    l.deferred()
                        .backs
                        .iter()
                        .filter(|&&(_, _, [row, rows, _])| row > 0 && rows > 0)
                        .map(|&(_, at, [row, rows, pitch])| {
                            let span = rows.saturating_sub(1).saturating_mul(pitch).saturating_add(row);
                            (at, at.saturating_add(span))
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
            let writes = runs.iter().flat_map(|l| l.deferred().writes.clone()).collect();
            submission_us(0, (prepared - started).saturating_sub(settled).as_micros() as u64);
            submitting(0, 0);
            #[cfg(feature = "diagnostics")]
            crate::copystat::submission_ends(lock);
            if profile {
                prof_since(Prof::After, &mut at);
            }
            let sent = Sent {
                number: this_submission,
                card,
                back,
                backs,
                writes,
            };
            working(was_working);
            return Some((submitted, sent));
        }
        // Diagnostics: each list on its own, timed (`VICTORY_GPU_TIMING`).
        #[cfg(feature = "diagnostics")]
        let groups: Vec<&[Option<ID3D12CommandList>]> = if timing() {
            firsts
                .iter()
                .zip(firsts.iter().skip(1).chain([&lists.len()]))
                .map(|(&a, &b)| &lists[a..b])
                .collect()
        } else {
            vec![&lists[..]]
        };
        #[cfg(not(feature = "diagnostics"))]
        let groups = [&lists[..]];
        submitting(2, runs.len());
        for (i, group) in groups.iter().enumerate() {
            *submitted += 1;
            let started = std::time::Instant::now();
            // SAFETY: closed lists; the fence and event are ours.
            unsafe {
                self.queue.ExecuteCommandLists(group);
                self.card_sent();
                let waited = self
                    .queue
                    .Signal(&self.fence, *submitted)
                    .and_then(|()| self.fence.SetEventOnCompletion(*submitted, self.event));
                match waited {
                    Ok(()) => {
                        working(false);
                        WaitForSingleObject(self.event, INFINITE);
                        stood(started);
                        working(true);
                    }
                    Err(e) => tracing::warn!(error = %e, "host submission not waited for"),
                }
            }
            // With timing on each group is one of `runs` (and the compare
            // of copies in the card's memory, with the last): every
            // command's time on the card goes to the sums by pipeline.
            #[cfg(feature = "diagnostics")]
            if timing() {
                // SAFETY: a plain query.
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                for (ms, op) in runs[i].slowest(frequency, usize::MAX) {
                    crate::copystat::command_took(&op, ms);
                }
            }
            // A submission of one list was never named here (`groups.len()
            // > 1` was the test): the two 50 ms lists of the gameplay
            // frame, each a submission of its own, printed nothing.
            #[cfg(feature = "diagnostics")]
            if timing() && started.elapsed() > std::time::Duration::from_millis(50) {
                // SAFETY: a plain query.
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                let commands = runs[i].notes.lock().unwrap_or_else(|e| e.into_inner()).len();
                tracing::warn!(ms = started.elapsed().as_millis(), commands, "slow host list");
                for (ms, op) in runs[i].slowest(frequency, 6) {
                    tracing::warn!("slow list command, {ms:.1} ms: {op}");
                }
            }
            let _ = (i, started);
        }
        let _ = &firsts;
        // (Waited for: what it copied from is free again at once.)
        crate::texture::copies_sent(*submitted);
        FINISHED.store(this_submission, Ordering::Release);
        bury(this_submission);
        let gpu = prepared.elapsed();
        submission_us(0, (prepared - started).as_micros() as u64);
        submission_us(1, gpu.as_micros() as u64);
        #[cfg(feature = "diagnostics")]
        crate::copystat::card_waited(gpu.as_micros() as u64, || {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| l.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clone())
                .collect();
            let text = format!("{} lists, pipelines {pipelines:?}", runs.len());
            text.chars().take(1200).collect()
        });
        // The host resets a device whose submission runs for about 2 s.
        if gpu > std::time::Duration::from_millis(250) {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| l.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clone())
                .collect();
            tracing::warn!(ms = gpu.as_millis(), lists = runs.len(), ?pipelines, "slow host submission");
        }
        // SAFETY: a plain query.
        if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
            fail(format_args!(
                "host GPU device removed after a submission of {} lists{}{}",
                runs.len(),
                self.removal_data(runs),
                last_notes(runs)
            ));
        }
        submitting(3, runs.len());
        // What shaders wrote goes back before the bottom-of-pipe writes,
        // which tell the title the GPU is done; each range once per
        // submission, however many dispatches declared it (the world's
        // passes declare the same big ranges dozens of times).
        let copying = std::time::Instant::now();
        // For copies in the card's memory the compare has run there: the
        // pieces that differ are fetched and merged.
        if let Some(card) = &card {
            self.card_back(card, &mut submitted, &mut std::collections::HashSet::new());
        }
        let back = declared_writes(runs);
        hand_back(&back);
        if let Some(card) = card {
            self.card_done(card);
        }
        #[cfg(feature = "diagnostics")]
        if !back.is_empty() {
            crate::copystat::add(crate::copystat::C::BackSubmissions, 1);
        }
        // Nothing of this submission goes back to the title: no range of
        // shader writes, no value written when it is done. The wait for
        // such a one is only for order and for what the card still holds.
        #[cfg(feature = "diagnostics")]
        if back.is_empty() && runs.iter().all(|l| l.deferred().writes.is_empty()) {
            use crate::copystat::{C, add};
            add(C::NothingBack, 1);
            add(C::NothingBackUs, gpu.as_micros() as u64);
        }
        copy_back(0, copying.elapsed().as_micros() as u64);
        for list in runs {
            // Read backs first: a value written at the bottom of the pipe
            // may be what tells the title that they are there.
            read_backs(list);
            for &(at, what, bytes) in &list.deferred().writes {
                what.write(at as *mut u8, bytes);
            }
        }
        self.log_messages();
        submitting(0, 0);
        #[cfg(feature = "diagnostics")]
        crate::copystat::submission_ends(lock);
        // With the relay: nothing is in the air now, so no fetch still to
        // be merged is older than the sends to card copies so far, and
        // their notes can go.
        if relay() {
            crate::shadow::relay_fetched(*QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()));
        }
        working(was_working);
        None
    }

    /// With `VICTORY_D3D12_DRED`: the faulting address and the allocations
    /// it lies in (by the names we give them) or the title memory copy just
    /// below it; for each list that did not finish, the command it stopped
    /// at and what the draws and dispatches in flight there run (or, in
    /// lists that never started, the pipelines they bind).
    fn removal_data(&self, runs: &[&HostList]) -> String {
        let Ok(dred) = self.device.cast::<ID3D12DeviceRemovedExtendedData1>() else {
            return String::new();
        };
        let mut text = String::new();
        let mut fault_va = None;
        // SAFETY: the outputs are the runtime's, valid while the device
        // lives; nodes are walked as linked, histories have their counts.
        unsafe {
            if let Ok(fault) = dred.GetPageFaultAllocationOutput1()
                && fault.PageFaultVA != 0
            {
                let va = fault.PageFaultVA;
                fault_va = Some(va);
                text.push_str(&format!("; page fault at {va:#x}"));
                for (what, head) in [
                    ("inside", fault.pHeadExistingAllocationNode),
                    ("inside freed", fault.pHeadRecentFreedAllocationNode),
                ] {
                    let mut names = Vec::new();
                    let mut node = head;
                    while let Some(n) = node.as_ref()
                        && names.len() < 8
                    {
                        let name = if n.ObjectNameW.is_null() {
                            "unnamed".to_owned()
                        } else {
                            n.ObjectNameW.to_string().unwrap_or_default()
                        };
                        names.push(format!("{name} (type {})", n.AllocationType.0));
                        node = n.pNext;
                    }
                    if !names.is_empty() {
                        text.push_str(&format!("; {what} {names:?}"));
                    }
                }
                text.push_str(&crate::shadow::near(va));
                text.push_str(&crate::descriptor::host::near_heaps(va));
            }
            // The plain output: the extended one (with marker texts) came
            // back without stopped lists on this host.
            let crumbs = match dred.GetAutoBreadcrumbsOutput() {
                Ok(crumbs) => crumbs,
                Err(e) => {
                    text.push_str(&format!("; no breadcrumbs ({e})"));
                    return text;
                }
            };
            // Every list's progress (done/count), in the runtime's order.
            let mut progress = Vec::new();
            let mut node = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = node.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                let ours = p.pCommandList.as_ref().map(|l| l.as_raw());
                let index = runs.iter().position(|r| Some(r.list.as_raw()) == ours);
                progress.push(format!("{done}/{} {index:?}", p.BreadcrumbCount));
                node = p.pNext;
            }
            text.push_str(&format!("; breadcrumbs (done/count, list) {progress:?}"));
            let mut plain = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = plain.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                if done < p.BreadcrumbCount && !p.pCommandHistory.is_null() {
                    let history =
                        std::slice::from_raw_parts(p.pCommandHistory, p.BreadcrumbCount as usize);
                    text.push_str(&format!(
                        "; a list stopped at command {done} of {} (op {})",
                        p.BreadcrumbCount, history[done as usize].0
                    ));
                    let list = p.pCommandList.as_ref().map(|l| l.as_raw());
                    let run = runs.iter().find(|r| Some(r.list.as_raw()) == list);
                    // The k-th marker command is the k-th note.
                    #[cfg(feature = "diagnostics")]
                    let markers: Vec<(u32, String)> = run.map_or_else(Vec::new, |r| {
                        let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
                        history
                            .iter()
                            .enumerate()
                            .filter(|(_, op)| **op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
                            .zip(notes.iter())
                            .map(|((at, _), (note, range))| {
                                let place = fault_va.zip(*range).map(|(va, r)| fault_in(va, r));
                                (at as u32, format!("{note}{}", place.unwrap_or_default()))
                            })
                            .collect()
                    });
                    #[cfg(not(feature = "diagnostics"))]
                    let markers: Vec<(u32, String)> = Vec::new();
                    #[cfg(not(feature = "diagnostics"))]
                    let _ = fault_va;
                    if done > 0 {
                        text.push_str(&in_flight(history, &markers, done as usize));
                    } else if let Some(r) = run {
                        let pipelines = r.pipelines.lock().unwrap_or_else(|e| e.into_inner());
                        text.push_str(&format!(", its pipelines {pipelines:?}"));
                    }
                }
                plain = p.pNext;
            }
        }
        text
    }

    /// With the frame line (`VICTORY_D3D12_DEBUG=1`, diagnostics builds):
    /// the debug layer's messages since the last call go to our log, the
    /// first five of each kind in full, the rest counted, with one line of
    /// the counts by kind.
    ///
    /// They were only read when the device was lost ([`fail`]): a run that
    /// survived (run 205) said nothing of what the layer had found, and
    /// what it finds is what a driver's fault would have to be traced back
    /// to (an object let go while the card uses it, a resource in the
    /// wrong state, a view of something gone).
    pub(crate) fn drain_messages(&self) {
        let Some(queue) = &self.messages else {
            return;
        };
        static SEEN: Mutex<std::collections::BTreeMap<i32, (u64, i32)>> = Mutex::new(std::collections::BTreeMap::new());
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: each message is read into a buffer of the size the queue
        // reports, then the queue is emptied.
        let stored = unsafe {
            let stored = queue.GetNumStoredMessages();
            for i in 0..stored {
                let mut size = 0;
                if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                    continue;
                }
                let mut buffer = vec![0u64; size.div_ceil(8)];
                let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                    let m = &*message;
                    let kind = seen.entry(m.ID.0).or_insert((0, m.Severity.0));
                    kind.0 += 1;
                    if kind.0 <= 5 {
                        let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                        let text = String::from_utf8_lossy(text);
                        let text = text.trim_end_matches('\0');
                        tracing::warn!(id = m.ID.0, severity = m.Severity.0, nth = kind.0, "D3D12 debug layer: {text}");
                    }
                }
            }
            queue.ClearStoredMessages();
            stored
        };
        if stored > 0 {
            let counts: Vec<String> = seen.iter().map(|(id, (n, severity))| format!("id {id} (severity {severity}) x{n}")).collect();
            tracing::info!(
                "D3D12 debug layer messages so far by kind (severity 0 corruption, 1 error, 2 warning, 3 info, 4 message): {}",
                counts.join(", ")
            );
        }
    }

    /// The host device (`crate::cardmeter`).
    pub(crate) fn device(&self) -> &ID3D12Device {
        &self.device
    }

    /// Ticks a second of the queue's timestamps; 0 if it has none.
    pub(crate) fn timestamp_frequency(&self) -> u64 {
        // SAFETY: a plain query.
        unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(0)
    }

    /// Moves the debug layer's messages into our log; returns the first
    /// error among them.
    pub(crate) fn log_messages(&self) -> Option<String> {
        let Some(queue) = &self.messages else {
            return None;
        };
        let mut first_error = None;
        // SAFETY: each message is read into a buffer of the size the queue
        // reports, then the queue is emptied.
        unsafe {
            for i in 0..queue.GetNumStoredMessages() {
                let mut size = 0;
                if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                    continue;
                }
                let mut buffer = vec![0u64; size.div_ceil(8)];
                let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                    let m = &*message;
                    let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                    let text = String::from_utf8_lossy(text);
                    let text = text.trim_end_matches('\0');
                    tracing::warn!(id = m.ID.0, severity = m.Severity.0, "D3D12: {text}");
                    if m.Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 && first_error.is_none() {
                        first_error = Some(text.to_owned());
                    }
                }
            }
            queue.ClearStoredMessages();
        }
        #[cfg(test)]
        if queue2_tests::GPU_VALIDATION.load(Ordering::Relaxed) && let Some(error) = &first_error {
            // The relay must finish landing this submission before a test can fail.
            queue2_tests::VALIDATION_ERROR.lock().unwrap_or_else(|e| e.into_inner())
                .get_or_insert_with(|| error.clone());
        }
        first_error
    }

    /// A host render target view of `resource`, valid until it is recorded.
    pub(crate) fn rtv(
        &self,
        resource: Option<&ID3D12Resource>,
        desc: Option<&D3D12_RENDER_TARGET_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_rtv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource, or none with a
        // description (a typed null view).
        unsafe {
            let mut handle = self.rtvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.rtv_size) as usize;
            self.device
                .CreateRenderTargetView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }

    /// A host depth view of `resource`, valid until it is recorded.
    pub(crate) fn dsv(
        &self,
        resource: &ID3D12Resource,
        desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_dsv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource.
        unsafe {
            let mut handle = self.dsvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.dsv_size) as usize;
            self.device
                .CreateDepthStencilView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }
}

impl HostList {
    /// The timestamp heap and its read-back buffer (diagnostics, timing on).
    #[cfg(feature = "diagnostics")]
    fn stamps(&self) -> Option<&(ID3D12QueryHeap, ID3D12Resource)> {
        if !timing() {
            return None;
        }
        self.stamps
            .get_or_init(|| {
                let device = &host()?.device;
                let mut heap: Option<ID3D12QueryHeap> = None;
                let mut back: Option<ID3D12Resource> = None;
                let readback = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: u64::from(STAMPS) * 8,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                };
                // SAFETY: plain object creation.
                unsafe {
                    device
                        .CreateQueryHeap(
                            &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: STAMPS, NodeMask: 0 },
                            &mut heap,
                        )
                        .ok()?;
                    device
                        .CreateCommittedResource(
                            &readback,
                            D3D12_HEAP_FLAG_NONE,
                            &desc,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            None,
                            &mut back,
                        )
                        .ok()?;
                }
                Some((heap?, back?))
            })
            .as_ref()
    }

    /// The noted commands that took longest on the GPU, longest first, as
    /// (milliseconds, text) (diagnostics, timing on, after the list ran).
    #[cfg(feature = "diagnostics")]
    fn slowest(&self, frequency: u64, n: usize) -> Vec<(f64, String)> {
        let notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        let (Some((_, back)), count) = (self.stamps(), notes.len().min(STAMPS as usize - 1)) else {
            return Vec::new();
        };
        let mut data = std::ptr::null_mut();
        let range = D3D12_RANGE { Begin: 0, End: (count + 1) * 8 };
        // SAFETY: a read-back buffer the finished list resolved into.
        let stamps: Vec<u64> = unsafe {
            if back.Map(0, Some(&range), Some(&mut data)).is_err() {
                return Vec::new();
            }
            let s = std::slice::from_raw_parts(data.cast::<u64>(), count + 1).to_vec();
            back.Unmap(0, None);
            s
        };
        let mut times: Vec<(f64, String)> = (0..count)
            .map(|i| {
                let ticks = stamps[i + 1].saturating_sub(stamps[i]);
                (ticks as f64 * 1e3 / frequency.max(1) as f64, notes[i].0.chars().take(300).collect())
            })
            .collect();
        times.sort_by(|a, b| b.0.total_cmp(&a.0));
        times.truncate(n);
        times
    }

    pub(crate) fn close(&self) {
        // Held barriers (`VICTORY_PREP_BARRIERS`) and a held barrier on all
        // resources go in before the list's end (and before the copies of
        // depth twins below).
        self.flush_barriers();
        self.hazard_flush();
        self.twins_to_depth();
        let scratch = self.indirect_args.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some((scratch, _)) = scratch {
            self.indirect_common(&scratch);
        }
        self.card_budget_out();
        // The scratch buffers its prelude fills and leaves readable as
        // arguments: common again behind the list, as every scratch buffer.
        for set in self.gather.lock().unwrap_or_else(|e| e.into_inner()).sets.iter().filter(|s| s.records() > 0) {
            // SAFETY: an open list and a live buffer.
            unsafe {
                self.list.ResourceBarrier(&[transition(
                    &set.scratch,
                    D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                    D3D12_RESOURCE_STATE_COMMON,
                )]);
            }
        }
        crate::gpuprof::end(&self.list);
        crate::cardmeter::end(&self.list, crate::cardmeter::Span::from_raw(self.meter.swap(u32::MAX, Ordering::Relaxed)));
        #[cfg(feature = "diagnostics")]
        if let Some((heap, back)) = self.stamps() {
            let count = (self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32).min(STAMPS - 1);
            // SAFETY: an open list, our heap and read-back buffer.
            unsafe {
                self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, count);
                self.list.ResolveQueryData(heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, count + 1, back, 0);
            }
        }
        // SAFETY: our list.
        if let Err(e) = unsafe { self.list.Close() } {
            if std::env::var_os("VICTORY_D3D12_DEBUG").is_some() {
                fail(format_args!(
                    "a host command list did not close ({e}): a command recorded into it was invalid"
                ));
            }
            tracing::warn!(error = %e, "a host command list did not close; its work is dropped");
            self.broken.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Only after the list's last submission finished (every one is waited
    /// for).
    /// `VICTORY_BARRIER_MERGE`: work just recorded may write through
    /// unordered access (`writes`: whether its shaders can).
    pub(crate) fn uav_work(&self, writes: bool) {
        if writes {
            self.uav_pending.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// `VICTORY_BARRIER_MERGE`: whether unordered access writes recorded
    /// since the last barrier on all resources are still unordered.
    pub(crate) fn uav_pending(&self) -> bool {
        self.uav_pending.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn set_uav_pending(&self, pending: bool) {
        self.uav_pending.store(pending, std::sync::atomic::Ordering::Relaxed);
    }

    /// `VICTORY_BARRIER_HAZARD`: the hazards of the list.
    fn hazards(&self) -> std::sync::MutexGuard<'_, victory_core::sync::Hazards> {
        self.hazards.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records a barrier on all resources (`VICTORY_BARRIER_HAZARD`): every
    /// earlier shader access finishes before later work.
    fn all_uav_barrier(&self) {
        let barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(None),
                }),
            },
        };
        self.record_barriers(&[barrier]);
        // (It also orders what `VICTORY_BARRIER_MERGE` counts as pending.)
        self.set_uav_pending(false);
    }

    /// `VICTORY_BARRIER_HAZARD`: work the hazards do not track is about to
    /// be recorded: a held barrier goes in first if anything is pending.
    pub(crate) fn hazard_flush(&self) {
        if !barrier_hazard() {
            return;
        }
        let needed = self.hazards().flush();
        if needed {
            self.all_uav_barrier();
            HAZARD_STATS[3].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// `VICTORY_BARRIER_HAZARD`: the title asks for earlier shader work to
    /// finish (`None`: all of it; else that of `key`). Held until a
    /// dispatch or other work needs it.
    pub(crate) fn hazard_request(&self, key: Option<victory_core::sync::Key>) {
        let mut h = self.hazards();
        match key {
            None => {
                h.request_all();
                HAZARD_STATS[0].fetch_add(1, Ordering::Relaxed);
            }
            Some(k) => {
                h.request(k);
                HAZARD_STATS[1].fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// `VICTORY_BARRIER_HAZARD`: a dispatch accessing `access` comes next;
    /// a held barrier goes in before it if they conflict.
    pub(crate) fn hazard_op(&self, access: &victory_core::sync::Access) {
        let needed = self.hazards().op(access);
        HAZARD_STATS[4].fetch_add(1, Ordering::Relaxed);
        if access.unknown {
            HAZARD_STATS[5].fetch_add(1, Ordering::Relaxed);
        }
        if needed {
            self.all_uav_barrier();
            HAZARD_STATS[2].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn reset(&self) {
        self.broken.store(false, std::sync::atomic::Ordering::Relaxed);
        self.uav_pending.store(true, std::sync::atomic::Ordering::Relaxed);
        *self.hazards() = victory_core::sync::Hazards::new();
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        self._alive.used();
        // Held barriers belong to the recording the reset ends (close
        // recorded them; a list reset unclosed drops them).
        if self.has_held_barriers.swap(false, Ordering::AcqRel) {
            self.held_barriers.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
        if prep_reuse() {
            self.deferred().clear_keeping();
            PREP_STATS[P::Reused as usize].fetch_add(1, Ordering::Relaxed);
        } else {
            *self.deferred() = Deferred::default();
        }
        self.writes.store(0, Ordering::Relaxed);
        {
            let mut chunks = self.clamp_chunks.lock().unwrap_or_else(|e| e.into_inner());
            chunks.used = 0;
            chunks.at = 0;
            chunks.barrier = false;
        }
        self.states.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.twins.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.aliases.lock().unwrap_or_else(|e| e.into_inner()).clear();
        {
            let mut gather = self.gather.lock().unwrap_or_else(|e| e.into_inner());
            let Gather { closed, sets, spare, .. } = &mut *gather;
            *closed = false;
            for set in sets.drain(..) {
                if spare.len() < GATHER_SETS {
                    spare.push(set.into_scratch());
                }
            }
        }
        {
            let mut pipelines = self.pipelines.lock().unwrap_or_else(|e| e.into_inner());
            // For the indirect guard's report of the run just reset.
            if crate::indirect::guard_on() && !pipelines.is_empty() {
                let text: String = format!("{pipelines:?}").chars().take(1500).collect();
                GUARD_PIPELINES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(std::ptr::from_ref(self) as usize, text);
            }
            pipelines.clear();
        }
        #[cfg(feature = "diagnostics")]
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).clear();
        {
            let mut scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
            (scratch.current, scratch.used) = (0, 0);
            // Every submission of the list has run: nothing reads its
            // scratch memory any more, and the next list to ask takes it.
            if scratch_pool() {
                give_chunks(std::mem::take(&mut scratch.chunks));
            }
        }
        // SAFETY: the list is not executing.
        let reset = unsafe {
            self.allocator
                .Reset()
                .and_then(|()| self.list.Reset(&self.allocator, None))
        };
        if let Err(e) = reset {
            fail(format_args!("a host command list did not reset ({e})"));
        }
        self.meter
            .store(crate::cardmeter::begin(&self.list, crate::cardmeter::Kind::Lists).raw(), Ordering::Relaxed);
        crate::gpuprof::begin(&self.list);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::ResetNs, started.elapsed().as_nanos() as u64);
    }

    /// Moves a title texture's host copy to `wanted` (a host state) in this
    /// list: the host texture and the state it leaves, if that is a change.
    /// The list's first use of a texture records nothing: its prelude brings
    /// the texture there at submission.
    pub(crate) fn transition(
        &self,
        resource: crate::object::This,
        wanted: u32,
    ) -> Option<(&'static ID3D12Resource, u32)> {
        let texture = crate::resource::host_resource(resource)?;
        let copy = crate::resource::host_copy(resource)?;
        let wanted = legal_state(texture, wanted);
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        match states.entry(std::sync::Arc::as_ptr(&copy) as usize) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((wanted, wanted, copy, u64::from(wanted & WRITE_STATES != 0)));
                None
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                if wanted & WRITE_STATES != 0 {
                    o.get_mut().3 += 1;
                }
                let before = std::mem::replace(&mut o.get_mut().1, wanted);
                (before != wanted).then_some((texture, before))
            }
        }
    }

    /// Moves host texture `copy` to `wanted` (a host state) in this list,
    /// as [`HostList::transition`] does a title resource's host copy, and
    /// records the barrier: for textures that are not a title resource's
    /// host copy (colour twins) and for the copies between the two.
    fn settle(&self, copy: &std::sync::Arc<crate::resource::HostTexture>, wanted: u32) {
        let wanted = legal_state(&copy.texture, wanted);
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let before = match states.entry(std::sync::Arc::as_ptr(copy) as usize) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((wanted, wanted, copy.clone(), u64::from(wanted & WRITE_STATES != 0)));
                None
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                if wanted & WRITE_STATES != 0 {
                    o.get_mut().3 += 1;
                }
                let before = std::mem::replace(&mut o.get_mut().1, wanted);
                (before != wanted).then_some(before)
            }
        };
        drop(states);
        if let Some(before) = before {
            let barrier = transition(
                &copy.texture,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATES(wanted as i32),
            );
            self.record_barriers(&[barrier]);
        }
    }

    /// Records `barriers` now. With `VICTORY_PREP_BARRIERS` any held ones
    /// go first, in the same call.
    pub(crate) fn record_barriers(&self, barriers: &[D3D12_RESOURCE_BARRIER]) {
        if barriers.is_empty() {
            return;
        }
        if prep_barriers() && self.has_held_barriers.load(Ordering::Acquire) {
            let words: Vec<[u64; 4]> = barriers.iter().map(barrier_words).collect();
            self.hold_barriers(&words);
            self.flush_barriers();
            return;
        }
        if prep_barriers() {
            PREP_STATS[P::BarrierCallsIn as usize].fetch_add(1, Ordering::Relaxed);
            PREP_STATS[P::BarrierCallsOut as usize].fetch_add(1, Ordering::Relaxed);
            PREP_STATS[P::BarriersIn as usize].fetch_add(barriers.len() as u64, Ordering::Relaxed);
            PREP_STATS[P::BarriersOut as usize].fetch_add(barriers.len() as u64, Ordering::Relaxed);
        }
        // SAFETY: an open list and live resources (borrowed pointers).
        unsafe { self.list.ResourceBarrier(barriers) };
    }

    /// `VICTORY_PREP_BARRIERS`: holds `barriers` (the title's, from one
    /// call) until the list's next other command, which records all held
    /// ones as one call ([`HostList::flush_barriers`]).
    /// The barriers are given as the words of `D3D12_RESOURCE_BARRIER`s
    /// ([`barrier_words`]; the title's `crate::list::Barrier` has the same
    /// layout).
    pub(crate) fn hold_barriers(&self, barriers: &[[u64; 4]]) {
        if barriers.is_empty() {
            return;
        }
        PREP_STATS[P::BarrierCallsIn as usize].fetch_add(1, Ordering::Relaxed);
        PREP_STATS[P::BarriersIn as usize].fetch_add(barriers.len() as u64, Ordering::Relaxed);
        let mut held = self.held_barriers.lock().unwrap_or_else(|e| e.into_inner());
        held.extend_from_slice(barriers);
        self.has_held_barriers.store(true, Ordering::Release);
    }

    /// `VICTORY_PREP_BARRIERS`: records the held barriers as one call,
    /// without the transitions that change nothing, the waits for unordered
    /// access another one in the run covers, and with `A -> B -> C` through
    /// a read-only `B` as `A -> C` (`victory_core::prep::coalesce`).
    pub(crate) fn flush_barriers(&self) {
        if !self.has_held_barriers.load(Ordering::Acquire) {
            return;
        }
        let started = std::time::Instant::now();
        let mut held = self.held_barriers.lock().unwrap_or_else(|e| e.into_inner());
        self.has_held_barriers.store(false, Ordering::Release);
        if held.is_empty() {
            return;
        }
        let out: Vec<[u64; 4]> = if held.len() == 1 {
            std::mem::take(&mut *held)
        } else {
            use victory_core::prep::Held;
            let kinds: Vec<Held> = held
                .iter()
                .map(|w| match w[0] as u32 {
                    0 => Held::Transition {
                        resource: w[1] as usize,
                        subresource: w[2] as u32,
                        before: (w[2] >> 32) as u32,
                        after: w[3] as u32,
                        flags: (w[0] >> 32) as u32,
                    },
                    2 => Held::Uav { resource: w[1] as usize },
                    _ => Held::Other,
                })
                .collect();
            let plan = victory_core::prep::coalesce(&kinds);
            PREP_STATS[P::Noops as usize].fetch_add(u64::from(plan.noops), Ordering::Relaxed);
            PREP_STATS[P::UavsDropped as usize].fetch_add(u64::from(plan.uavs_dropped), Ordering::Relaxed);
            PREP_STATS[P::Folded as usize].fetch_add(u64::from(plan.folded), Ordering::Relaxed);
            let out = plan
                .keep
                .iter()
                .map(|&(i, before)| {
                    let mut w = held[i];
                    if let Some(before) = before {
                        w[2] = (w[2] & 0xffff_ffff) | (u64::from(before) << 32);
                    }
                    w
                })
                .collect();
            held.clear();
            out
        };
        drop(held);
        if !out.is_empty() {
            PREP_STATS[P::BarrierCallsOut as usize].fetch_add(1, Ordering::Relaxed);
            PREP_STATS[P::BarriersOut as usize].fetch_add(out.len() as u64, Ordering::Relaxed);
            const _: () = assert!(size_of::<[u64; 4]>() == size_of::<D3D12_RESOURCE_BARRIER>());
            // SAFETY: the words of barriers built by this crate, borrowed
            // resource pointers never released through this slice.
            let barriers = unsafe { std::slice::from_raw_parts(out.as_ptr().cast::<D3D12_RESOURCE_BARRIER>(), out.len()) };
            // SAFETY: an open list and live resources.
            unsafe { self.list.ResourceBarrier(barriers) };
        }
        PREP_STATS[P::BarrierNs as usize].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    /// A use of title texture `resource` about to be recorded, if it is a
    /// depth texture with a colour twin (false if it is not): of the twin
    /// (`colour`: unordered access) or of the depth texture (depth target,
    /// clear, shader read, copy, resolve), writing it or only reading. The
    /// side used is first brought up to date: by a copy recorded here if
    /// this list wrote the other side since, by the list's prelude if the
    /// list uses the side as it finds it. The twin is left in the
    /// unordered access state; the caller moves the depth texture to the
    /// state its use needs, as for any texture. A twin still ahead when the
    /// list closes is copied then (`twins_to_depth`).
    /// shortcut: every depth target bound for a draw counts as written
    /// (also when the draw only tests), and every unordered access view a
    /// shader names as written; views a shader picks from the heap by a
    /// computed index are not seen here at all.
    pub(crate) fn twin_use(
        &self,
        resource: crate::object::This,
        colour: bool,
        write: bool,
    ) -> bool {
        const UNORDERED_ACCESS: u32 = 0x8;
        // Asked for every texture a draw reads: most have no twin.
        let Some(twin) = crate::resource::twin(resource) else {
            return false;
        };
        let Some(depth) = crate::resource::host_copy(resource) else {
            return false;
        };
        let side = usize::from(colour);
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        let u = twins
            .entry(std::sync::Arc::as_ptr(&twin) as usize)
            .or_insert_with(|| TwinUse {
                depth: depth.clone(),
                twin: twin.clone(),
                needs: [false; 2],
                behind: [None; 2],
                writing: false,
                late: false,
            });
        let copy = match u.behind[side] {
            Some(behind) => behind,
            None => {
                u.needs[side] = true;
                false
            }
        };
        u.behind[side] = Some(false);
        match (write, colour) {
            // Shaders write the twin when the draw or dispatch runs: what
            // the same one reads of the depth texture is prepared before
            // (`twins_written`).
            (true, true) => u.writing = true,
            (true, false) => u.behind[1] = Some(true),
            (false, _) => {}
        }
        // shortcut: the depth plane's twin is not drawn into the plane
        // here: the pass sets a pipeline and a target of ours, which a list
        // still recording the title's commands cannot take. This use sees
        // the plane as it was; the pass runs when the list closes
        // (`twins_to_depth`). Counted.
        if copy && !colour && twin.plane.is_some() {
            u.late = true;
            crate::resource::PLANES[2].fetch_add(1, Ordering::Relaxed);
        }
        drop(twins);
        if copy {
            self.twin_copy(&depth, &twin, colour);
        } else if colour {
            self.settle(&twin.colour, UNORDERED_ACCESS);
        }
        // The depth plane's twin is written through unordered access too.
        if colour && let Some(plane) = &twin.plane {
            self.settle(plane, UNORDERED_ACCESS);
        }
        true
    }

    /// Records the pass that draws the twin of the depth plane of `depth`
    /// into the plane, if `twin` has one, with the moves to the states the
    /// pass needs; the twin is left in the unordered access state. Only
    /// for a list that records nothing of the title's afterwards
    /// (`record_plane_pass`).
    fn pass_plane(
        &self,
        depth: &std::sync::Arc<crate::resource::HostTexture>,
        twin: &crate::resource::Twin,
    ) {
        const UNORDERED_ACCESS: u32 = 0x8;
        const DEPTH_WRITE: u32 = 0x10;
        const PIXEL_SHADER_RESOURCE: u32 = 0x80;
        let Some(plane) = &twin.plane else { return };
        self.settle(depth, DEPTH_WRITE);
        self.settle(plane, PIXEL_SHADER_RESOURCE);
        record_plane_pass(&self.list, &depth.texture, &plane.texture);
        self.settle(plane, UNORDERED_ACCESS);
    }

    /// Records the copy of a depth texture into its colour twin (`colour`)
    /// or of the twin into the depth texture, with the moves to the copy
    /// states. The twin is left in the unordered access state.
    fn twin_copy(
        &self,
        depth: &std::sync::Arc<crate::resource::HostTexture>,
        twin: &crate::resource::Twin,
        colour: bool,
    ) {
        const UNORDERED_ACCESS: u32 = 0x8;
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        let (from, to) = if colour {
            (depth, &twin.colour)
        } else {
            (&twin.colour, depth)
        };
        self.settle(from, COPY_SOURCE);
        self.settle(to, COPY_DEST);
        record_twin_copy(&self.list, &depth.texture, twin, colour);
        self.settle(&twin.colour, UNORDERED_ACCESS);
    }

    /// Before the list closes: the depth textures whose twins it wrote last
    /// are given what was written, and left readable by shaders. Shaders
    /// that pick a view from the heap by a computed index are not seen by
    /// `twin_use` and read the depth texture in lists that never name it
    /// (in a captured frame of the title no table names a shadow
    /// map for reading but the pass that fills the next one).
    fn twins_to_depth(&self) {
        const SHADER_RESOURCE: u32 = 0x40 | 0x80;
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        let written: Vec<_> = twins
            .values_mut()
            .filter(|u| u.behind[0] == Some(true) || u.late)
            .map(|u| {
                // Not copied again if the list copied since the last write
                // and only the depth plane's pass is left.
                let copy = u.behind[0] == Some(true);
                u.behind[0] = Some(false);
                u.late = false;
                (u.depth.clone(), u.twin.clone(), copy)
            })
            .collect();
        drop(twins);
        for (depth, twin, copy) in &written {
            if *copy {
                self.twin_copy(depth, twin, false);
            }
            // Nothing of the title's is recorded after this: the pass may
            // leave its own pipeline and target set.
            self.pass_plane(depth, twin);
            self.settle(depth, SHADER_RESOURCE);
        }
    }

    /// The draw or dispatch just prepared is recorded: the twins it writes
    /// now hold what their depth textures lack.
    pub(crate) fn twins_written(&self) {
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        for u in twins.values_mut().filter(|u| u.writing) {
            u.writing = false;
            u.behind[0] = Some(true);
        }
    }

    /// At submission: the list that moves each texture this list uses from
    /// its real state to the one the list starts with; the textures' real
    /// states become those the list ends with. For depth textures with a
    /// colour twin it first copies the side the list uses as it finds it
    /// from the other, if a list submitted earlier left it behind.
    fn prelude(&self) -> Option<ID3D12CommandList> {
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        let host_state = |state: u32| D3D12_RESOURCE_STATES(state as i32);
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        // The copies (depth texture, twin, whether into the twin); the
        // sides are then as this list leaves them.
        let mut syncs = Vec::new();
        let twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        for u in twins.values() {
            let real = u.twin.behind.load(Ordering::Relaxed);
            let mut behind = [real & 1 != 0, real & 2 != 0];
            for side in [0, 1] {
                if u.needs[side] && behind[side] {
                    syncs.push((u.depth.clone(), u.twin.clone(), side == 1));
                    behind = [false; 2];
                }
            }
            let left = [0, 1].map(|side| u.behind[side].unwrap_or(behind[side]));
            let left = u32::from(left[0]) | (u32::from(left[1]) << 1);
            u.twin.behind.store(left, Ordering::Relaxed);
        }
        drop(twins);
        // A copy runs with its textures in the copy states: each moves
        // there from its real state, and from there to the state the list
        // starts with (back to its real state if the list does not use it).
        let mut to_copy = Vec::new();
        let mut barriers = Vec::new();
        let mut copying = std::collections::HashMap::new();
        for (depth, twin, colour) in &syncs {
            let (from, to) = if *colour {
                (depth, &twin.colour)
            } else {
                (&twin.colour, depth)
            };
            for (texture, state) in [(from, COPY_SOURCE), (to, COPY_DEST)] {
                let key = std::sync::Arc::as_ptr(texture) as usize;
                let real = texture.state.load(Ordering::Relaxed);
                copying.insert(key, state);
                if real != state {
                    to_copy.push(transition(
                        &texture.texture,
                        host_state(real),
                        host_state(state),
                    ));
                    if !states.contains_key(&key) {
                        barriers.push(transition(
                            &texture.texture,
                            host_state(state),
                            host_state(real),
                        ));
                    }
                }
            }
        }
        barriers.extend(states.iter().filter_map(|(key, (first, last, copy, _))| {
            let real = copy.state.swap(*last, Ordering::Relaxed);
            let real = copying.get(key).copied().unwrap_or(real);
            (real != *first)
                .then(|| transition(&copy.texture, host_state(real), host_state(*first)))
        }));
        // The records the list's draws read from slots (`crate::indirect::gather`).
        let gather = self.gather.lock().unwrap_or_else(|e| e.into_inner());
        let gathered = gather.sets.iter().any(|s| s.records() > 0);
        if barriers.is_empty() && syncs.is_empty() && !gathered {
            return None;
        }
        let Some((allocator, list)) = self
            .prelude
            .get_or_init(|| host().and_then(|h| h.make_prelude()))
            .as_ref()
        else {
            fail(format_args!("no host list for a state prelude"));
        };
        // SAFETY: the prelude is not executing (every submission is waited
        // for); the textures are live.
        let recorded = unsafe {
            allocator.Reset().and_then(|()| {
                list.Reset(allocator, None)?;
                if !to_copy.is_empty() {
                    list.ResourceBarrier(&to_copy);
                }
                for (depth, twin, colour) in &syncs {
                    record_twin_copy(list, &depth.texture, twin, *colour);
                }
                if !barriers.is_empty() {
                    list.ResourceBarrier(&barriers);
                }
                if gathered {
                    // The slots must hold clamped records before a draw
                    // reads them: a prelude that cannot record the passes
                    // is no prelude.
                    let filled = match &gather.budget {
                        Some(budget) => crate::indirect::record_gathers(list, &gather.sets, budget),
                        None => false,
                    };
                    if !filled {
                        fail(format_args!("a list's gathered indirect records could not be clamped in its prelude"));
                    }
                }
                list.Close()
            })
        };
        if let Err(e) = recorded {
            fail(format_args!("a host state prelude did not record ({e})"));
        }
        list.cast().ok()
    }

    /// From here on the list gathers no records (`crate::indirect::gather`):
    /// what is about to be recorded may write arguments.
    pub(crate) fn gather_close(&self) {
        self.gather.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
    }

    /// A slot for the record at `offset` of the host copy `src` in the
    /// scratch buffer the list's prelude fills (`crate::indirect::gather`,
    /// which checked the record): the buffer and the slot's place in it.
    pub(crate) fn gather_slot(&self, src: &ID3D12Resource, src_id: u64, offset: u32, (kind, stride): (u32, u32)) -> Option<(ID3D12Resource, u64)> {
        let none = |why: usize| {
            crate::indirect::GATHER_NONE[why].fetch_add(1, Ordering::Relaxed);
            None
        };
        let mut gather = self.gather.lock().unwrap_or_else(|e| e.into_inner());
        if gather.closed {
            return none(1);
        }
        let found = gather
            .sets
            .iter()
            .position(|s| s.src_id == src_id && s.kind == kind && s.stride == stride && s.src.as_raw() == src.as_raw());
        let at = match found {
            Some(at) => at,
            None => {
                if gather.sets.len() >= GATHER_SETS {
                    return none(2);
                }
                // The prelude zeroes the budget as the list zeroes its own.
                if self.list.cast::<ID3D12GraphicsCommandList2>().is_err() {
                    return none(4);
                }
                let Some(host) = host() else {
                    return none(4);
                };
                if gather.budget.is_none() {
                    let Some(budget) = host.uav_buffer(256) else {
                        return none(4);
                    };
                    name(&budget, || "indirect budget of gathered records".to_owned());
                    gather.budget = Some(budget);
                }
                let scratch = match gather.spare.pop() {
                    Some(scratch) => scratch,
                    None => {
                        let Some(scratch) = host.uav_buffer(u64::from(crate::indirect::GATHER_MOST) * u64::from(crate::indirect::GATHER_STRIDE)) else {
                            return none(4);
                        };
                        name(&scratch, || "indirect arguments, gathered".to_owned());
                        scratch
                    }
                };
                let Some(table) = self.scratch(u64::from(crate::indirect::GATHER_MOST) * 4) else {
                    gather.spare.push(scratch);
                    return none(4);
                };
                gather.sets.push(crate::indirect::GatherSet::new(src.clone(), src_id, (kind, stride), scratch, table));
                gather.sets.len() - 1
            }
        };
        let set = &mut gather.sets[at];
        let Some(slot) = set.slot(offset) else {
            return none(3);
        };
        Some((set.scratch.clone(), u64::from(slot) * u64::from(stride)))
    }

    /// Notes what the command about to be recorded does, as a marker
    /// command whose text the list keeps (diagnostics, with device-removed
    /// data on): the report pairs the breadcrumbs' marker commands with the
    /// texts in order and names the commands a fault stopped among.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_op(&self, what: impl FnOnce() -> String) {
        self.note(what, None);
    }

    /// As [`HostList::note_op`], for a copy that reads the host GPU range
    /// `(address, bytes)`: the report places the faulting address in it.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_read(&self, what: impl FnOnce() -> String, range: (u64, u64)) {
        self.note(what, Some(range));
    }

    #[cfg(feature = "diagnostics")]
    fn note(&self, what: impl FnOnce() -> String, range: Option<(u64, u64)>) {
        if !dred_on() {
            crate::capture::op(std::ptr::from_ref(self) as usize, what);
            return;
        }
        {
            let started = std::time::Instant::now();
            let at = self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32;
            if let Some((heap, _)) = self.stamps() && at < STAMPS - 1 {
                // SAFETY: an open list and our query heap.
                unsafe { self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, at) };
            }
            let what = what();
            crate::capture::op(std::ptr::from_ref(self) as usize, || what.clone());
            let text: Vec<u16> = what.encode_utf16().chain([0]).collect();
            const UNICODE: u32 = 0;
            // SAFETY: an open list; the marker is copied when recorded.
            unsafe {
                self.list.SetMarker(UNICODE, Some(text.as_ptr().cast()), 2 * text.len() as u32)
            };
            self.notes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((what, range));
            crate::copystat::add(crate::copystat::C::NoteNs, started.elapsed().as_nanos() as u64);
        }
    }

    /// Whether nothing recorded since the list was opened does any work:
    /// no draw or dispatch (the first keeps its pipeline), no copy, clear,
    /// upload or resolve (they defer a step or move a texture's state), no
    /// bottom-of-pipe write. State set on the list does not count.
    pub(crate) fn untouched(&self) -> bool {
        {
            let deferred = self.deferred();
            if !(deferred.copies.is_empty()
                && deferred.uploads.is_empty()
                && deferred.kept.is_empty()
                && deferred.textures.is_empty()
                && deferred.writes.is_empty()
                && deferred.backs.is_empty())
            {
                return false;
            }
        }
        self.states.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
            && self.twins.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// Notes a pipeline the list binds.
    pub(crate) fn used_pipeline(&self, name: &str) {
        let mut pipelines = self.pipelines.lock().unwrap_or_else(|e| e.into_inner());
        if !pipelines.contains(name) {
            pipelines.insert(name.to_owned());
        }
    }

    /// The list binds a pipeline: it and its root signature are kept until
    /// the list is reset, and its name noted, the first time only (a kept
    /// pipeline's address names no other).
    pub(crate) fn bind_pipeline(&self, name: &str, pso: &ID3D12PipelineState, root: &ID3D12RootSignature) {
        {
            let mut deferred = self.deferred();
            if !deferred.bound.insert(pso.as_raw() as usize) && bind_cache() {
                return;
            }
            deferred.kept.push(pso.clone().into());
            deferred.kept.push(root.clone().into());
        }
        self.used_pipeline(name);
    }

    fn deferred(&self) -> std::sync::MutexGuard<'_, Deferred> {
        self.deferred.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn builder_upload(&self, upload: crate::memory::BuilderUpload) {
        self.deferred().builder_uploads.push(upload);
    }

    /// Numbered raw-address copies already prepared on this list for reads,
    /// writes to every copy, and the diagnostics for omitted writes.
    pub(crate) fn raw_copy_counts(&self, n: usize, write_all: bool, narrowed: bool) -> [usize; 3] {
        let mut deferred = self.deferred();
        let before = deferred.raw_copies;
        deferred.raw_copies[0] = n;
        if write_all { deferred.raw_copies[1] = n; }
        if narrowed { deferred.raw_copies[2] = n; }
        before
    }

    /// Save the exact backing that the builder binds for a size or counter result.
    pub(crate) fn builder_report(&self, shadow: &std::sync::Arc<crate::shadow::Shadow>, at: u64, len: u64) {
        let mut d = self.deferred();
        let after_step = d.copies.len();
        if let Some(previous) = d
            .builder_reports
            .iter()
            .position(|r| r.address == at && std::sync::Arc::ptr_eq(&r.shadow, shadow))
        {
            d.builder_reports.remove(previous);
        }
        d.builder_reports.push(BuilderReport {
            shadow: shadow.clone(),
            address: at,
            len,
            after_step,
        });
    }

    /// A size copied in its producing list is available on the GPU before the CPU can read it.
    pub(crate) fn copy_builder_report(&self, dst: u64, src: u64, len: u64) -> bool {
        static ENHANCED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        self.copy_builder_report_with_barriers(dst, src, len, *ENHANCED.get_or_init(crate::draw::enhanced_barriers))
    }

    fn copy_builder_report_with_barriers(&self, dst: u64, src: u64, len: u64, enhanced: bool) -> bool {
        self.copy_builder_report_to(dst, src, len, enhanced, crate::shadow::find)
    }

    fn copy_builder_report_to(
        &self, dst: u64, src: u64, len: u64, enhanced: bool,
        destination: impl FnOnce(u64, u64) -> Option<std::sync::Arc<crate::shadow::Shadow>>,
    ) -> bool {
        if len == 0 {
            return false;
        }
        let from = {
            let d = self.deferred();
            let Some(report) = d
                .builder_reports
                .iter()
                .rev()
                .find(|r| r.address <= src && src.saturating_add(len) <= r.address.saturating_add(r.len))
            else {
                return false;
            };
            let overlaps =
                |at: usize, n: usize| at < (src + len) as usize && (src as usize) < at.saturating_add(n);
            if d.copies[report.after_step..].iter().any(|s| match s {
                Step::Copy(at, _, n) | Step::Run(_, Some((at, _, n))) => overlaps(*at, *n),
                Step::Run(_, None) => true,
                _ => false,
            }) {
                return false;
            }
            report.shadow.clone()
        };
        self.flush_barriers();
        let Some(to) = destination(dst, len) else {
            self.read_back_builder_report(dst, &from.buffer, from.offset(src), len);
            return true;
        };
        // The destination's title bytes first; then its range is the card's.
        to.refresh(self, dst, len);
        to.gpu_writes(self, dst, len);
        let src_offset = from.offset(src);
        let dst_offset = to.offset(dst);
        let alias = from.buffer == to.buffer;
        let uav = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        let common = D3D12_RESOURCE_STATE_COMMON;
        if enhanced && alias && src_offset.abs_diff(dst_offset) >= len
            && let Ok(l7) = self.list.cast::<ID3D12GraphicsCommandList7>()
        {
            self.copy_report_disjoint(&l7, &from.buffer, src_offset, dst_offset, len);
        } else if alias {
            // Legacy copy states cover the whole buffer, even for disjoint ranges.
            let scratch = host().and_then(|h| h.uav_buffer(len))
                .unwrap_or_else(|| fail(format_args!("no staging buffer for a {len}-byte builder report")));
            self.copy_report_legacy((&from.buffer, src_offset, uav), (&scratch, 0, common), len);
            self.copy_report_legacy((&scratch, 0, common), (&to.buffer, dst_offset, uav), len);
            self.keep(&scratch);
        } else {
            // A different destination can itself be the output of an earlier builder dispatch.
            // State belongs to the resource, even when the two report ranges are disjoint.
            let dst_state = if self.deferred().builder_reports.iter().any(|r| r.shadow.buffer == to.buffer) {
                uav
            } else {
                common
            };
            self.copy_report_legacy((&from.buffer, src_offset, uav), (&to.buffer, dst_offset, dst_state), len);
        }
        self.keep(&to.buffer);
        self.keep(&from.buffer);
        self.count_write();
        #[cfg(feature = "diagnostics")]
        self.note_op(|| format!("copy builder report {src:#x} to {dst:#x} ({len} bytes)"));
        true
    }

    fn copy_report_legacy(
        &self,
        (from, src, from_state): (&ID3D12Resource, u64, D3D12_RESOURCE_STATES),
        (to, dst, to_state): (&ID3D12Resource, u64, D3D12_RESOURCE_STATES),
        len: u64,
    ) {
        let read = D3D12_RESOURCE_STATE_COPY_SOURCE;
        let write = D3D12_RESOURCE_STATE_COPY_DEST;
        // A dispatch promotes its report to UAV until the submission ends. Restore each
        // buffer's incoming state so another copy or dispatch sees that same state.
        // SAFETY: the resources are distinct, live, and contain the two ranges.
        unsafe {
            self.list.ResourceBarrier(&[transition(from, from_state, read), transition(to, to_state, write)]);
            self.list.CopyBufferRegion(to, dst, from, src, len);
            self.list.ResourceBarrier(&[transition(to, write, to_state), transition(from, read, from_state)]);
        }
    }

    /// A title destination without a GPU shadow still receives the report at settlement.
    fn read_back_builder_report(&self, dst: u64, from: &ID3D12Resource, offset: u64, len: u64) {
        let buffer = host().and_then(|h| h.read_back_buffer(len))
            .unwrap_or_else(|| fail(format_args!("out of readback memory for a {len}-byte builder report")));
        let uav = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        let read = D3D12_RESOURCE_STATE_COPY_SOURCE;
        // SAFETY: the producer owns this range. Readback heaps stay in COPY_DEST.
        unsafe {
            self.list.ResourceBarrier(&[transition(from, uav, read)]);
            self.list.CopyBufferRegion(&buffer, 0, from, offset, len);
            self.list.ResourceBarrier(&[transition(from, read, uav)]);
        }
        self.keep(from);
        self.defer_read_back(buffer, dst as *mut u8, [len as usize, 1, len as usize]);
        self.count_write();
    }

    /// A self-copy between disjoint ranges is legal with enhanced barriers. Cross the
    /// legacy/enhanced boundary in COMMON on both sides of the copy.
    fn copy_report_disjoint(
        &self, list: &ID3D12GraphicsCommandList7, buffer: &ID3D12Resource,
        src: u64, dst: u64, len: u64,
    ) {
        let common = D3D12_RESOURCE_STATE_COMMON;
        let uav = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        let access = D3D12_BARRIER_ACCESS_COPY_SOURCE | D3D12_BARRIER_ACCESS_COPY_DEST;
        let barrier = |before, after| D3D12_BUFFER_BARRIER {
            SyncBefore: D3D12_BARRIER_SYNC_ALL,
            SyncAfter: D3D12_BARRIER_SYNC_ALL,
            AccessBefore: before,
            AccessAfter: after,
            // SAFETY: borrowed for the Barrier call. The list retains the buffer.
            pResource: unsafe { std::mem::transmute_copy(buffer) },
            Offset: 0,
            Size: u64::MAX,
        };
        let apply = |b: D3D12_BUFFER_BARRIER| unsafe {
            list.Barrier(&[D3D12_BARRIER_GROUP {
                Type: D3D12_BARRIER_TYPE_BUFFER,
                NumBarriers: 1,
                Anonymous: D3D12_BARRIER_GROUP_0 { pBufferBarriers: &b },
            }]);
        };
        // SAFETY: the producer owns the buffer in UAV state and the ranges do not intersect.
        unsafe {
            self.list.ResourceBarrier(&[transition(buffer, uav, common)]);
            apply(barrier(D3D12_BARRIER_ACCESS_COMMON, access));
            self.list.CopyBufferRegion(buffer, dst, buffer, src, len);
            apply(barrier(access, D3D12_BARRIER_ACCESS_COMMON));
            self.list.ResourceBarrier(&[transition(buffer, common, uav)]);
        }
    }

    /// Copies `len` bytes from `src` to `dst` when the list is submitted.
    pub(crate) fn defer_copy(&self, dst: *mut u8, src: *const u8, len: usize) {
        self.count_write();
        let mut deferred = self.deferred();
        COPIES_JOINED[0].fetch_add(1, Ordering::Relaxed);
        // A copy that goes on where the one recorded just before it ended,
        // source and destination both: with `join_copies` the two are one
        // step.
        if let Some(Step::Copy(d, s, n)) = deferred.copies.last_mut()
            && continues((*d, *s, *n), (dst as usize, src as usize, len))
        {
            COPIES_JOINED[1].fetch_add(1, Ordering::Relaxed);
            if join_copies() {
                *n += len;
                return;
            }
        }
        deferred
            .copies
            .push(Step::Copy(dst as usize, src as usize, len));
        // (A set's clear writes all its control bytes, empty or not.)
        if !prep_reuse() || !deferred.refreshed.is_empty() {
            deferred.refreshed.clear();
        }
        if !deferred.gpu_written.is_empty() {
            deferred.gpu_written.clear();
        }
        if !deferred.filled.is_empty() {
            deferred.filled.clear();
        }
    }

    /// Copies `len` bytes of title memory at `src` into the list's upload
    /// memory at `dst` when the list is submitted.
    /// The host address of `len` bytes of scratch memory that hold the title
    /// memory at `src` as of the list's submission: with
    /// `VICTORY_BIND_CACHE=1` one fill for every ask until the list next
    /// writes title memory.
    pub(crate) fn fill_scratch(&self, src: *const u8, len: u64, fill: u64) -> Option<u64> {
        let cache = bind_cache();
        let key = (src as usize, fill as usize);
        if cache {
            let known = self.deferred().filled.get(&key).copied();
            if known.is_some() {
                return known;
            }
        }
        let (gpu, to) = self.scratch(len)?;
        if fill > 0 {
            self.defer_fill(to, src, fill as usize);
        }
        if cache {
            self.deferred().filled.insert(key, gpu);
        }
        Some(gpu)
    }

    pub(crate) fn defer_fill(&self, dst: *mut u8, src: *const u8, len: usize) {
        self.deferred()
            .copies
            .push(Step::Fill(dst as usize, src as usize, len));
    }

    /// Brings `[address, address + len)` of a buffer's host copy up to date
    /// when the list is submitted (once per range until a title copy
    /// intervenes), keeping the copy alive until the list is reset.
    pub(crate) fn defer_refresh(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        let mut deferred = self.deferred();
        let key = (std::sync::Arc::as_ptr(shadow) as usize, address, len);
        if deferred.refreshed.insert(key) {
            deferred
                .copies
                .push(Step::Refresh(shadow.clone(), address, len));
        }
    }

    /// Shaders of the list write `[address, address + len)` of a buffer's
    /// host copy: copied back into title memory once the list has run.
    pub(crate) fn defer_gpu_write(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        let mut deferred = self.deferred();
        if deferred.quarantined {
            return;
        }
        if gpu_write_once() {
            let key = (std::sync::Arc::as_ptr(shadow) as usize, address, len);
            if !deferred.gpu_written.insert(key) {
                return;
            }
        }
        deferred.copies.push(Step::GpuWrite(shadow.clone(), address, len));
    }

    /// The list records a quarantined stage of the structure builder: no
    /// shader write of the whole list is copied back into title memory,
    /// those declared before it included (fx #935, `Deferred::quarantined`).
    pub(crate) fn quarantine(&self) {
        let mut deferred = self.deferred();
        if !deferred.quarantined {
            deferred.quarantined = true;
            deferred.copies.retain(|step| !matches!(step, Step::GpuWrite(..)));
        }
    }

    /// A `VICTORY_NARROW_*` switch kept `[address, address + len)` of a
    /// buffer's host copy from being declared written (`kind`): counted for
    /// the frame line only.
    pub(crate) fn note_narrowed(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
        kind: crate::narrow::Cut,
    ) {
        if len == 0 {
            return;
        }
        crate::narrow::cut(kind, len as u64);
        self.deferred()
            .narrowed
            .push((std::sync::Arc::as_ptr(shadow) as usize, address, address.saturating_add(len)));
    }

    /// A read back: once the list has run, `rows` rows of `row` bytes go
    /// from `buffer` (rows 256-byte pitched, see `PlacedFootprint::host_rows`)
    /// to title memory at `dst`, pitched `pitch`. The buffer lives until
    /// the list is reset.
    pub(crate) fn defer_read_back(&self, buffer: ID3D12Resource, dst: *mut u8, [row, rows, pitch]: [usize; 3]) {
        self.deferred().backs.push((buffer, dst as usize, [row, rows, pitch]));
    }

    /// Stores `what` (`bytes` of it) at `dst` once the list has run.
    pub(crate) fn defer_write(&self, dst: *mut u8, what: crate::queue::Written, bytes: usize) {
        self.deferred().writes.push((dst as usize, what, bytes));
    }

    /// A mapped upload buffer of `size` bytes, alive until the list is
    /// reset.
    ///
    /// shortcut: one buffer per copy; pool them once uploads are frequent.
    pub(crate) fn upload(&self, host: &Host, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let (buffer, data) = host.upload_buffer(size)?;
        name(&buffer, || format!("list upload {size:#x}"));
        self.deferred().hold_upload(buffer.clone(), size);
        Some((buffer, data))
    }

    /// `len` bytes of upload memory (256-byte aligned) until the list is
    /// reset: the host GPU address and the CPU pointer.
    pub(crate) fn scratch(&self, len: u64) -> Option<(u64, *mut u8)> {
        let host = host()?;
        let mut s = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        let len = len.max(1).next_multiple_of(256);
        if len > SCRATCH_CHUNK {
            let (buffer, data) = host.upload_buffer(len)?;
            name(&buffer, || format!("list scratch {len:#x}"));
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            self.deferred().hold_upload(buffer, len);
            return Some((gpu, data));
        }
        // The chunk in use while `len` fits in what is left of it, else the
        // next one (a list's first chunk may be a small one).
        while s.current < s.chunks.len() && s.used + len > s.chunks[s.current].1 {
            (s.current, s.used) = (s.current + 1, 0);
        }
        if s.current == s.chunks.len() {
            let size = if scratch_pool() && s.chunks.is_empty() && len <= SCRATCH_FIRST {
                SCRATCH_FIRST
            } else {
                SCRATCH_CHUNK
            };
            let chunk = take_chunk(host, size)?;
            s.chunks.push((chunk, size));
            s.used = 0;
        }
        let (data, gpu) = (s.chunks[s.current].0.data, s.chunks[s.current].0.gpu);
        let at = s.used;
        s.used += len;
        Some((gpu + at, (data + at as usize) as *mut u8))
    }

    /// The bytes of scratch chunks the list holds now (after a reset:
    /// none while lists share their chunks).
    pub(crate) fn scratch_kept(&self) -> u64 {
        let scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        scratch.chunks.iter().map(|c| c.1).sum()
    }

    /// The host address of the list's indirect-work budget, zeroed at each
    /// submission of the list from the first use after a reset on.
    pub(crate) fn budget(&self) -> Option<u64> {
        let (buffer, data) = self
            .budget
            .get_or_init(|| {
                let (b, d) = host()?.shared_buffer(256)?;
                Some((b, d as usize))
            })
            .as_ref()?;
        let card = self.card_budget.get_or_init(|| self.make_card_budget()).as_ref();
        let mut deferred = self.deferred();
        if !deferred.budget_zeroed {
            deferred.budget_zeroed = true;
            let at = *data;
            let key = std::ptr::from_ref(self) as usize;
            // SAFETY: the list's own mapped words, written before it runs.
            // The words after the budget hold what the clamp noted about the
            // list's last run (mesh dispatches: the largest group count
            // asked for, the groups asked in all; with the card's copy, as
            // the list's end copied them here): taken, then zeroed.
            deferred.copies.push(Step::Host(Box::new(move || unsafe {
                let words = at as *mut u32;
                // (fx #793: read only by diagnostics builds)
                #[cfg(feature = "diagnostics")]
                {
                    let (largest, all) = (words.add(1).read_volatile(), words.add(2).read_volatile());
                    MESH_GROUPS[0].fetch_max(u64::from(largest), Ordering::Relaxed);
                    MESH_GROUPS[1].fetch_add(u64::from(all), Ordering::Relaxed);
                }
                // Word 5: records the indirect guard emptied in that run.
                let bad = words.add(5).read_volatile();
                if bad != 0 {
                    indirect_guard_report(key, bad);
                }
                // Words 6 and 7: the records refused for want of budget,
                // the largest cost.
                let (refused, biggest) = (words.add(6).read_volatile(), words.add(7).read_volatile());
                if refused != 0 {
                    BUDGET_NOTES[0].fetch_add(u64::from(refused), Ordering::Relaxed);
                }
                BUDGET_NOTES[1].fetch_max(u64::from(biggest), Ordering::Relaxed);
                for k in 0..8 {
                    words.add(k).write_volatile(0);
                }
            })));
            drop(deferred);
            if let Some(card) = card {
                self.zero_card_budget(card);
            }
        }
        // SAFETY: a live buffer.
        Some(unsafe { card.unwrap_or(buffer).GetGPUVirtualAddress() })
    }

    /// The budget's words in the card's own memory ([`card_budget`]), if
    /// the list can zero them itself (`WriteBufferImmediate`).
    fn make_card_budget(&self) -> Option<ID3D12Resource> {
        if !card_budget() {
            return None;
        }
        self.list.cast::<ID3D12GraphicsCommandList2>().ok()?;
        let buffer = host()?.uav_buffer(256)?;
        name(&buffer, || "indirect budget".to_owned());
        Some(buffer)
    }

    /// Records, before the list's first clamp, the zeroing of the card's
    /// copy of the budget: common, written as a copy, then left for
    /// unordered access until the list closes ([`HostList::card_budget_out`]).
    fn zero_card_budget(&self, card: &ID3D12Resource) {
        let Ok(list2) = self.list.cast::<ID3D12GraphicsCommandList2>() else {
            return;
        };
        // SAFETY: a live buffer.
        let at = unsafe { card.GetGPUVirtualAddress() };
        let zeros = [0u64, 4, 8, 20, 24, 28].map(|offset| D3D12_WRITEBUFFERIMMEDIATE_PARAMETER {
            Dest: at + offset,
            Value: 0,
        });
        // SAFETY: an open list; the buffer is the list's own and in the
        // common state at the start of every recording.
        unsafe {
            self.list.ResourceBarrier(&[transition(
                card,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )]);
            list2.WriteBufferImmediate(zeros.len() as u32, zeros.as_ptr(), None);
            self.list.ResourceBarrier(&[transition(
                card,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )]);
        }
    }

    /// Before the list closes, if this recording charged the card's copy
    /// of the budget: its notes on mesh records go to the mapped words
    /// (read at the next submission), and it goes back to common.
    fn card_budget_out(&self) {
        if !self.deferred().budget_zeroed {
            return;
        }
        let (Some(Some(card)), Some(Some((mapped, _)))) =
            (self.card_budget.get(), self.budget.get())
        else {
            return;
        };
        // SAFETY: an open list; both buffers are the list's own. The
        // mapped one is promoted from common to the copy destination.
        unsafe {
            self.list.ResourceBarrier(&[transition(
                card,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            self.list.CopyBufferRegion(mapped, 4, card, 4, 28);
            self.list.ResourceBarrier(&[transition(
                card,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            )]);
        }
    }

    /// A device-local unordered-access buffer of at least `size` bytes for
    /// clamped indirect arguments, reused across executions and resets of the
    /// list. Smaller buffers replaced mid-list stay alive until
    /// [`HostList::reset`].
    pub(crate) fn indirect_scratch(&self, size: u64) -> Option<ID3D12Resource> {
        let needed = size.max(64 << 10);
        let mut slot = self.indirect_args.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((buffer, cap)) = &*slot
            && *cap >= needed
        {
            return Some(buffer.clone());
        }
        let prev = slot.as_ref().map_or(0, |(_, cap)| *cap);
        let alloc = prev
            .saturating_mul(2)
            .max(needed)
            .next_multiple_of(64 << 10);
        let buffer = host()?.uav_buffer(alloc)?;
        name(&buffer, || format!("indirect arguments {alloc:#x}"));
        let old = slot.replace((buffer.clone(), alloc));
        drop(slot);
        if let Some((old_buf, _)) = old {
            // It ends the list common, as every scratch buffer does.
            self.indirect_common(&old_buf);
            self.keep(&old_buf);
        }
        Some(buffer)
    }

    /// Moves `scratch` back to the common state if a clamp left it readable
    /// as indirect arguments ([`HostList::indirect_readable`]): where the
    /// list's first clamp expects it. Done before the list closes and before
    /// a bigger scratch buffer replaces it: one barrier per list (where
    /// there was one per indirect execution), and the list ends as it
    /// begins however it is submitted.
    fn indirect_common(&self, scratch: &ID3D12Resource) {
        {
            let mut deferred = self.deferred();
            if deferred.indirect_readable != scratch.as_raw() as usize {
                return;
            }
            deferred.indirect_readable = 0;
        }
        let barrier = transition(
            scratch,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            D3D12_RESOURCE_STATE_COMMON,
        );
        // SAFETY: an open list and a live buffer this recording left in
        // `INDIRECT_ARGUMENT`.
        unsafe { self.list.ResourceBarrier(&[barrier]) };
    }

    /// The state the list's indirect scratch buffer `scratch` is in at this
    /// point of the recording: indirect arguments if a clamp in this
    /// recording left it so ([`HostList::indirect_readable`]), otherwise
    /// common: the state a buffer is made in, and the one a recording
    /// leaves it in when it closes or replaces it ([`HostList::indirect_common`]).
    pub(crate) fn indirect_state(&self, scratch: &ID3D12Resource) -> D3D12_RESOURCE_STATES {
        if self.deferred().indirect_readable == scratch.as_raw() as usize {
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT
        } else {
            D3D12_RESOURCE_STATE_COMMON
        }
    }

    /// A clamp recorded in this list left `scratch` readable as indirect
    /// arguments. It stays so for every execution that reads it, until the
    /// next clamp moves it back to unordered access.
    pub(crate) fn indirect_readable(&self, scratch: &ID3D12Resource) {
        self.deferred().indirect_readable = scratch.as_raw() as usize;
    }

    /// Whether the scratch buffer `buffer` was left in `INDIRECT_ARGUMENT`
    /// by this list (and is no longer, as the caller is about to record the
    /// transition back): see `crate::indirect::keep_args`.
    pub(crate) fn take_indirect_kept(&self, buffer: &ID3D12Resource) -> bool {
        self.deferred().indirect_kept.remove(&(buffer.as_raw() as usize))
    }

    /// Whether this list left `buffer` in `INDIRECT_ARGUMENT` (tests).
    #[cfg(test)]
    pub(crate) fn indirect_kept(&self, buffer: &ID3D12Resource) -> bool {
        self.deferred().indirect_kept.contains(&(buffer.as_raw() as usize))
    }

    /// The scratch buffer `buffer` stays in `INDIRECT_ARGUMENT` after an
    /// execution read it.
    pub(crate) fn set_indirect_kept(&self, buffer: &ID3D12Resource) {
        self.deferred().indirect_kept.insert(buffer.as_raw() as usize);
    }

    /// Counts a dispatch or a copy: what may write indirect arguments.
    pub(crate) fn count_write(&self) {
        self.writes.fetch_add(1, Ordering::Relaxed);
    }

    /// Dispatches and copies since the reset.
    pub(crate) fn writes(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }

    /// Room for `bytes` of clamped indirect arguments in this list's
    /// buffers, writable by shaders from here on in the list: the buffer and
    /// the offset. None for more than a buffer holds.
    pub(crate) fn clamp_room(&self, bytes: u64) -> Option<(ID3D12Resource, u64)> {
        const CHUNK: u64 = 256 << 10;
        let bytes = bytes.next_multiple_of(16);
        if bytes > CHUNK {
            return None;
        }
        let mut chunks = self.clamp_chunks.lock().unwrap_or_else(|e| e.into_inner());
        if chunks.used > 0 && chunks.at + bytes <= CHUNK {
            let at = chunks.at;
            chunks.at += bytes;
            return Some((chunks.all[chunks.used - 1].0.clone(), at));
        }
        // The next buffer, writable.
        if chunks.used == chunks.all.len() {
            let buffer = host()?.uav_buffer(CHUNK)?;
            name(&buffer, || "hoisted indirect arguments".to_string());
            chunks.all.push((buffer, D3D12_RESOURCE_STATE_COMMON));
        }
        let i = chunks.used;
        chunks.used += 1;
        chunks.at = bytes;
        let mut barriers = Vec::new();
        if !chunks.barrier {
            // What earlier lists' shaders wrote first.
            chunks.barrier = true;
            barriers.push(D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                ..Default::default()
            });
        }
        let (buffer, state) = &mut chunks.all[i];
        if *state != D3D12_RESOURCE_STATE_UNORDERED_ACCESS {
            barriers.push(transition(buffer, *state, D3D12_RESOURCE_STATE_UNORDERED_ACCESS));
            *state = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        }
        let buffer = buffer.clone();
        if !barriers.is_empty() {
            // SAFETY: an open list and live buffers.
            unsafe { self.list.ResourceBarrier(&barriers) };
        }
        Some((buffer, 0))
    }

    /// The clamped arguments written so far become readable as indirect
    /// arguments by the lists after this one.
    pub(crate) fn clamp_end(&self) {
        let mut chunks = self.clamp_chunks.lock().unwrap_or_else(|e| e.into_inner());
        let used = chunks.used;
        let barriers: Vec<_> = chunks.all[..used]
            .iter_mut()
            .filter(|(_, state)| *state == D3D12_RESOURCE_STATE_UNORDERED_ACCESS)
            .map(|(buffer, state)| {
                *state = D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT;
                transition(
                    buffer,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                )
            })
            .collect();
        if !barriers.is_empty() {
            // SAFETY: an open list and live buffers.
            unsafe { self.list.ResourceBarrier(&barriers) };
        }
    }

    /// Keeps `object` alive until the list is reset.
    pub(crate) fn keep(&self, object: &impl Interface) {
        if let Ok(object) = object.cast::<windows::core::IUnknown>() {
            self.deferred().kept.push(object);
        }
    }

    /// Marvisak #2341: whether `placed`'s alias copy in this list was made
    /// from `source`'s contents as they are now (no use of the source in a
    /// writing state since). Without a copy, or with the source unknown
    /// to the list: no.
    pub(crate) fn alias_is_current(&self, placed: usize, source: crate::object::This) -> bool {
        let Some(copy) = crate::resource::host_copy(source) else {
            return false;
        };
        let key = std::sync::Arc::as_ptr(&copy) as usize;
        let version = self.states.lock().unwrap_or_else(|e| e.into_inner()).get(&key).map(|s| s.3);
        version.is_some_and(|v| self.aliases.lock().unwrap_or_else(|e| e.into_inner()).get(&placed) == Some(&(key, v)))
    }

    /// Marvisak #2341: `placed`'s alias copy was just recorded from
    /// `source`'s contents as they are now.
    pub(crate) fn alias_copied(&self, placed: usize, source: crate::object::This) {
        let Some(copy) = crate::resource::host_copy(source) else {
            return;
        };
        let key = std::sync::Arc::as_ptr(&copy) as usize;
        let version = self.states.lock().unwrap_or_else(|e| e.into_inner()).get(&key).map(|s| s.3);
        let mut aliases = self.aliases.lock().unwrap_or_else(|e| e.into_inner());
        match version {
            Some(v) => aliases.insert(placed, (key, v)),
            None => aliases.remove(&placed),
        };
    }

    /// Whether this is the first time since the reset that the list asks
    /// about `key` (a texture to bring up to date).
    pub(crate) fn first_use(&self, key: usize) -> bool {
        self.deferred().textures.insert(key)
    }

    /// Runs `work` when the list is submitted, in order with its copies.
    /// `work` may write title memory (refreshes after it copy again).
    pub(crate) fn defer(&self, work: impl Fn() + Send + 'static) {
        self.count_write();
        if prep_reuse() {
            let mut deferred = self.deferred();
            deferred.copies.push(Step::Run(Box::new(work), None));
            if !deferred.refreshed.is_empty() {
                deferred.refreshed.clear();
            }
            if !deferred.gpu_written.is_empty() {
                deferred.gpu_written.clear();
            }
            if !deferred.filled.is_empty() {
                deferred.filled.clear();
            }
            return;
        }
        self.deferred().copies.push(Step::Run(Box::new(work), None));
        self.deferred().refreshed.clear();
        self.deferred().gpu_written.clear();
        self.deferred().filled.clear();
    }

    /// As [`HostList::defer`], for work that reads title memory only at
    /// `[src, src + len)` and writes it only at `[dst, dst + len)` (the
    /// title's memory-to-memory copies): with
    /// [`crate::memcopy::known_writes`] the submission's write-watch
    /// asking learns that and stands.
    pub(crate) fn defer_known(&self, dst: usize, src: usize, len: usize, work: impl Fn() + Send + 'static) {
        self.count_write();
        if prep_reuse() {
            let mut deferred = self.deferred();
            deferred.copies.push(Step::Run(Box::new(work), Some((dst, src, len))));
            if !deferred.refreshed.is_empty() {
                deferred.refreshed.clear();
            }
            if !deferred.gpu_written.is_empty() {
                deferred.gpu_written.clear();
            }
            if !deferred.filled.is_empty() {
                deferred.filled.clear();
            }
            return;
        }
        self.deferred().copies.push(Step::Run(Box::new(work), Some((dst, src, len))));
        self.deferred().refreshed.clear();
        self.deferred().gpu_written.clear();
        self.deferred().filled.clear();
    }

    /// As [`HostList::defer`], for work that writes only host memory.
    pub(crate) fn defer_host(&self, work: impl Fn() + Send + 'static) {
        self.deferred().copies.push(Step::Host(Box::new(work)));
    }

    /// Brings a texture's staging buffer up to date when the list is
    /// submitted, keeping it alive until the list is reset.
    pub(crate) fn defer_texture(&self, staging: std::sync::Arc<crate::texture::Staging>) {
        self.deferred().copies.push(Step::Texture(staging));
    }

    /// Runs the list's submission-time steps; refreshes go into `batch`
    /// (the title's big buffers, a 174 MB one among them, are read by most
    /// lists of a submission, through many ranges).
    fn run_deferred(&self, batch: &mut Batch) {
        #[cfg(feature = "diagnostics")]
        use crate::copystat::{C, add};
        for step in &self.deferred().copies {
            let started = std::time::Instant::now();
            let copied = match step {
                &Step::Copy(dst, src, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Copies, 1);
                        add(C::CopyBytes, len as u64);
                    }
                    if prep_copy_flush() {
                        batch.title_written_at(dst, len);
                    } else {
                        if !batch.refreshes.is_empty() {
                            PREP_STATS[P::CopyFlushes as usize].fetch_add(1, Ordering::Relaxed);
                        }
                        batch.title_written();
                    }
                    // `VICTORY_COPY_BACK_ONLY_READBACK`: shader results that
                    // stayed in a host copy go into the source first (a
                    // result copied into a read-back buffer was copied
                    // stale), and the destination's are the title's now.
                    crate::shadow::own_read(src, len, 0);
                    crate::shadow::own_write(dst, len);
                    #[cfg(feature = "diagnostics")]
                    fill_check::copy_runs(dst, src, len);
                    let moving = std::time::Instant::now();
                    if !(prep_copy_direct() && copy_direct(dst, src, len)) {
                        copy_mapped(dst as *mut u8, src as *const u8, len, true);
                    }
                    PREP_STATS[P::Copies as usize].fetch_add(1, Ordering::Relaxed);
                    PREP_STATS[P::CopyUs as usize].fetch_add(moving.elapsed().as_micros() as u64, Ordering::Relaxed);
                    // With the submission's one asking: the journal learns it.
                    crate::watch::wrote(dst, len);
                    len
                }
                &Step::Fill(dst, src, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Copies, 1);
                        add(C::CopyBytes, len as u64);
                        fill_check::runs(src, len);
                    }
                    crate::shadow::own_read(src, len, 0);
                    copy_mapped(dst as *mut u8, src as *const u8, len, false);
                    len
                }
                Step::Refresh(shadow, address, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Refreshes, 1);
                        add(C::RefreshBytes, *len as u64);
                    }
                    // Another copy's shader results over these bytes go
                    // into title memory first (this copy's own stay).
                    crate::shadow::own_read(*address, *len, shadow.id());
                    batch.add(shadow, *address, *len);
                    continue;
                }
                Step::Run(work, _) | Step::Host(work) => {
                    if matches!(step, Step::Run(..)) {
                        batch.title_written();
                    } else if !batch.textures_due.is_empty() {
                        // A host step may read what a queued texture check
                        // writes (levels on demand map the levels `detile`
                        // has just held): the queue goes first, in the order
                        // the steps were recorded, as without
                        // VICTORY_WIDE_TEXTURES (fx #768, grosterr's fix).
                        batch.refresh_textures();
                    }
                    // Copies compared with title memory after their flight
                    // (`VICTORY_MEM2_GPU_ONLY_NOSNAP`): a write whose place is
                    // known keeps the bytes from before it; one whose place is
                    // not gives every such copy its snapshot now, before any
                    // shader ran.
                    match step {
                        &Step::Run(_, Some((dst, src, len))) => {
                            crate::shadow::own_read(src, len, 0);
                            crate::shadow::title_will_change(dst, len);
                        }
                        Step::Run(_, None) => {
                            crate::shadow::own_read_all();
                            crate::shadow::nosnap_unknown_write();
                        }
                        _ => {}
                    }
                    work();
                    match step {
                        // Where it wrote is known: the journal learns it and
                        // the submission's one asking stands.
                        &Step::Run(_, Some((dst, _, len))) if crate::memcopy::known_writes() => {
                            crate::watch::wrote(dst, len);
                            crate::memcopy::add(crate::memcopy::M::RunsKept, 1);
                        }
                        // It wrote title memory, where is not known here: the
                        // submission's one asking ends, later refreshes ask anew.
                        Step::Run(..) => {
                            crate::watch::ask_once_end();
                            crate::memcopy::add(crate::memcopy::M::RunsEnded, 1);
                        }
                        _ => {}
                    }
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Runs, 1);
                        add(C::RunUs, started.elapsed().as_micros() as u64);
                    }
                    continue;
                }
                Step::GpuWrite(shadow, address, len) => {
                    // The shaders start from the title's data: this copy's
                    // pending refresh first. The other copies' wait for
                    // the next flush of all (flushing all of them at every
                    // range a dispatch writes made thousands of write-watch
                    // askings a frame).
                    batch.flush_one(shadow);
                    #[cfg(feature = "diagnostics")]
                    let snapshot = std::time::Instant::now();
                    shadow.writing(*address, *len);
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::GpuWrites, 1);
                        add(C::SnapshotUs, snapshot.elapsed().as_micros() as u64);
                    }
                    continue;
                }
                Step::Texture(staging) => {
                    if batch.texture_due(staging) {
                        // A texture over memory shaders wrote through a
                        // buffer: their results that stayed in the buffer's
                        // copy go into title memory first.
                        let (at, len) = staging.memory();
                        crate::shadow::own_read(at, len, 0);
                        if crate::memcopy::texture_overlap() {
                            // Beside this thread's refreshes of buffer
                            // copies; waited for before title memory is
                            // written and before the lists run.
                            let staging = staging.clone();
                            batch.textures_beside.spawn(move || {
                                let started = std::time::Instant::now();
                                staging.refresh();
                                crate::memcopy::add(crate::memcopy::M::Overlapped, 1);
                                crate::memcopy::add(crate::memcopy::M::OverlapUs, started.elapsed().as_micros() as u64);
                            });
                        } else if wide_textures() {
                            batch.textures_due.push(staging.clone());
                        } else {
                            staging.refresh();
                        }
                    }
                    continue;
                }
            };
            preparing(0, started.elapsed().as_micros() as u64);
            preparing(1, copied as u64);
        }
    }
}

/// Whether a submission's refreshes of host copies run on several threads,
/// a copy to a thread (`VICTORY_WIDE_REFRESH=1`; off by default).
///
/// A submission brings some sixty copies up to date, one after the other
/// on the submitting thread: for each, the pages the title wrote are asked
/// for and read through the kernel into the copy. That is the frame line's
/// "copies", 45-50 ms a frame, and the copies have nothing to do with one
/// another: each refresh writes its own copy and its own notes, and reads
/// title memory. With the switch the copies of a flush (four or more) are
/// spread over the threads kept for the snapshots (`crate::shadow::wide`).
/// The write-watch journal and the guards are behind their own locks, as
/// they are for the title's threads.
fn wide_refresh() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WIDE_REFRESH").is_some_and(|v| v == "1"))
}


/// A submission's refreshes not done yet, merged per buffer copy into the
/// union of the requested ranges: done before a step that writes title
/// memory or gives a range to shaders, and before the lists run.
///
/// Also the textures brought up to date in the submission so far: most
/// lists of a world submission sample the same textures, and a texture in
/// physical-page memory is hashed whole each time it is checked. Checked
/// once, it is checked again only after a step that writes title memory.
#[derive(Default)]
struct Batch {
    refreshes: Vec<ShadowRefresh>,
    /// Where each copy (by address) is in `refreshes`: the list that makes
    /// the heap's buffers ready adds some 4,100 copies, and searching them
    /// for every refresh of a submission was tens of millions of
    /// comparisons a frame.
    index: std::collections::HashMap<usize, usize>,
    /// Staging buffers (by address) refreshed since the last title write.
    textures: std::collections::HashSet<usize>,
    /// Their refreshes under way on other threads
    /// ([`crate::memcopy::texture_overlap`]).
    textures_beside: crate::memcopy::Group,
    /// With [`wide_textures`]: textures due, refreshed together at the next
    /// flush (before a step that writes title memory, before a host step,
    /// and before the lists run), several at a time.
    textures_due: Vec<std::sync::Arc<crate::texture::Staging>>,
    /// `VICTORY_PREP_COPY_FLUSH`: the span of the pending ranges (may be
    /// wider than they are after a copy's refresh is done alone), and how
    /// many were added since all were last done.
    span: (usize, usize),
    ranges: usize,
    /// `VICTORY_COPY_PARTS=1` (Marvisak #2428): the disjoint ranges of
    /// each copy brought up to date since the last title write, by copy
    /// (held, so that its address names no other). A refresh inside one
    /// of them is not asked for again in the submission.
    current: std::collections::HashMap<usize, (std::sync::Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)>,
}

/// `VICTORY_COPY_PARTS=1` (off by default): port of Marvisak #2428
/// (borosf; replaces #2419). A submission keeps, per buffer copy, the
/// disjoint ranges it brought up to date since the last step that wrote
/// title memory, and a refresh that lies inside one is not done again.
/// Copied ranges merge only where they touch (#2428's fix to #2419, which
/// merged them across a page's gap and so skipped pages never copied).
/// The pending side (disjoint ranges merged within [`COPY_GAP`], one 64
/// KiB page, upstream's `COALESCE`) was already ours ([`RefreshRanges`],
/// #827); upstream's `current` was one span per copy (6d021fe3), which
/// the fork never took.
/// Not used while shader results are owned by their copies
/// (`VICTORY_COPY_BACK_ONLY_READBACK`): materializing them writes title
/// memory under a range kept current.
fn copy_parts() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_COPY_PARTS").is_some_and(|v| v == "1");
        if on {
            tracing::info!("a submission does not refresh again the ranges of a copy it brought up to date (VICTORY_COPY_PARTS=1)");
        }
        on
    })
}

/// Adds `[address, end)` to `ranges`, kept disjoint and sorted: what the
/// range overlaps, or lies within `gap` of, merges into it, and ranges it
/// now bridges merge together (Marvisak #2428 `push_range`). The ranges
/// copied take no gap, so coverage never claims a byte no flush copied (a
/// request inside a gap would be wrongly skipped).
fn push_range(ranges: &mut Vec<(usize, usize)>, gap: usize, address: usize, end: usize) {
    let (mut from, mut to) = (address, end);
    let mut i = 0;
    while i < ranges.len() {
        let (a, e) = ranges[i];
        if a <= to.saturating_add(gap) && from <= e.saturating_add(gap) {
            ranges.remove(i);
            from = from.min(a);
            to = to.max(e);
        } else {
            i += 1;
        }
    }
    ranges.push((from, to));
    ranges.sort_unstable();
}

/// Whether `[address, end)` lies inside one of `ranges`.
fn covered(ranges: &[(usize, usize)], address: usize, end: usize) -> bool {
    ranges.iter().any(|&(a, e)| a <= address && end <= e)
}

type ShadowRefresh = (std::sync::Arc<crate::shadow::Shadow>, RefreshRanges);

/// Keep a single range inline. Merge accumulated overlaps before refreshing.
enum RefreshRanges {
    One((usize, usize)),
    Many(Vec<(usize, usize)>),
}

impl RefreshRanges {
    fn add(&mut self, (from, to): (usize, usize)) {
        match self {
            Self::One((start, end)) if from <= *end && *start <= to => {
                *start = (*start).min(from);
                *end = (*end).max(to);
            }
            Self::One(previous) => *self = Self::Many(vec![*previous, (from, to)]),
            Self::Many(ranges) => ranges.push((from, to)),
        }
    }

    /// Joins overlapping ranges, and ranges at most `gap` bytes apart
    /// (traidy2222 #827: [`COPY_GAP`] for the flush of all, the bytes
    /// between are title memory too).
    fn merge(&mut self, gap: usize) {
        if let Self::Many(ranges) = self {
            ranges.sort_unstable();
            ranges.dedup_by(|next, previous| {
                if next.0 > previous.1.saturating_add(gap) {
                    return false;
                }
                previous.1 = previous.1.max(next.1);
                true
            });
        }
    }

    fn as_slice(&self) -> &[(usize, usize)] {
        match self {
            Self::One(range) => std::slice::from_ref(range),
            Self::Many(ranges) => ranges,
        }
    }
}

impl Batch {
    fn add(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>, address: usize, len: usize) {
        if len == 0 {
            return;
        }
        let range = (address, address + len);
        let key = std::sync::Arc::as_ptr(shadow) as usize;
        if copy_parts()
            && !crate::shadow::owners_any()
            && self.current.get(&key).is_some_and(|(_, ranges)| covered(ranges, range.0, range.1))
        {
            PREP_STATS[P::PartsSkipped as usize].fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.span = if self.ranges == 0 { range } else { (self.span.0.min(range.0), self.span.1.max(range.1)) };
        self.ranges += 1;
        match self.index.get(&key) {
            Some(&at) => self.refreshes[at].1.add(range),
            None => {
                self.index.insert(key, self.refreshes.len());
                self.refreshes.push((shadow.clone(), RefreshRanges::One(range)));
            }
        }
    }

    /// Before shaders are given a range of `shadow`'s copy: its pending
    /// refresh is done, the others stay pending.
    fn flush_one(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>) {
        let Some(at) = self.index.remove(&(std::sync::Arc::as_ptr(shadow) as usize)) else {
            return;
        };
        let (shadow, mut ranges) = self.refreshes.swap_remove(at);
        // The last entry took its place.
        if let Some(moved) = self.refreshes.get(at) {
            self.index.insert(std::sync::Arc::as_ptr(&moved.0) as usize, at);
        }
        ranges.merge(COPY_GAP);
        self.now_current(&shadow, &ranges);
        Self::update(&shadow, &ranges);
        if self.refreshes.is_empty() {
            (self.span, self.ranges) = ((0, 0), 0);
        }
    }

    /// `VICTORY_COPY_PARTS`: `ranges` of `shadow` are brought up to date
    /// now (before its update, which no title write can come between).
    fn now_current(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>, ranges: &RefreshRanges) {
        if !copy_parts() {
            return;
        }
        let key = std::sync::Arc::as_ptr(shadow) as usize;
        let (_, current) = self.current.entry(key).or_insert_with(|| (shadow.clone(), vec![]));
        for &(from, to) in ranges.as_slice() {
            push_range(current, 0, from, to);
        }
    }

    /// Brings each requested range of a copy up to date, timed.
    fn update(shadow: &crate::shadow::Shadow, ranges: &RefreshRanges) {
        for &(from, to) in ranges.as_slice() {
            let started = std::time::Instant::now();
            let copied = shadow.update(from, to - from);
            let us = started.elapsed().as_micros() as u64;
            #[cfg(feature = "diagnostics")]
            crate::copystat::range(from, to - from, us, 0);
            preparing(0, us);
            preparing(1, copied as u64);
        }
    }

    /// Whether `staging` is still to be refreshed in this submission (and
    /// notes it refreshed).
    fn texture_due<T>(&mut self, staging: &std::sync::Arc<T>) -> bool {
        self.textures.insert(std::sync::Arc::as_ptr(staging) as *const () as usize)
    }

    /// Before a step that writes title memory: pending refreshes are done
    /// first, and textures are checked again after it.
    fn title_written(&mut self) {
        self.flush();
        self.textures_done();
        self.textures.clear();
        self.current.clear();
    }

    /// `VICTORY_PREP_COPY_FLUSH`: before a title copy into `[dst, dst +
    /// len)`: the pending refreshes of copies with a range that shares a
    /// page with it are done first, the others stay pending; textures are
    /// checked again after it, as with [`Batch::title_written`]. Many
    /// pending ranges, or all of them meeting it: all are done.
    fn title_written_at(&mut self, dst: usize, len: usize) {
        /// Pending ranges looked at one by one, at most; past it all are
        /// done (as before), so a copy never scans a long list.
        const MOST: usize = 256;
        const PAGE: usize = 4096;
        if !self.refreshes.is_empty() {
            let written = (dst, dst.saturating_add(len));
            let span = self.span;
            if !victory_core::prep::pages_meet(span, written, PAGE) {
                PREP_STATS[P::CopyFlushesSaved as usize].fetch_add(1, Ordering::Relaxed);
            } else if self.ranges > MOST {
                PREP_STATS[P::CopyFlushes as usize].fetch_add(1, Ordering::Relaxed);
                self.flush();
            } else {
                let meeting: Vec<std::sync::Arc<crate::shadow::Shadow>> = self
                    .refreshes
                    .iter()
                    .filter(|(_, ranges)| ranges.as_slice().iter().any(|&r| victory_core::prep::pages_meet(r, written, PAGE)))
                    .map(|(shadow, _)| shadow.clone())
                    .collect();
                if meeting.len() == self.refreshes.len() {
                    PREP_STATS[P::CopyFlushes as usize].fetch_add(1, Ordering::Relaxed);
                    self.flush();
                } else {
                    // Some (or none) done now, the rest left pending.
                    PREP_STATS[P::CopyFlushesSaved as usize].fetch_add(1, Ordering::Relaxed);
                    for shadow in &meeting {
                        self.flush_one(shadow);
                    }
                }
            }
        }
        self.textures_done();
        self.textures.clear();
        self.current.clear();
    }

    /// Waits for the texture refreshes under way on other threads.
    fn textures_done(&mut self) {
        // `VICTORY_WIDE_TEXTURES`: the queued checks first (before title
        // memory is written and before the lists run).
        self.refresh_textures();
        if self.textures_beside.busy() {
            let started = std::time::Instant::now();
            self.textures_beside.join();
            crate::memcopy::add(crate::memcopy::M::OverlapWaitUs, started.elapsed().as_micros() as u64);
        }
    }

    fn flush(&mut self) {
        if self.refreshes.is_empty() {
            self.index.clear();
            return;
        }
        let begun = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let faults = crate::regions::page_faults();
        self.flush_all();
        {
            use crate::regions::{S, add, max};
            let us = begun.elapsed().as_micros() as u64;
            add(S::Flushes, 1);
            add(S::FlushUs, us);
            max(S::FlushMaxUs, us);
            #[cfg(feature = "diagnostics")]
            add(S::FlushFaults, crate::regions::page_faults().saturating_sub(faults));
        }
        #[cfg(feature = "diagnostics")]
        crate::regions::tick();
    }

    fn flush_all(&mut self) {
        self.index.clear();
        (self.span, self.ranges) = ((0, 0), 0);
        for (_, ranges) in &mut self.refreshes {
            ranges.merge(COPY_GAP);
        }
        if copy_parts() {
            let done: Vec<ShadowRefresh> = self.refreshes.iter().map(|(s, r)| (s.clone(), RefreshRanges::Many(r.as_slice().to_vec()))).collect();
            for (shadow, ranges) in &done {
                self.now_current(shadow, ranges);
            }
        }
        // One write-watch asking for ranges that lie close together.
        let ahead = self.refreshes.len() > 1
            || self.refreshes.first().is_some_and(|(_, ranges)| ranges.as_slice().len() > 1);
        if ahead {
            let started = std::time::Instant::now();
            let ranges: Vec<(usize, usize)> = self
                .refreshes
                .iter()
                .filter(|(shadow, _)| !shadow.unwatched())
                .flat_map(|(_, ranges)| ranges.as_slice().iter().copied())
                .collect();
            crate::watch::ask_ahead(&ranges);
            preparing(0, started.elapsed().as_micros() as u64);
        }
        if wide_refresh() && self.refreshes.len() >= 4 {
            // Each copy on its own, several at a time: a copy's refresh
            // writes that copy and reads title memory, nothing of another's.
            // The time counted is the time it took, not the threads' sum.
            let started = std::time::Instant::now();
            let before = PREPARING[0].load(Ordering::Relaxed);
            let jobs: Vec<Box<dyn FnOnce() + Send>> = self
                .refreshes
                .drain(..)
                .map(|(shadow, ranges)| Box::new(move || Self::update(&shadow, &ranges)) as Box<dyn FnOnce() + Send>)
                .collect();
            crate::shadow::wide_run(jobs);
            let summed = PREPARING[0].load(Ordering::Relaxed).saturating_sub(before);
            let over = summed.saturating_sub(started.elapsed().as_micros() as u64);
            PREPARING[0].fetch_sub(over, Ordering::Relaxed);
            crate::framelog::sub(crate::framelog::T::CopyUs, over);
        } else {
            for (shadow, ranges) in self.refreshes.drain(..) {
                Self::update(&shadow, &ranges);
            }
        }
        if ahead {
            crate::watch::ask_ahead_done();
        }
    }

    /// The textures queued by [`wide_textures`], several at a time. Each
    /// refresh reads its texture's title memory and writes its own staging
    /// buffer and notes (under its own lock); a texture is queued once per
    /// batch ([`Batch::texture_due`]). The time counted is the time it
    /// took, not the threads' sum.
    fn refresh_textures(&mut self) {
        match self.textures_due.len() {
            0 => {}
            1 => {
                if let Some(staging) = self.textures_due.pop() {
                    staging.refresh();
                }
            }
            _ => {
                let started = std::time::Instant::now();
                let before = PREPARING[2].load(Ordering::Relaxed);
                let jobs: Vec<Box<dyn FnOnce() + Send>> = self
                    .textures_due
                    .drain(..)
                    .map(|staging| Box::new(move || staging.refresh()) as Box<dyn FnOnce() + Send>)
                    .collect();
                crate::shadow::wide_run(jobs);
                let summed = PREPARING[2].load(Ordering::Relaxed).saturating_sub(before);
                PREPARING[2].fetch_sub(summed.saturating_sub(started.elapsed().as_micros() as u64), Ordering::Relaxed);
            }
        }
    }
}

/// Whether a submission's texture checks run several at a time
/// (`VICTORY_WIDE_TEXTURES=1`; off by default; fx #768).
///
/// While the world streams in, a submission checks hundreds of textures,
/// one after another on the submitting thread. With the switch the checks
/// due in a batch are queued and run at its next flush (before a step that
/// writes title memory, before a host step, and before the lists run) on
/// the threads kept for the snapshots (`crate::shadow::wide`).
fn wide_textures() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WIDE_TEXTURES").is_some_and(|v| v == "1"))
}

/// Copies title memory at submission. The ranges were checked against their
/// resources when recorded, but the title may since have unmapped pages of
/// them (tiled memory, page mappings), which read as zeros here; a title
/// destination (`title_dst`) that is not all mapped is left alone.
///
/// The source is read through the kernel (`ReadProcessMemory`), which
/// reports an unmapped page instead of faulting. Asking `VirtualQuery` first
/// cost about 2 ms per copy in the title's big pools (it walks the page
/// tables to the region's end), which made copies run at 100-200 MB/s.
/// `VICTORY_COPY_FAST=1`: a copy of title memory into a host copy of ours
/// is a plain `memcpy` when the kernel says every page of the source can be
/// read, instead of a `ReadProcessMemory` of the whole range.
///
/// Why: the kernel's copy runs at about 600 MB/s on this title's buffers
/// (measured in gameplay: 182 MB in 260 ms, 380 MB in 581 ms), while the
/// CPU moves the same bytes at several GB/s. The copies are the frame's
/// largest bucket, so the call's own cost is most of the frame.
///
/// Off by default while the race below is not settled: the title may unmap
/// a page between the question and the copy (a page pool's pages move when
/// streaming replaces them), and a fault there kills the process, where the
/// kernel's call reports the page instead. Pages of the buffers a list
/// declares are not shown to move in the runs so far.
fn fast_reads() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COPY_FAST").is_some_and(|v| v == "1"))
}

pub(crate) fn copy_mapped(dst: *mut u8, src: *const u8, len: usize, title_dst: bool) {
    #[cfg(feature = "diagnostics")]
    match crate::copystat::mode() {
        1 => {
            // SAFETY: timing only: the caller vouches for the source.
            unsafe { std::ptr::copy(src, dst, len) };
            return;
        }
        2 => return,
        _ => {}
    }
    if len == 0 {
        return;
    }
    // Into host memory: the runtime's record of what it mapped says where
    // the holes are, without a question to Windows (`crate::regions`).
    if !title_dst && crate::regions::copy(dst, src, len, |d, s, n| copy_os(d, s, n, false)) {
        return;
    }
    let begun = std::time::Instant::now();
    copy_os(dst, src, len, title_dst);
    {
        use crate::regions::{S, add};
        add(S::OsCopies, 1);
        add(S::OsCopyBytes, len as u64);
        add(S::OsCopyUs, begun.elapsed().as_micros() as u64);
    }
}

/// `VICTORY_PREP_COPY_DIRECT`: a title copy (title memory into title
/// memory) as a plain move, if both ranges are at most 1 MiB and the
/// runtime's mapping record says all of both is mapped, checked with the
/// mappings of both held still ([`crate::watch::title_memory_reading_now`]:
/// no change of them can begin until the move is done). False if not done
/// (the caller copies through the kernel).
fn copy_direct(dst: usize, src: usize, len: usize) -> bool {
    const MOST: usize = 1 << 20;
    if len == 0 || len > MOST || !crate::regions::fast() {
        return false;
    }
    let Some(_source) = crate::watch::title_memory_reading_now(src, len) else {
        return false;
    };
    let Some(_destination) = crate::watch::title_memory_reading_now(dst, len) else {
        return false;
    };
    if crate::regions::all_mapped(src, len) != Some(true) || crate::regions::all_mapped(dst, len) != Some(true) {
        return false;
    }
    // As the kernel's write: guarded blocks opened, placed textures told.
    crate::watch::writing(dst, len);
    // SAFETY: the record says both ranges show mapped, committed,
    // read-write views (the runtime commits a view before telling), and no
    // change of their mapping can begin while the two guards are held. The
    // ranges may overlap: a move.
    unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, len) };
    PREP_STATS[P::CopiesDirect as usize].fetch_add(1, Ordering::Relaxed);
    true
}

/// Whether title memory `[at, at + len)` is all mapped: the runtime's
/// record if it knows (`crate::regions`), else Windows.
fn title_committed(at: *const u8, len: usize) -> bool {
    match crate::regions::all_mapped(at as usize, len) {
        Some(all) => all,
        None => crate::regions::timed(crate::regions::S::Queries, Some(crate::regions::S::QueryUs), || {
            crate::texture::committed(at, len)
        }),
    }
}

/// [`copy_mapped`] through the kernel, as before the record.
fn copy_os(dst: *mut u8, src: *const u8, len: usize, title_dst: bool) {
    if len == 0 {
        return;
    }
    {
        use std::sync::atomic::Ordering::Relaxed;
        if title_dst {
            COPY_MOVED[2].fetch_add(len as u64, Relaxed);
            COPY_MOVED[3].fetch_add(1, Relaxed);
        } else {
            COPY_MOVED[0].fetch_add(len as u64, Relaxed);
            COPY_MOVED[1].fetch_add(1, Relaxed);
        }
    }
    // A title destination that is not all mapped is left alone. Asking
    // Windows first (`VirtualQuery`) walks the page tables to the end of
    // the destination's region: in the title's big blocks that is
    // milliseconds a copy, and it was a seventh of the submitting thread's
    // time in gameplay (run 60: 71 of 500 looks). The kernel's copy below
    // says the same when it fails, so the question is only asked then.
    // `VICTORY_COPY_ASK=1` asks first, as before.
    static ASK: OnceLock<bool> = OnceLock::new();
    let ask_first = title_dst
        && *ASK.get_or_init(|| std::env::var_os("VICTORY_COPY_ASK").is_some_and(|v| v == "1"));
    if ask_first && !title_committed(dst, len) {
        return;
    }
    if title_dst {
        // The kernel writes the destination: it must not be guarded.
        crate::watch::writing(dst as usize, len);
    }
    // `VICTORY_COPY_FAST=1`: the source is all there, so move it with the
    // CPU ([`fast_reads`] has why this is off by default).
    if !title_dst && len >= 64 << 10 && fast_reads() && crate::texture::committed(src, len) {
        // SAFETY: the question above found every page of the range readable,
        // and a host destination is our own copy of `len` bytes.
        unsafe { std::ptr::copy_nonoverlapping(src, dst, len) };
        return;
    }
    #[cfg(feature = "diagnostics")]
    let moved = std::time::Instant::now();
    // (A title destination may be missing too: the kernel says so.) Into
    // host memory: in place first, if asked for (`crate::plaincopy`);
    // through the kernel if that is off or met a page that was not there.
    let whole = if title_dst {
        read_kernel(src, dst, len)
    } else {
        crate::plaincopy::copy(dst, src, len) || read(src, dst, len)
    };
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::MoveUs, moved.elapsed().as_micros() as u64);
    if whole {
        // A title destination the kernel wrote: its pages' dirty bits are
        // the layer's own write. Left set, the next refresh reports them as
        // the title's and copies the same bytes up again, every frame.
        if title_dst {
            crate::watch::forgot(dst as usize, len);
        }
        return;
    }
    // The copy failed: a source page is not there, or the destination is
    // not all mapped. Then the destination stays as it is (the kernel may
    // have written its first pages).
    if title_dst && !ask_first && !title_committed(dst, len) {
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::Fallbacks, 1);
    #[cfg(feature = "diagnostics")]
    let sparse = std::time::Instant::now();
    if !title_dst && len >= 64 << 10 {
        read_sparse(src, dst, len);
    } else {
        read_pieces(src, dst, len);
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::SparseUs, sparse.elapsed().as_micros() as u64);
}

/// After a failed bulk read into host memory, skip contiguous holes.
/// `VICTORY_COPY_NOQUERY=1`: the runs come from the runtime's mapping
/// record (`crate::regions::sparse_runs`), and `VirtualQuery` is asked only
/// about memory it does not know ([`read_sparse_query`]).
fn read_sparse(src: *const u8, dst: *mut u8, len: usize) {
    let Some(runs) = crate::regions::sparse_runs(src as usize, len) else {
        read_sparse_query(src, dst, len);
        return;
    };
    read_sparse_runs(src, dst, &runs);
}

/// [`read_sparse`] by the record's runs (fetched with its lock let go: the
/// translation table and the kernel are asked without it). A mapped run is
/// read as a committed region after a query was (the kernel's guarded read,
/// piece by piece if a mapping changed meanwhile); a hole as an uncommitted
/// one (zeros, or the translation table's bytes), after one look at its
/// first byte through the kernel, as `crate::regions::copy` does: memory the
/// title reserved anew without the runtime reads, and is read as committed.
fn read_sparse_runs(src: *const u8, dst: *mut u8, runs: &[victory_core::memmap::Run]) {
    use victory_core::memmap::Kind;
    for run in runs {
        let at = run.start as *const u8;
        let to = dst.wrapping_add(run.start - src as usize);
        let n = run.len();
        match run.kind {
            Kind::Unknown => read_sparse_query(at, to, n),
            Kind::Mapped => {
                crate::regions::add(crate::regions::S::QueriesSaved, 1);
                if !read(at, to, n) {
                    read_pieces(at, to, n);
                }
            }
            Kind::Hole => {
                crate::regions::add(crate::regions::S::QueriesSaved, 1);
                let mut byte = [0u8];
                if read_into(at, &mut byte) {
                    if !read(at, to, n) {
                        read_pieces(at, to, n);
                    }
                    continue;
                }
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
                if crate::translate::any(run.start, n) {
                    crate::translate::read(run.start, to, n, |_, d, k| {
                        // SAFETY: inside the host destination.
                        unsafe { d.write_bytes(0, k) };
                        true
                    });
                } else {
                    // SAFETY: this path is only used with a writable host destination.
                    unsafe { to.write_bytes(0, n) };
                }
            }
        }
    }
}

/// [`read_sparse`] asking Windows region by region, as before the record.
/// Never cache the query across copies: the title may remap the same address.
fn read_sparse_query(src: *const u8, dst: *mut u8, len: usize) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualQuery,
    };
    let mut offset = 0;
    while offset < len {
        let at = src.wrapping_add(offset);
        let to = dst.wrapping_add(offset);
        let remaining = len - offset;
        // SAFETY: the query inspects an address and fills a local structure.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let size = crate::regions::timed(crate::regions::S::Queries, Some(crate::regions::S::QueryUs), || unsafe {
            VirtualQuery(at.cast(), &mut info, size_of_val(&info))
        });
        let n = (info.BaseAddress as usize).saturating_add(info.RegionSize)
            .saturating_sub(at as usize).min(remaining);
        if size != size_of_val(&info) || n == 0 {
            read_pieces(at, to, remaining);
            break;
        }
        if info.State != MEM_COMMIT || info.Protect == PAGE_NOACCESS {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
            // Pages shown through the translation table read their bytes
            // (`VICTORY_MEM2_GPU_ONLY_NOVIEW`); the rest are zeros.
            if crate::translate::any(at as usize, n) {
                crate::translate::read(at as usize, to, n, |_, d, k| {
                    // SAFETY: inside the host destination.
                    unsafe { d.write_bytes(0, k) };
                    true
                });
            } else {
                // SAFETY: this path is only used with a writable host destination.
                unsafe { to.write_bytes(0, n) };
            }
        } else if !read(at, to, n) {
            // A mapping may change between query and read.
            read_pieces(at, to, n);
        }
        offset += n;
    }
}

fn read_pieces(src: *const u8, dst: *mut u8, len: usize) {
    // Something is unmapped: page by page (64 KiB, then 4 KiB in a piece
    // that fails), zeros for what cannot be read.
    for_pieces(src as usize, len, 64 << 10, |at, n| {
        let to = dst.wrapping_add(at - src as usize);
        if !read_kernel(at as *const u8, to, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                let to = dst.wrapping_add(at - src as usize);
                if !read_kernel(at as *const u8, to, n) {
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
                    // SAFETY: inside the destination, which is mapped.
                    unsafe { to.write_bytes(0, n) };
                }
            });
        }
    });
}

/// Reads title memory at `src` into `dst`; false if a page of it is not
/// readable.
pub(crate) fn read_into(src: *const u8, dst: &mut [u8]) -> bool {
    // In place first, if asked for (`crate::plaincopy`): the destination is
    // memory of ours, as in `copy_mapped`.
    crate::plaincopy::copy(dst.as_mut_ptr(), src, dst.len()) || read(src, dst.as_mut_ptr(), dst.len())
}

/// Copies `len` bytes from `src` to `dst` unless a source page is not
/// readable.
fn read(src: *const u8, dst: *mut u8, len: usize) -> bool {
    crate::native::probe::release(src as usize, len, false);
    // Pages shown through the translation table (`VICTORY_MEM2_GPU_ONLY_NOVIEW`)
    // have no view for the kernel to read: their bytes come from their
    // sections, the rest through the kernel.
    if crate::translate::any(src as usize, len) {
        return crate::translate::read(src as usize, dst, len, |at, d, n| read_os(at as *const u8, d, n));
    }
    read_os(src, dst, len)
}

/// [`read`] past the translation table: `VICTORY_SAFE_COPY=1` (yaajme)
/// reads in place with the fault caught, else the kernel.
fn read_os(src: *const u8, dst: *mut u8, len: usize) -> bool {
    if crate::safe_copy::on() {
        // SAFETY: the destination is mapped writable memory of `len` bytes; a
        // source page that is not there ends the copy with `false`.
        return unsafe { crate::safe_copy::copy(dst, src, len) };
    }
    read_kernel(src, dst, len)
}

/// [`read`] by the kernel, which checks the destination too.
fn read_kernel(src: *const u8, dst: *mut u8, len: usize) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut done = 0;
    crate::regions::add(crate::regions::S::Reads, 1);
    // SAFETY: the destination is mapped writable memory of `len` bytes; the
    // kernel checks the source.
    unsafe { ReadProcessMemory(GetCurrentProcess(), src.cast(), dst.cast(), len, &mut done) != 0 }
}

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    /// The kernel's write into a process's memory, `(process, address,
    /// bytes, length, written out)`: an NTSTATUS, negative when a page of
    /// the range is not there or not writable. It changes no protection.
    fn NtWriteVirtualMemory(
        process: *mut std::ffi::c_void,
        base: *mut std::ffi::c_void,
        buffer: *const std::ffi::c_void,
        size: usize,
        written: *mut usize,
    ) -> i32;
}

/// Writes `len` bytes by the kernel; `false` if not all of `dst` is there.
pub(crate) fn write_kernel(dst: *mut u8, src: *const u8, len: usize) -> bool {
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut done = 0;
    // SAFETY: the source is live memory of ours; the kernel checks the rest.
    unsafe { NtWriteVirtualMemory(GetCurrentProcess(), dst.cast(), src.cast(), len, &mut done) >= 0 && done == len }
}

/// Whether title memory is written with the kernel's own call
/// (`NtWriteVirtualMemory`) and not `WriteProcessMemory`, which first asks
/// what the pages are (as `VirtualQuery` does: about 2 ms a call in the
/// title's big pools, see [`copy_mapped`]): putting back what shaders wrote
/// cost 1.2-1.9 s a frame for 430-800 pieces, 2 ms each whatever their size
/// (run 24). On by default; `VICTORY_DIRECT_WRITES=0` writes as before.
fn direct_writes() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DIRECT_WRITES").is_none_or(|v| v != "0"))
}

/// Copies host memory into title memory after a submission (what shaders
/// wrote), leaving pages that are not mapped alone.
pub(crate) fn write_mapped(dst: *mut u8, src: *const u8, len: usize) {
    // Pages shown through the translation table (`VICTORY_MEM2_GPU_ONLY_NOVIEW`):
    // written in their sections, as a write through a view would.
    if crate::translate::any(dst as usize, len) {
        crate::translate::write(dst as usize, src, len, |at, from, n| write_mapped_os(at as *mut u8, from, n));
        return;
    }
    write_mapped_os(dst, src, len);
}

/// [`write_mapped`] for a run of copy-back whose guards the caller opened
/// (`crate::watch::writing_soft`): with `VICTORY_CB_ALIASWRITE=1`, a plain
/// store in place with its fault caught, no kernel call (about 5 us a run
/// by `NtWriteVirtualMemory`). The pages are writable once opened, so this
/// is the same write the kernel makes, through the same addresses (the
/// same write-watch bits, no guard fault: the guards' handler leaves this
/// instruction's faults alone). If a page faults (not mapped, not
/// writable, guarded again meanwhile), the whole run goes the old way,
/// which writes what it can exactly as before.
pub(crate) fn write_run(dst: *mut u8, src: *const u8, len: usize) {
    write_run_with(crate::shadow::cbfast::aliaswrite_on(), dst, src, len);
}

/// [`write_run`], in place or not whatever the switch says (for tests).
pub(crate) fn write_run_with(inplace: bool, dst: *mut u8, src: *const u8, len: usize) {
    if len == 0 {
        return;
    }
    if inplace && !crate::translate::any(dst as usize, len) && crate::safe_copy::write_ready() {
        // SAFETY: `src` is our merged run of `len` bytes; a missing or
        // read-only destination page ends the copy with `false`.
        if unsafe { crate::safe_copy::write(dst, src, len) } {
            crate::shadow::cbfast::ALIASWRITE[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        crate::shadow::cbfast::ALIASWRITE[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    write_mapped(dst, src, len);
}

/// [`write_mapped`] through the kernel.
fn write_mapped_os(dst: *mut u8, src: *const u8, len: usize) {
    use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let direct = direct_writes();
    let write = |at: usize, n: usize| {
        let from = src.wrapping_add(at - dst as usize);
        let mut done = 0;
        if direct {
            // SAFETY: as below; the kernel checks the destination and
            // writes nothing it cannot (the caller opened its guards:
            // `crate::watch::writing`).
            return unsafe {
                NtWriteVirtualMemory(GetCurrentProcess(), at as _, from.cast(), n, &mut done) >= 0
            };
        }
        // SAFETY: the source is our mapped host copy; the kernel checks the
        // destination.
        unsafe { WriteProcessMemory(GetCurrentProcess(), at as _, from.cast(), n, &mut done) != 0 }
    };
    if len == 0 || write(dst as usize, len) {
        return;
    }
    for_pieces(dst as usize, len, 64 << 10, |at, n| {
        if !write(at, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                write(at, n);
            });
        }
    });
}

/// Calls `f` with the pieces of `[start, start + len)` cut at multiples of
/// `step`.
fn for_pieces(start: usize, len: usize, step: usize, mut f: impl FnMut(usize, usize)) {
    let end = start + len;
    let mut at = start;
    while at < end {
        let next = ((at / step + 1) * step).min(end);
        f(at, next - at);
        at = next;
    }
}

impl Host {
    /// The queue everything of ours runs on, for a call that must go to it
    /// in order with the lists (`crate::tiles`: a texture's levels are
    /// given memory ahead of the list that uploads them).
    pub(crate) fn queue(&self) -> &ID3D12CommandQueue {
        &self.queue
    }

    /// A buffer in the upload heap, mapped for writing.
    /// A buffer of `size` bytes the card writes and we read (a read back),
    /// in the state copies into it need.
    pub(crate) fn read_back_buffer(&self, size: u64) -> Option<ID3D12Resource> {
        crate::census::made(0, size);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a read-back buffer.
        let made = unsafe {
            self.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
        };
        match made {
            Ok(()) => buffer,
            Err(e) => {
                tracing::warn!(error = %e, size, "no read-back buffer");
                None
            }
        }
    }

    pub(crate) fn upload_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        crate::census::made(1, size);
        MADE[0].fetch_add(1, Ordering::Relaxed);
        MADE[1].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of an upload buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no upload buffer");
                None
            }
        }
    }
}

/// The swap chain formats a presented texture can be copied into: its own
/// family's.
fn swap_format(format: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
    Some(match format.0 {
        27..=32 => DXGI_FORMAT_R8G8B8A8_UNORM,
        87 | 90 | 91 => DXGI_FORMAT_B8G8R8A8_UNORM,
        23..=25 => DXGI_FORMAT_R10G10B10A2_UNORM,
        9..=14 => DXGI_FORMAT_R16G16B16A16_FLOAT,
        _ => return None,
    })
}

const BUFFERS: u32 = 2;

struct Presenter {
    swap_chain: IDXGISwapChain3,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    copy: HostList,
    /// With presents sent ahead ([`late_free_present`]): a list for each
    /// present in the air, and the submission it last flew as (it is used
    /// again once that one has finished).
    ahead: Vec<(HostList, u64)>,
    hwnd: HWND,
    meter: Meter,
    /// Samples the frame for the boot splash until it has faded.
    sampler: Option<crate::splash::Sampler>,
    /// The splash picture as a texture (and the upload buffer it came from) while it shows.
    splash: Option<(ID3D12Resource, ID3D12Resource)>,
    /// Pacing and latency ([`victory_core::pace`]).
    pace: Pace,
}

/// The present side of [`victory_core::pace`]: the swap chain's latency
/// object, the pacer and its timer, the statistics.
struct Pace {
    /// `VICTORY_PACE_LATENCY`: signalled when the swap chain takes another frame.
    waitable: Option<HANDLE>,
    /// The swap chain was made with tearing allowed (`VICTORY_PACE_TEARING`).
    tearing: bool,
    pacer: Option<victory_core::pace::Pacer>,
    timer: Option<HANDLE>,
    window: victory_core::pace::Window,
    /// (present count, title token) of presents not yet seen on the display.
    shown: std::collections::VecDeque<(u32, u64)>,
    /// The last present count the display statistics answered for.
    seen: u32,
}

// SAFETY: the handles are plain kernel handles, used under the presenter lock.
unsafe impl Send for Pace {}

/// `VICTORY_LOW_LATENCY`: the swap chain's latency object, waited on at the
/// title's frame start ([`crate::frame`]'s origin wait) instead of by the
/// presenter (0: none).
pub(crate) static FRAME_ROOM: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// At the title's frame start: wait until the swap chain has room
/// (`VICTORY_LOW_LATENCY`), then for the limiter (`VICTORY_FPS_CAP`).
pub(crate) fn frame_start_wait() {
    use std::sync::atomic::Ordering::Relaxed;
    use victory_core::pace;
    if victory_core::fastboot::unthrottled() {
        return;
    }
    let h = FRAME_ROOM.load(Relaxed);
    if h != 0 {
        let t0 = pace::now_ns();
        // SAFETY: the swap chain's latency object, kept for the process.
        let r = unsafe { WaitForSingleObjectEx(HANDLE(h as _), 50, true) };
        pace::START_WAIT_NS.fetch_add(pace::now_ns() - t0, Relaxed);
        if r == windows::Win32::Foundation::WAIT_TIMEOUT {
            pace::START_TIMEOUTS.fetch_add(1, Relaxed);
        }
    }
    // The timer as a raw value: a kernel handle kept for the process.
    static LIMITER: std::sync::Mutex<Option<(pace::Limiter, isize)>> = std::sync::Mutex::new(None);
    let Some(period) = pace::limiter_period() else { return };
    let mut g = LIMITER.lock().unwrap_or_else(|e| e.into_inner());
    let (lim, timer) = g.get_or_insert_with(|| (pace::Limiter::default(), high_res_timer().map_or(0, |h| h.0 as isize)));
    let now = pace::now_ns();
    let at = lim.start_at(now, period);
    sleep_until((*timer != 0).then(|| HANDLE(*timer as _)), at);
    pace::LIMIT_WAIT_NS.fetch_add(at - now, Relaxed);
}

fn high_res_timer() -> Option<HANDLE> {
    use windows::Win32::System::Threading::{
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, TIMER_ALL_ACCESS,
    };
    // SAFETY: plain creation; falls back to an ordinary timer on older systems.
    unsafe {
        CreateWaitableTimerExW(None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)
            .or_else(|_| CreateWaitableTimerExW(None, None, 0, TIMER_ALL_ACCESS.0))
            .ok()
    }
}

/// Swap chain flags for the pacing switches.
fn swap_flags(tearing: bool) -> DXGI_SWAP_CHAIN_FLAG {
    let mut f = 0;
    if victory_core::pace::switches().latency > 0 {
        f |= DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0;
    }
    if tearing {
        f |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0;
    }
    DXGI_SWAP_CHAIN_FLAG(f)
}

/// Whether the system can present with tearing (variable refresh).
fn tearing_supported(factory: &IDXGIFactory2) -> bool {
    let Ok(f5) = factory.cast::<IDXGIFactory5>() else { return false };
    let mut allow = windows::core::BOOL(0);
    // SAFETY: a BOOL-sized out value for this feature.
    unsafe {
        f5.CheckFeatureSupport(
            DXGI_FEATURE_PRESENT_ALLOW_TEARING,
            (&raw mut allow).cast(),
            size_of::<windows::core::BOOL>() as u32,
        )
    }
    .is_ok()
        && allow.as_bool()
}

/// Sleeps until [`victory_core::pace::now_ns`] reaches `until`: a
/// high-resolution waitable timer for most of it, a short spin for the rest.
fn sleep_until(timer: Option<HANDLE>, until: u64) {
    use windows::Win32::System::Threading::SetWaitableTimer;
    const SPIN: u64 = 400_000;
    let now = victory_core::pace::now_ns();
    if until <= now {
        return;
    }
    let coarse = until - now;
    if coarse > SPIN {
        let sleep = coarse - SPIN;
        match timer {
            Some(t) => {
                // Relative, in 100 ns units.
                let due = -((sleep / 100) as i64);
                // SAFETY: our timer handle.
                unsafe {
                    if SetWaitableTimer(t, &due, 0, None, None, false).is_ok() {
                        WaitForSingleObject(t, (sleep / 1_000_000 + 50) as u32);
                    } else {
                        std::thread::sleep(std::time::Duration::from_nanos(sleep));
                    }
                }
            }
            None => std::thread::sleep(std::time::Duration::from_nanos(sleep)),
        }
    }
    while victory_core::pace::now_ns() < until {
        std::hint::spin_loop();
        std::thread::yield_now();
    }
}

impl Pace {
    fn new(swap_chain: &IDXGISwapChain3, tearing: bool) -> Pace {
        let s = victory_core::pace::switches();
        let waitable = (s.latency > 0)
            .then(|| {
                let sc2 = swap_chain.cast::<IDXGISwapChain2>().ok()?;
                // SAFETY: a swap chain made with the waitable flag.
                unsafe {
                    sc2.SetMaximumFrameLatency(s.latency).ok()?;
                    let h = sc2.GetFrameLatencyWaitableObject();
                    (!h.is_invalid()).then_some(h)
                }
            })
            .flatten();
        // Made whether or not pacing is on: the overlay can turn it on.
        let timer = Some(high_res_timer());
        // Low latency: the title's frame start waits on it, not the presenter.
        let waitable = match waitable {
            Some(h) if s.low_latency => {
                FRAME_ROOM.store(h.0 as isize, std::sync::atomic::Ordering::Relaxed);
                None
            }
            w => w,
        };
        tracing::info!(
            ?s,
            waitable = waitable.is_some(),
            tearing,
            "pacing switches (VICTORY_PACE*)"
        );
        Pace {
            waitable,
            tearing,
            pacer: None,
            timer: timer.flatten(),
            window: victory_core::pace::Window::default(),
            shown: std::collections::VecDeque::new(),
            seen: 0,
        }
    }

    /// Before the frame takes a back buffer: wait until the swap chain has
    /// room (at most `VICTORY_PACE_LATENCY` frames queued).
    fn wait_for_room(&mut self) {
        if let Some(h) = self.waitable {
            let t0 = victory_core::pace::now_ns();
            // SAFETY: the swap chain's latency object.
            unsafe { WaitForSingleObjectEx(h, 100, true) };
            self.window.latency_wait += victory_core::pace::now_ns() - t0;
        }
    }

    /// Just before `Present`: hold the frame for an even cadence.
    fn hold(&mut self) {
        if !victory_core::pace::live_pace() || victory_core::fastboot::unthrottled() {
            return;
        }
        let base = victory_core::pace::live_fps().map_or_else(
            || crate::frame::interval().as_nanos() as u64,
            victory_core::pace::period_of_fps,
        );
        // With frame generation running every rendered frame is shown
        // twice: the rate asked for is the output's, so base frames are
        // held two periods.
        let base = base * crate::fsr::framegen_factor();
        let pacer = self.pacer.get_or_insert_with(|| victory_core::pace::Pacer::new(base));
        pacer.set_base(base);
        let ready = victory_core::pace::now_ns();
        let release = pacer.release_at(ready);
        sleep_until(self.timer, release);
        self.window.pace_wait += release - ready;
    }

    /// `Present`'s sync interval and flags.
    fn present_args(&self) -> (u32, DXGI_PRESENT) {
        // Tearing only uncapped: a capped frame waits for the scan-out.
        let capped = victory_core::pace::limiter_period().is_some()
            || (victory_core::pace::live_pace() && victory_core::pace::live_fps().is_some());
        if self.tearing && !capped { (0, DXGI_PRESENT_ALLOW_TEARING) } else { (0, DXGI_PRESENT(0)) }
    }

    /// After `Present`: statistics, and what the display said about earlier presents.
    fn presented(&mut self, swap_chain: &IDXGISwapChain3) {
        let s = victory_core::pace::switches();
        if s.feedback {
            // SAFETY: plain queries of our swap chain.
            unsafe {
                if let Ok(count) = swap_chain.GetLastPresentCount() {
                    self.shown.push_back((count, crate::frame::last_token()));
                    while self.shown.len() > 16 {
                        self.shown.pop_front();
                    }
                }
                let mut st = DXGI_FRAME_STATISTICS::default();
                if swap_chain.GetFrameStatistics(&mut st).is_ok() && st.PresentCount != self.seen {
                    self.seen = st.PresentCount;
                    if let Some(&(_, token)) = self.shown.iter().find(|(c, _)| *c == st.PresentCount) {
                        crate::frame::displayed(token, st.SyncQPCTime);
                    }
                    self.shown.retain(|(c, _)| c.wrapping_sub(st.PresentCount) as i32 > 0);
                }
            }
        }
        if !s.stats {
            return;
        }
        let now = victory_core::pace::now_ns();
        self.window.present(now, victory_core::pace::input_latency(now));
        if let Some(m) = self.window.take_if_due(now) {
            tracing::info!(
                "pace: {} presents, frame time p50 {:.1} / p95 {:.1} / p99 {:.1} ms, present-to-present jitter {:.2} ms, \
                 input-to-present p50 {:.1} / p95 {:.1} ms, waited {:.0} ms for the swap chain and {:.0} ms by the pacer (period {:.1} ms),                  at frame start {:.0} ms for the swap chain ({} timed out) and {:.0} ms by the limiter (cap {} fps, limiter {})",
                m.frames,
                m.p50_ms,
                m.p95_ms,
                m.p99_ms,
                m.jitter_ms,
                m.latency_p50_ms,
                m.latency_p95_ms,
                m.latency_wait_ms,
                m.pace_wait_ms,
                self.pacer.as_ref().map_or(0.0, |p| p.period() as f64 / 1e6),
                victory_core::pace::START_WAIT_NS.swap(0, std::sync::atomic::Ordering::Relaxed) as f64 / 1e6,
                victory_core::pace::START_TIMEOUTS.swap(0, std::sync::atomic::Ordering::Relaxed),
                victory_core::pace::LIMIT_WAIT_NS.swap(0, std::sync::atomic::Ordering::Relaxed) as f64 / 1e6,
                victory_core::pace::FPS_CAP.get(),
                if victory_core::pace::LIMITER.on() { "on" } else { "off" },
            );
        }
    }
}

/// The frame rate, shown in the window's caption every second and logged
/// every few seconds.
struct Meter {
    since: std::time::Instant,
    frames: u32,
    #[cfg(feature = "diagnostics")]
    copy_stats_frames: u64,
    drawn: u64,
    logged: std::time::Instant,
    /// The caption the window had, once read.
    caption: Option<Vec<u16>>,
}

impl Meter {
    fn new() -> Self {
        #[cfg(feature = "diagnostics")]
        crate::profile::start();
        let now = std::time::Instant::now();
        Meter {
            since: now,
            frames: 0,
            #[cfg(feature = "diagnostics")]
            copy_stats_frames: 0,
            drawn: crate::draw::drawn(),
            logged: now,
            caption: None,
        }
    }

    /// Counts a presented frame.
    fn frame(&mut self, hwnd: HWND) {
        #[cfg(feature = "diagnostics")]
        tracing::debug!(target: "victory_gpu::frame", token = crate::frame::current(), "present");
        #[cfg(feature = "diagnostics")]
        crate::copystat::presented();
        #[cfg(feature = "diagnostics")]
        crate::regions::tick();
        self.frames += 1;
        #[cfg(feature = "diagnostics")]
        {
            self.copy_stats_frames += 1;
        }
        let elapsed = self.since.elapsed();
        if elapsed < std::time::Duration::from_secs(1) {
            return;
        }
        let drawn = crate::draw::drawn();
        let frames = f64::from(self.frames);
        let fps = frames / elapsed.as_secs_f64();
        let ms = elapsed.as_secs_f64() * 1000.0 / frames;
        let draws = (drawn - self.drawn) as f64 / frames;
        (self.since, self.frames, self.drawn) = (std::time::Instant::now(), 0, drawn);
        // Of a frame, ms preparing submissions and ms waiting for the GPU.
        let [cpu, gpu] = [0, 1].map(|i| {
            SUBMISSION_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames
        });
        let [copy_ms, copy_mb, texture_ms, texture_mb] = [0, 1, 2, 3].map(|i| {
            let scale = if i % 2 == 0 { 1e3 } else { 1e6 };
            PREPARING[i].swap(0, Ordering::Relaxed) as f64 / scale / frames
        });
        let [back_ms, back_mb] = [0, 1].map(|i| {
            COPY_BACK[i].swap(0, Ordering::Relaxed) as f64 / [1e3, 1e6][i] / frames
        });
        let dropped = crate::draw::take_dropped() as f64 / frames;
        let builder_off = crate::draw::take_dropped_builder() as f64 / frames;
        crate::ab::second(crate::ab::Second { frames, prep_ms: cpu, gpu_ms: gpu, back_ms, copies_mb: copy_mb + back_mb, dropped });
        let indirect = crate::list::take_indirect() as f64 / frames;
        {
            // The empty-record frame counter (7900XTX 449830f6).
            let refused = BUDGET_NOTES[0].swap(0, Ordering::Relaxed) as f64 / frames;
            let biggest = BUDGET_NOTES[1].swap(0, Ordering::Relaxed);
            tracing::info!(
                "indirect budget: {refused:.2} records a frame ran empty for want of their list's budget (2^27), the largest cost of one {biggest} (0: under 2^22); INDIRECT_REST={}",
                u8::from(crate::indirect::rest())
            );
        }
        #[cfg(feature = "diagnostics")]
        {
            let mesh_largest = MESH_GROUPS[0].swap(0, Ordering::Relaxed);
            let mesh_groups = MESH_GROUPS[1].swap(0, Ordering::Relaxed) as f64 / frames;
            tracing::info!(
                "indirect mesh dispatches: the largest group count asked for {mesh_largest} (run: at most 65535), {mesh_groups:.0} groups asked per frame"
            );
        }
        // (Run 200 counted indirect compute records asking for 77,000 to
        // 98,000 groups in x, 0.2 to 0.8 a frame, cut to the 65,535 one
        // dispatch runs; the counter went when the clamp's budget moved to
        // the card's memory.)
        {
            let [texture_us, slots] = [0, 1].map(|i| crate::draw::HEAP_PASS_PARTS[i].swap(0, Ordering::Relaxed));
            tracing::info!(
                "the pass over the heap per frame (the frame line's \"heap buffers ... ms\" is both parts): {:.1} ms in its                  pass over textures picked by index; {:.0} buffer slots looked at by its pass over buffer views",
                texture_us as f64 / 1e3 / frames,
                slots as f64 / frames
            );
        }
        crate::shadow::exact_line(frames);
        crate::descriptor::sync_line(frames);
        late_line(frames);
        re1_line(frames);
        readfix_line(frames);
        barrier_line(frames);
        prep_line(frames);
        prof_line(frames);
        crate::draw::batch_line(frames);
        hazard_line(frames);
        #[cfg(feature = "diagnostics")]
        crate::shadow::echo_line(frames);
        if crate::watch::beside_on() {
            let n = [0, 1].map(|k| crate::watch::BESIDE[k].swap(0, Ordering::Relaxed) as f64 / frames);
            tracing::info!("shader writes put back beside the guards per frame: {:.0}, the old way {:.0}", n[0], n[1]);
        }
        card_clock_line(frames, ms);
        crate::queue2::line(frames);
        crate::plaincopy::line(frames);
        if let Some(host) = host() {
            host.drain_messages();
        }
        {
            let held = crate::pipeline::WALKS_HELD.swap(0, Ordering::Relaxed);
            if held > 0 {
                tracing::info!(
                    "dispatches of programs that walk ray-tracing structures held back per frame (VICTORY_BVH_HOLD=1; the                      builder has not built them): {:.1}",
                    held as f64 / frames
                );
            }
        }
        crate::shadow::back_line(frames);
        {
            let [passes, pieces, compares] = [0, 1, 2].map(|i| CARD_PASSES[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if passes + compares > 0.0 {
                tracing::info!(
                    "our passes over copies in the card's memory per frame: {passes:.0} hash passes over {pieces:.0} pieces after \
                     ranges were sent up (VICTORY_CARD_REHASH_RUNS=1 joins them: now {}), {compares:.0} compare passes",
                    if rehash_runs() { "on" } else { "off" }
                );
            }
        }
        {
            let [own, asking, still, all] =
                [0, 1, 2, 3].map(|i| crate::texture::REFRESH_WAITS[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames);
            if all >= 1.0 {
                tracing::info!(
                    "texture refreshes per frame, all threads: {all:.1} ms, of them {own:.1} ms waiting for a texture another thread \
                     is refreshing, {asking:.1} ms asking the write watch and the guards, {still:.1} ms waiting for title memory to \
                     hold still (the runtime remapping it); the rest is reading, hashing and detiling"
                );
            }
            let [through, left] = [0, 1].map(|i| crate::texture::UNLOCKED[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if through + left > 0.0 {
                tracing::info!(
                    "texture refreshes that did not wait for title memory to hold still, per frame (VICTORY_TEXTURE_NO_WAIT=1): \
                     {through:.1} read through the kernel, {left:.1} left for the next refresh (a page was not there)"
                );
            }
            let [changes, waiting, under_way] = [0, 1, 2].map(|i| {
                crate::watch::REMAPS[i].swap(0, Ordering::Relaxed) as f64 / [1.0, 1e3, 1e3][i] / frames
            });
            if changes > 0.0 {
                tracing::info!(
                    "changes of title memory's mapping per frame: {changes:.0}, {waiting:.1} ms of them waiting for reads in place, \
                     {under_way:.1} ms under way (each holds {}; VICTORY_MAPPING_RANGES=0 holds all of title memory)",
                    if crate::watch::by_range() { "the bytes it changes" } else { "all of title memory" }
                );
            }
        }
        {
            let c = crate::watch::REMAP_SOURCES.each_ref().map(|a| a.swap(0, Ordering::Relaxed) as f64 / frames);
            if c.iter().any(|&v| v > 0.0) {
                tracing::info!(
                    "page mapping per frame (VICTORY_PAGEMAP_RUNS={}): Scatter {:.1} calls of {:.0} pages in {:.0} groups;                      single maps {:.1} calls of {:.0} pages; page-mapping batches {:.1} of {:.0} pages ({:.0} of one page                      repeated) as {:.0} operations, {:.0} of their pages in GPU-only reservations; views mapped {:.0},                      unmapped {:.0}; the GPU side told {:.0} times ({:.0} left out by batching); placeholder splits left                      out {:.0}; {:.2} ms in the mapper",
                    u8::from(crate::resource::page_batch_mapper().is_some()),
                    c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[14], c[7], c[8], c[9], c[10], c[11], c[15], c[12],
                    c[13] / 1e3,
                );
            }
            if crate::translate::lazy() {
                let l = crate::translate::LAZY.each_ref().map(|a| a.swap(0, Ordering::Relaxed) as f64 / frames);
                let calls = c[0] + c[3] + c[5];
                tracing::info!(
                    "lazy remap per frame (VICTORY_REMAP_LAZY=1): {:.1} calls, {:.0} pages put in the table, {:.0} taken out; \
                     views mapped lazily {:.1} ({:.1} on a CPU fault, {:.1} faults passed on to the guards), {:.1} unmapped \
                     at the end of a call, {:.1} placeholder splits left out; {} pages in the table, {} views awaiting unmap; \
                     {:.2} ms in the mapper",
                    calls, l[0], l[1], l[2], l[3], l[6], l[4], l[5],
                    crate::translate::live(),
                    crate::translate::pending(),
                    c[13] / 1e3,
                );
                let failed = crate::translate::FAILED.load(Ordering::Relaxed);
                let healed = crate::translate::HEALED.each_ref().map(|a| a.load(Ordering::Relaxed));
                if crate::translate::parking() || failed != 0 || healed.iter().any(|&h| h != 0) {
                    let k = crate::translate::PARK.each_ref().map(|a| a.swap(0, Ordering::Relaxed) as f64 / frames);
                    tracing::info!(
                        "parked views per frame (VICTORY_REMAP_PARK={}): {:.1} reused (same page, same address), {:.1}                          unmapped for another page, {:.1} parked; {} parked now; views that could not be mapped since the                          start {}; recovered in the fault handler since the start: {} mapped on a later try, {} views lost under                          an entry made anew, {} views committed on an access",
                        u8::from(crate::translate::parking()),
                        k[0], k[1], k[2],
                        crate::translate::parked(),
                        failed,
                        healed[0], healed[1], healed[2],
                    );
                }
            }
        }
        {
            let [copies, joined] = [0, 1].map(|i| COPIES_JOINED[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if copies > 0.0 {
                tracing::info!(
                    "title buffer copies recorded per frame: {copies:.0}, of them {joined:.0} continue the one before \
                     (VICTORY_JOIN_COPIES=1 makes those one copy with it: now {})",
                    if join_copies() { "on" } else { "off" }
                );
            }
        }
        // Passes over the heap's buffer views for shaders that pick them by index.
        let picked = crate::draw::HEAP_BUFFER_PASSES.swap(0, Ordering::Relaxed) as f64 / frames;
        let picked_ms =
            crate::draw::HEAP_BUFFER_MICROS.swap(0, Ordering::Relaxed) as f64 / 1e3 / frames;
        // Copies between depth textures and their colour twins, each way.
        let [twin_to_depth, twin_to_colour] =
            [2, 3].map(|i| crate::resource::TWINS[i].swap(0, Ordering::Relaxed) as f64 / frames);
        // Depth planes drawn from their twins.
        let planes = crate::resource::PLANES[0].swap(0, Ordering::Relaxed) as f64 / frames;
        // Copies in the card's memory (`crate::shadow::cards`).
        let card = crate::shadow::card_stats();
        #[cfg(feature = "diagnostics")]
        let slow = crate::watch::slow_asks();
        // Over the same frames as the counts above.
        #[cfg(feature = "diagnostics")]
        let drops = crate::pipeline::take_drops();
        if self.logged.elapsed() >= std::time::Duration::from_secs(5) {
            self.logged = std::time::Instant::now();
            crate::watch::pool_line();
            tracing::info!(
                "placed texture views: {} writes of another view of the texture a slot showed (it kept the old view until the new was made)",
                crate::descriptor::host::KEPT_VIEWS.load(Ordering::Relaxed)
            );
            tracing::info!(
                "copies left out since the start because both sides were one host texture: {}",
                crate::list::SELF_COPIES.load(Ordering::Relaxed)
            );
            if crate::drvwatch::errors() > 0 {
                tracing::warn!(
                    "errors the graphics driver reported to the system since the start: {} (the paths under suspicion are off)",
                    crate::drvwatch::errors()
                );
            }
            crate::tiles::status();
            crate::memopt::line(frames);
            {
                let [hb, hc, tb, tc] =
                    [0, 1, 2, 3].map(|i| COPY_MOVED[i].swap(0, Ordering::Relaxed) as f64 / frames);
                if hc + tc > 0.0 {
                    tracing::info!(
                        "copies moved per frame: {:.1} MB into host copies in {:.0} calls ({:.0} KB a call), \
                         {:.1} MB into title memory in {:.0} calls ({:.0} KB a call)",
                        hb / 1e6,
                        hc,
                        if hc > 0.0 { hb / hc / 1e3 } else { 0.0 },
                        tb / 1e6,
                        tc,
                        if tc > 0.0 { tb / tc / 1e3 } else { 0.0 }
                    );
                }
            }
            {
                let [queued, due, away, partly, oldest] =
                    [0, 1, 2, 3, 4].map(|i| crate::draw::HEAP_TEXTURES_QUEUE[i].load(Ordering::Relaxed));
                let [same, parts, tails] =
                    [0, 1, 2].map(|i| crate::draw::HEAP_TEXTURES_PARTLY[i].load(Ordering::Relaxed));
                tracing::info!(
                    "textures picked by index, the queue: {queued} slots, {due} of them due (the oldest written {oldest} frames ago), \
                     {away} waiting for their memory, {partly} uploaded in part; since the start: {} slots found uploaded already by \
                     the draws that read them through tables, {} writes of tables' slots (due some frames later), {} slots taken out \
                     as read through a table, {parts} uploads of textures not all mapped ({tails} of them without their first page), \
                     {same} looks that found no more mapped; {} slots made stale by a newer view of other memory there, \
                     {} ranges of memory mapped, unmapped or written under texture views ({} dropped), {} slots made due by them, \
                     {} made slots found unmapped and made null",
                    crate::draw::HEAP_TEXTURES_TABLED.load(Ordering::Relaxed),
                    crate::descriptor::host::TABLED_WRITES[0].load(Ordering::Relaxed),
                    crate::descriptor::host::TABLED_WRITES[1].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[0].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[1].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[2].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[3].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[4].load(Ordering::Relaxed)
                );
                // The queue's fairness and pace, over the frames since this
                // line was last written (five seconds).
                {
                    static FRAME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    let now = crate::frame::current();
                    let frames = now.saturating_sub(FRAME.swap(now, Ordering::Relaxed)).max(1) as f64;
                    let [fresh, fresh_waited, short, short_waited, part_frames] =
                        [5, 6, 7, 8, 9].map(|i| crate::draw::HEAP_TEXTURES_QUEUE[i].load(Ordering::Relaxed));
                    let [bytes, uploads, looked, micros, passes, by_time, by_bytes, by_room] =
                        [0, 1, 2, 3, 4, 5, 6, 7].map(|i| crate::draw::HEAP_TEXTURE_PASS[i].swap(0, Ordering::Relaxed));
                    tracing::info!(
                        "texture queue: {due} textures due ({fresh} never looked at, the oldest waiting {fresh_waited} frames; \
                         {short} showing less than the title holds, the oldest waiting {short_waited} frames), \
                         {partly} uploaded in part (the longest for {part_frames} frames); per frame over {frames:.0} frames: \
                         {:.0} bytes uploaded ({:.2} MB) in {:.1} uploads, {:.0} slots looked at, {:.1} ms in {:.2} passes; \
                         {by_time} of {passes} passes stopped at their time, {by_bytes} at the frame's bytes, {by_room} at a \
                         full card (VICTORY_FAIR_DUE={}, VICTORY_TEX_SPAN_PCT, VICTORY_RELOOK_MS)",
                        bytes as f64 / frames,
                        bytes as f64 / frames / 1e6,
                        uploads as f64 / frames,
                        looked as f64 / frames,
                        micros as f64 / 1e3 / frames,
                        passes as f64 / frames,
                        crate::descriptor::host::fair_due()
                    );
                }
                let [views, shown, waiting, tables, gone, own, free, start, other, shows, back] =
                    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10].map(|i: usize| crate::descriptor::host::SWEPT[i].load(Ordering::Relaxed));
                tracing::info!(
                    "placed texture views in the heap at the last look: {views}, of them {shown} shown, {waiting} in the queue, \
                     {tables} read through tables, {gone} without memory; with memory and nothing to bring them up to date: \
                     {own} of their own memory, {free} of memory no other view describes, {start} of memory another texture \
                     begins at, {other} of another's memory ({shows} of all these show another view); {back} put back into the \
                     queue since the start"
                );
            }
            {
                let mode = crate::watch::page_writes();
                let [writes, bytes, guarded, plain] = [0, 1, 2, 3].map(|i| crate::watch::WRITTEN[i].load(Ordering::Relaxed));
                let [kept, dropped, slots, uploaded] =
                    [0, 1, 2, 3].map(|i| crate::descriptor::host::PAGE_WRITES[i].load(Ordering::Relaxed));
                tracing::info!(
                    "writes behind the guards, since the start: {writes} DirectStorage writes into physical pages at addresses the                      title had mapped ({} MB); {guarded} guarded 64 KiB blocks of host copies lay under them ({}); {plain} reads to                      plain addresses finished ({}); {kept} ranges under texture views kept for the texture pass ({dropped} dropped);                      {slots} slots of placed textures held their bytes, {uploaded} of them uploaded before the write ({})                      (VICTORY_PAGE_WRITES: unset or guards opens the guarded blocks, 0 counts only, 1, slots, all)",
                    bytes >> 20,
                    if mode & 1 != 0 { "opened, taken again" } else { "left as they were" },
                    if mode & 4 != 0 { "told as well" } else { "counted only" },
                    if mode & 2 != 0 { "made due again" } else { "left as they were" }
                );
            }
            #[cfg(feature = "diagnostics")]
            tracing::info!(
                "write-watch askings of a millisecond or more: {:.1} a frame, {:.1} ms a frame; the longest asking {:.1} ms",
                slow[0] as f64 / frames,
                slow[1] as f64 / 1e3 / frames,
                slow[2] as f64 / 1e3
            );
            if crate::shadow::cards() {
                let per_frame = |i: usize| card[i] as f64 / frames;
                tracing::info!(
                    "host copies in the card's memory: {} copies {} MB, their bytes from before {} MB; per frame: \
                     {:.1} MB sent up in {:.0} ranges, {:.0} pieces compared on the card, {:.0} of them fetched \
                     ({:.1} MB) with {:.1} more waits for the card ({:.1} ms)",
                    card[7],
                    card[8] >> 20,
                    card[9] >> 20,
                    per_frame(0) / 1e6,
                    per_frame(1),
                    per_frame(2),
                    per_frame(3),
                    per_frame(4) / 1e6,
                    per_frame(5),
                    per_frame(6) / 1e3
                );
            }
            tracing::info!(
                "{fps:.1} fps, {ms:.1} ms/frame ({cpu:.1} preparing: copies {copy_ms:.1} ms \
                 {copy_mb:.1} MB, textures {texture_ms:.1} ms {texture_mb:.1} MB; {gpu:.1} on the \
                 GPU; {back_ms:.1} copying back {back_mb:.1} MB), {draws:.0} draws/frame, \
                 dropped {dropped:.0} without a host pipeline ({builder_off:.0} of them the ray-tracing structure builder's, which is off; {making} being made) and \
                 {indirect:.0} indirect; heap buffers {picked:.1} passes {picked_ms:.1} ms;                  texture slots written so far: {placed} placed, {objects} of objects; \
                 textures picked by index: {picked_textures} uploaded so far ({picked_mb} MB), \
                 {picked_waiting} waiting, {picked_away} passed over unmapped; depth textures with                  unordered access: {du0} made, bound as depth {du1}x, cleared as depth {du2}x,                  unordered views refused {du3}x; buffer views left null as the host would refuse them: {refused}",
                picked_textures = crate::draw::HEAP_TEXTURES.load(Ordering::Relaxed),
                picked_mb = crate::draw::HEAP_TEXTURE_BYTES.load(Ordering::Relaxed) >> 20,
                picked_waiting = crate::draw::HEAP_TEXTURES_WAITING.load(Ordering::Relaxed),
                picked_away = crate::draw::HEAP_TEXTURES_AWAY.load(Ordering::Relaxed),
                du0 = crate::resource::DEPTH_UNORDERED[0].load(Ordering::Relaxed),
                du1 = crate::resource::DEPTH_UNORDERED[1].load(Ordering::Relaxed),
                du2 = crate::resource::DEPTH_UNORDERED[2].load(Ordering::Relaxed),
                du3 = crate::resource::DEPTH_UNORDERED[3].load(Ordering::Relaxed),
                refused = format!(
                    "{}; depth twins: {} alive, {} depth textures left without one; twin copies per frame: {twin_to_depth:.1} into depth textures, {twin_to_colour:.1} into twins; depth planes: {planes:.1} drawn from their twins per frame, {} times not given back to a twin, {} times used before the pass",
                    crate::descriptor::host::REFUSED_VIEWS.load(Ordering::Relaxed),
                    crate::resource::TWINS[0].load(Ordering::Relaxed),
                    crate::resource::TWINS[1].load(Ordering::Relaxed),
                    crate::resource::PLANES[1].load(Ordering::Relaxed),
                    crate::resource::PLANES[2].load(Ordering::Relaxed)
                ),
                making = crate::pipeline::making(),
                placed = crate::descriptor::host::PLACED_TEXTURE_SLOTS.load(Ordering::Relaxed),
                objects = crate::descriptor::host::OBJECT_TEXTURE_SLOTS.load(Ordering::Relaxed)
            );
            crate::memcopy::log(frames);
            #[cfg(feature = "diagnostics")]
            {
                crate::copystat::log(std::mem::take(&mut self.copy_stats_frames) as f64);
                for (why, n) in drops.iter().take(40) {
                    tracing::info!(
                        "dropped {:.1}/frame without a host pipeline: {why}",
                        *n as f64 / frames
                    );
                }
            }
        }
        if self.caption.is_none() {
            // The window's own text, read without a message.
            let mut buffer = vec![0u16; 512];
            // SAFETY: a window handle and a buffer of that length.
            let n = unsafe { InternalGetWindowText(hwnd, &mut buffer) };
            buffer.truncate(n.max(0) as usize);
            self.caption = Some(buffer);
        }
        let Some(caption) = &self.caption else { return };
        let text = format!(" | VIctory {fps:.0} fps {ms:.1} ms/frame {draws:.0} draws/frame");
        let mut text: Vec<u16> = caption.iter().copied().chain(text.encode_utf16()).collect();
        text.push(0);
        // Setting the text waits for the window's thread (a title thread,
        // which may not pump for a while), so a thread of ours does it and
        // presenting never waits; it takes the newest text when free.
        static PENDING: Mutex<Option<(usize, Vec<u16>)>> = Mutex::new(None);
        static SETTER: std::sync::Once = std::sync::Once::new();
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((hwnd.0 as usize, text));
        SETTER.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("victory-caption".into())
                .spawn(|| {
                    loop {
                        let next = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
                        match next {
                            // SAFETY: a window handle and a NUL-terminated text.
                            Some((hwnd, text)) => unsafe {
                                let _ = SetWindowTextW(HWND(hwnd as *mut _), PCWSTR(text.as_ptr()));
                            },
                            None => std::thread::sleep(std::time::Duration::from_millis(100)),
                        }
                    }
                });
        });
    }
}

impl Host {
    /// Shows `texture`, which is in `state` (a host state) and stays in it,
    /// as frame `frame` (saved if captures are on). `None` if nothing was
    /// shown; else, in diagnostics builds, whether the frame was black.
    pub(crate) fn present(
        &self,
        texture: &ID3D12Resource,
        state: u32,
        frame: u64,
        over: Option<(&ID3D12Resource, u32)>,
    ) -> Option<bool> {
        let showing = std::time::Instant::now();
        let shown = self.present_now(texture, state, frame, over);
        crate::framelog::add(crate::framelog::T::PresentShowUs, showing.elapsed().as_micros() as u64);
        shown
    }

    fn present_now(
        &self,
        texture: &ID3D12Resource,
        state: u32,
        frame: u64,
        over: Option<(&ID3D12Resource, u32)>,
    ) -> Option<bool> {
        // SAFETY: a live resource.
        let desc = unsafe { texture.GetDesc() };
        let Some(format) = swap_format(desc.Format) else {
            tracing::warn!(format = desc.Format.0, "cannot present this format");
            return None;
        };
        let (width, height) = (desc.Width as u32, desc.Height);
        let mut presenter = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        if presenter.is_none() {
            *presenter = match self.presenter(width, height, format) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "no swap chain");
                    return None;
                }
            };
        }
        let p = presenter.as_mut()?;
        if (p.width, p.height, p.format) != (width, height, format) {
            // Presents sent ahead hold back buffers until they have run.
            settle_now(WHY_SYNC);
            // SAFETY: no back buffer is held; every submission was waited for.
            let resized = unsafe {
                p.swap_chain
                    .ResizeBuffers(BUFFERS, width, height, format, swap_flags(p.pace.tearing))
            };
            match resized {
                Ok(()) => {
                    (p.width, p.height, p.format) = (width, height, format);
                    tracing::info!(
                        width,
                        height,
                        format = format.0,
                        frame,
                        "the swap chain's buffers were made again for another size or format"
                    );
                    self.log_video_memory("right after the swap chain's buffers were made again");
                }
                Err(e) => {
                    tracing::warn!(error = %e, width, height, "swap chain not resized");
                    return None;
                }
            }
        }
        let state = D3D12_RESOURCE_STATES(state as i32);
        p.pace.wait_for_room();
        // SAFETY: our swap chain, list and a live source texture.
        unsafe {
            let index = p.swap_chain.GetCurrentBackBufferIndex();
            let Ok(back) = p.swap_chain.GetBuffer::<ID3D12Resource>(index) else {
                return None;
            };
            // Sent ahead ([`late_free_present`]) unless the frame is to be
            // read back at once (a saved or listed frame): then a list of
            // its own, one whose last flight has finished or a new one.
            #[cfg(feature = "diagnostics")]
            let read_now = crate::capture::wanted(frame) || crate::capture::listed(frame);
            #[cfg(not(feature = "diagnostics"))]
            let read_now = false;
            let slot = if late_free_present() && !read_now {
                let finished = FINISHED.load(Ordering::Acquire);
                p.ahead.iter().position(|(_, flown)| *flown <= finished).or_else(|| {
                    let list = self.new_list().ok()?;
                    list.close();
                    p.ahead.push((list, 0));
                    Some(p.ahead.len() - 1)
                })
            } else {
                None
            };
            let copy = match slot {
                Some(i) => &p.ahead[i].0,
                None => &p.copy,
            };
            copy.reset();
            let list = &copy.list;
            list.ResourceBarrier(&[
                transition(texture, state, D3D12_RESOURCE_STATE_COPY_SOURCE),
                transition(
                    &back,
                    D3D12_RESOURCE_STATE_PRESENT,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                ),
            ]);
            list.CopyResource(&back, texture);
            #[cfg(feature = "diagnostics")]
            let capture = crate::capture::wanted(frame)
                .then(|| crate::capture::read_back(&self.device, list, texture))
                .flatten();
            #[cfg(feature = "diagnostics")]
            let probe = if slot.is_some() { None } else { crate::capture::probe(&self.device, list, texture) };
            #[cfg(not(feature = "diagnostics"))]
            let _ = frame;
            // The present's second plane, if it has one, goes over the copy
            // ([`crate::over`]): the back buffer is a target meanwhile.
            let readable = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
            let over = over.and_then(|(plane, was)| {
                // SAFETY: a live resource.
                let format = swap_format(plane.GetDesc().Format)?;
                Some((plane, D3D12_RESOURCE_STATES(was as i32), format))
            });
            let mut drawn = false;
            if let Some((plane, was, plane_format)) = over {
                let mut barriers = vec![
                    transition(texture, D3D12_RESOURCE_STATE_COPY_SOURCE, state),
                    transition(
                        &back,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                    ),
                ];
                let moved = was.0 & readable.0 == 0;
                if moved {
                    barriers.push(transition(plane, was, readable));
                }
                list.ResourceBarrier(&barriers);
                drawn = crate::over::record(
                    &self.device,
                    list,
                    &back,
                    format,
                    (width, height),
                    plane,
                    plane_format,
                );
                let mut barriers = vec![transition(
                    &back,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    D3D12_RESOURCE_STATE_PRESENT,
                )];
                if moved {
                    barriers.push(transition(plane, readable, was));
                }
                list.ResourceBarrier(&barriers);
            } else {
                list.ResourceBarrier(&[
                    transition(texture, D3D12_RESOURCE_STATE_COPY_SOURCE, state),
                    transition(
                        &back,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        D3D12_RESOURCE_STATE_PRESENT,
                    ),
                ]);
            }
            let _ = drawn;
            // The boot splash fades once the title shows content. What is judged is the
            // composited back buffer (the game plane with the interface plane over it: menus
            // and loading screens are on the interface plane, the game plane is black there):
            // a few texels of it, every few presents, until the splash is gone (then nothing
            // is done here).
            let mut sampled = false;
            if crate::splash::active() {
                if p.sampler.is_none() {
                    p.sampler = crate::splash::Sampler::new(&self.device);
                }
                if let Some(s) = p.sampler.as_mut() {
                    if let Some(luma) = s.harvest(FINISHED.load(Ordering::Acquire)) {
                        crate::splash::note_luma(luma);
                    }
                    if s.idle() && crate::splash::note_present() {
                        list.ResourceBarrier(&[transition(
                            &back,
                            D3D12_RESOURCE_STATE_PRESENT,
                            D3D12_RESOURCE_STATE_COPY_SOURCE,
                        )]);
                        s.record(list, &back, format, format);
                        list.ResourceBarrier(&[transition(
                            &back,
                            D3D12_RESOURCE_STATE_COPY_SOURCE,
                            D3D12_RESOURCE_STATE_PRESENT,
                        )]);
                        sampled = true;
                    }
                }
            } else if let Some(sampler) = p.sampler.take() {
                // A present sent ahead ([`late_free_present`]) may still copy into it.
                retire(sampler);
            }
            // The splash picture over the frame, at its opacity, after the sample above (which
            // judges the title's own content).
            crate::splash::mark_presented();
            let alpha = crate::splash::alpha();
            if alpha > 0.0 {
                let fresh = p.splash.is_none();
                if fresh {
                    p.splash = crate::splash::picture().and_then(|img| splash_texture(&self.device, &img).ok());
                }
                if let Some((picture, upload)) = p.splash.as_ref() {
                    if fresh {
                        // SAFETY: an open list; both resources are live and sized for the copy.
                        let d = picture.GetDesc();
                        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
                        self.device.GetCopyableFootprints(&d, 0, 1, 0, Some(&mut footprint), None, None, None);
                        list.CopyTextureRegion(
                            &D3D12_TEXTURE_COPY_LOCATION {
                                pResource: std::mem::transmute_copy(picture),
                                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
                            },
                            0,
                            0,
                            0,
                            &D3D12_TEXTURE_COPY_LOCATION {
                                pResource: std::mem::transmute_copy(upload),
                                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
                            },
                            None,
                        );
                        list.ResourceBarrier(&[transition(
                            picture,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                        )]);
                    }
                    list.ResourceBarrier(&[transition(
                        &back,
                        D3D12_RESOURCE_STATE_PRESENT,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                    )]);
                    crate::over::record_faded(
                        &self.device,
                        list,
                        &back,
                        format,
                        (width, height),
                        picture,
                        DXGI_FORMAT_B8G8R8A8_UNORM,
                        alpha,
                    );
                    list.ResourceBarrier(&[transition(
                        &back,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                        D3D12_RESOURCE_STATE_PRESENT,
                    )]);
                }
            } else if !crate::splash::active()
                && let Some(splash) = p.splash.take()
            {
                // A present sent ahead ([`late_free_present`]) may still draw it: freed at
                // once, the draw read freed memory (a GPU page fault, device removed).
                retire(splash);
            }
            // The settings overlay, over everything (F1 opens it; closed, it costs a key query).
            let menu = crate::overlay::record(
                &self.device,
                list,
                &back,
                format,
                (width, height),
                p.hwnd,
                FINISHED.load(Ordering::Acquire),
                &|| self.video_memory(),
            );
            copy.close();
            match slot {
                Some(i) => {
                    let flown = self.execute_own(copy);
                    p.ahead[i].1 = flown;
                    if let Some(m) = menu {
                        crate::overlay::sent(m, flown);
                    }
                    if sampled {
                        if let Some(s) = p.sampler.as_mut() {
                            s.sent(flown);
                        }
                    }
                }
                None => {
                    self.execute(&[copy]);
                    if let Some(m) = menu {
                        // Not told which submission it was: the newest started.
                        crate::overlay::sent(m, submissions().0);
                    }
                    if sampled {
                        if let Some(s) = p.sampler.as_mut() {
                            s.sent(0);
                        }
                    }
                }
            }
            #[cfg(feature = "diagnostics")]
            if let Some((buffer, footprint)) = capture {
                crate::capture::save_read_back(frame, &buffer, &footprint);
            }
            p.pace.hold();
            let (sync, flags) = p.pace.present_args();
            let _ = p.swap_chain.Present(sync, flags);
            p.pace.presented(&p.swap_chain);
            p.meter.frame(p.hwnd);
            #[cfg(feature = "diagnostics")]
            return Some(probe.is_some_and(|b| crate::capture::black(&b)));
            #[cfg(not(feature = "diagnostics"))]
            Some(false)
        }
    }

    fn presenter(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<Presenter> {
        let hwnd = match title_window() {
            Some(hwnd) => hwnd,
            None => window(width, height)?,
        };
        self.log_video_memory("right before the swap chain is made");
        // SAFETY: plain object creation for our window.
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let tearing = victory_core::pace::switches().tearing && tearing_supported(&factory);
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: BUFFERS,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                Flags: swap_flags(tearing).0 as u32,
                ..Default::default()
            };
            // `VICTORY_FRAMEGEN=1`: the FidelityFX frame-generation swap
            // chain in place of a plain one (`crate::fsr`).
            let swap_chain = match crate::fsr::framegen_swap_chain(&factory, &self.queue, hwnd, &desc) {
                Some(chain) => chain,
                None => factory
                    .CreateSwapChainForHwnd(&self.queue, hwnd, &desc, None, None)?
                    .cast::<IDXGISwapChain3>()?,
            };
            let pace = Pace::new(&swap_chain, tearing);
            let copy = self.new_list()?;
            copy.close();
            tracing::info!(width, height, format = format.0, "presenting to a window");
            self.log_video_memory("right after the swap chain is made");
            Ok(Presenter {
                swap_chain,
                width,
                height,
                format,
                copy,
                ahead: Vec::new(),
                hwnd,
                meter: Meter::new(),
                sampler: None,
                splash: None,
                pace,
            })
        }
    }
}

/// The splash picture as a texture in the copy-destination state, with an upload buffer holding
/// its texels (the caller records the copy).
fn splash_texture(
    device: &ID3D12Device,
    img: &crate::splash::Image,
) -> windows::core::Result<(ID3D12Resource, ID3D12Resource)> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(img.w),
        Height: img.h,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        ..Default::default()
    };
    // SAFETY: plain creation; the upload buffer is mapped and written inside its size.
    unsafe {
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut total = 0u64;
        device.GetCopyableFootprints(&desc, 0, 1, 0, Some(&mut footprint), None, None, Some(&mut total));
        let mut texture: Option<ID3D12Resource> = None;
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
            &mut texture,
        )?;
        let mut upload: Option<ID3D12Resource> = None;
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_UPLOAD, ..Default::default() },
            D3D12_HEAP_FLAG_NONE,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: total,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            },
            D3D12_RESOURCE_STATE_GENERIC_READ,
            None,
            &mut upload,
        )?;
        let (texture, upload) = (texture.ok_or_else(windows::core::Error::empty)?, upload.ok_or_else(windows::core::Error::empty)?);
        let mut at: *mut std::ffi::c_void = std::ptr::null_mut();
        upload.Map(0, None, Some(&mut at))?;
        let pitch = footprint.Footprint.RowPitch as usize;
        let row = img.w as usize * 4;
        for y in 0..img.h as usize {
            let dst = (at as *mut u8).add(footprint.Offset as usize + y * pitch);
            // opaque: the picture covers what is under it as far as the opacity says
            for (x, px) in img.bgra[y * row..(y + 1) * row].chunks_exact(4).enumerate() {
                dst.add(x * 4).copy_from_nonoverlapping([px[0], px[1], px[2], 255].as_ptr(), 4);
            }
        }
        upload.Unmap(0, None);
        Ok((texture, upload))
    }
}

/// A visible top-level window of this process: the one the title made, if
/// it made one.
fn title_window() -> Option<HWND> {
    extern "system" fn visit(hwnd: HWND, found: LPARAM) -> windows::core::BOOL {
        let mut process = 0;
        // SAFETY: a window handle from the enumeration; `found` is ours.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process));
            if process == windows::Win32::System::Threading::GetCurrentProcessId()
                && !crate::splash::is_splash(hwnd)
                && IsWindowVisible(hwnd).as_bool()
            {
                *(found.0 as *mut Option<HWND>) = Some(hwnd);
                return false.into();
            }
        }
        true.into()
    }
    let mut found: Option<HWND> = None;
    // SAFETY: the callback only writes `found`, which outlives the call.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&raw mut found as isize)) };
    found
}

/// `state` without the bits the host texture's flags forbid: the console
/// lets the title name states its own resources allow (unordered access on
/// a depth target, whose host copy cannot have both).
pub(crate) fn legal_state(texture: &ID3D12Resource, state: u32) -> u32 {
    // SAFETY: a plain query.
    legal_for(unsafe { texture.GetDesc() }.Flags.0 as u32, state)
}

/// [`legal_state`] for a texture with resource flags `flags`.
pub(crate) fn legal_for(flags: u32, state: u32) -> u32 {
    // (flag, set or clear forbids, state bits): render target, unordered
    // access, depth, denied shader resource.
    const FORBIDDEN: [(u32, bool, u32); 4] = [
        (0x1, false, 0x4),
        (0x4, false, 0x8),
        (0x2, false, 0x10 | 0x20),
        (0x8, true, 0x40 | 0x80),
    ];
    FORBIDDEN
        .iter()
        .filter(|&&(flag, deny, _)| (flags & flag != 0) == deny)
        .fold(state, |s, &(_, _, bits)| s & !bits)
}

impl Host {
    /// The card's memory as the system accounts it to this process, in
    /// bytes: the budget and the use of the card's own memory, then of the
    /// PC's memory shared with the card (where the system moves what does
    /// not fit on the card).
    /// Registers `event` for the system's budget-change notification.
    pub(crate) fn budget_notification(&self, event: HANDLE) -> Option<u32> {
        // SAFETY: a live event handle kept for the run.
        unsafe { self.adapter.as_ref()?.RegisterVideoMemoryBudgetChangeNotificationEvent(event).ok() }
    }

    pub(crate) fn video_memory(&self) -> Option<[u64; 4]> {
        let adapter = self.adapter.as_ref()?;
        let mut own = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let mut shared = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: plain queries that fill our structures.
        unsafe {
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut own)
                .ok()?;
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut shared)
                .ok()?;
        }
        // The card's budget moved by 256 MB or more since the last look:
        // said at once, with what was in use at the last look, so the log
        // has the moment. (It fell from 11,242 MB to some 10,300 MB with
        // the first world frame in runs 179 to 185, and the memory line,
        // seconds later, was the first to show it. We reserve nothing
        // and make the swap chain once, at the start.)
        static LAST: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];
        let now = [own.Budget, own.CurrentUsage, shared.CurrentUsage];
        let last = [0, 1, 2].map(|i| LAST[i].swap(now[i], Ordering::Relaxed));
        if last[0] != 0 && last[0].abs_diff(own.Budget) >= (256u64 << 20) {
            tracing::info!(
                "the system changed the budget of the card's memory from {} MB to {} MB (frame {}); at the last look {} MB of the card's and {} MB of the PC's shared memory were in use",
                last[0] >> 20,
                own.Budget >> 20,
                crate::frame::current(),
                last[1] >> 20,
                last[2] >> 20
            );
            self.log_video_memory("right after that change");
        }
        Some([own.Budget, own.CurrentUsage, shared.Budget, shared.CurrentUsage])
    }

    /// Both of the system's memory groups for this process, in bytes: for
    /// the card's own memory and then for the PC's memory shared with the
    /// card, the budget, the use, what could still be reserved and what
    /// is reserved (the layer reserves none).
    pub(crate) fn video_memory_groups(&self) -> Option<[[u64; 4]; 2]> {
        let adapter = self.adapter.as_ref()?;
        let mut own = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let mut shared = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: plain queries that fill our structures.
        unsafe {
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut own)
                .ok()?;
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut shared)
                .ok()?;
        }
        let all = |m: &DXGI_QUERY_VIDEO_MEMORY_INFO| {
            [m.Budget, m.CurrentUsage, m.AvailableForReservation, m.CurrentReservation]
        };
        Some([all(&own), all(&shared)])
    }

    /// Logs [`Self::video_memory_groups`], with `when` it is said.
    pub(crate) fn log_video_memory(&self, when: &str) {
        let Some([own, shared]) = self.video_memory_groups() else {
            return;
        };
        tracing::info!(
            "video memory as the system counts it, {}: the card's own: budget {} MB, in use {} MB, {} MB could still be reserved, {} MB reserved; the PC's shared with the card: budget {} MB, in use {} MB, {} MB could still be reserved, {} MB reserved (the layer reserves none)",
            when,
            own[0] >> 20,
            own[1] >> 20,
            own[2] >> 20,
            own[3] >> 20,
            shared[0] >> 20,
            shared[1] >> 20,
            shared[2] >> 20,
            shared[3] >> 20
        );
    }

    /// A buffer in system memory that the CPU maps (write-back, so reading
    /// it is fast too) and shaders read and write: for host copies of title
    /// memory that shaders write, which upload memory cannot be. Starts in
    /// the common state (buffers move out of it implicitly).
    pub(crate) fn shared_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        crate::census::made(2, size);
        MADE[2].fetch_add(1, Ordering::Relaxed);
        MADE[3].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of a CPU-visible buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no shared buffer");
                None
            }
        }
    }

    /// A buffer in the card's own memory that the CPU maps (write-combined:
    /// writing it is fast, reading it is slow) and shaders read and write:
    /// D3D12's GPU upload heap, there with Resizable BAR
    /// ([`crate::rebar`]). Starts in the common state.
    pub(crate) fn gpu_upload_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_GPU_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of a CPU-visible buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no buffer in the GPU upload heap");
                None
            }
        }
    }

    /// A device-local buffer shaders may write (unordered access) and indirect
    /// execution may read, starting in the common state.
    pub(crate) fn uav_buffer(&self, size: u64) -> Option<ID3D12Resource> {
        crate::census::made(3, size);
        MADE[4].fetch_add(1, Ordering::Relaxed);
        MADE[5].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a device-local buffer.
        let made = unsafe {
            self.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut buffer,
            )
        };
        match made {
            Ok(()) => buffer,
            Err(e) => {
                tracing::warn!(error = %e, size, "no unordered-access buffer");
                None
            }
        }
    }
}

/// For adapters without device-removed data (the software one): the last
/// commands each list of the failed submission noted (diagnostics, with
/// `VICTORY_D3D12_DRED`).
fn last_notes(runs: &[&HostList]) -> String {
    #[cfg(feature = "diagnostics")]
    {
        let mut text = String::new();
        for (i, r) in runs.iter().enumerate() {
            let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
            for (n, _) in notes.iter().rev().take(6).rev() {
                text.push_str(&format!("; list {i} of {}, of its {}: {n}", runs.len(), notes.len()));
            }
        }
        text
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        let _ = runs;
        String::new()
    }
}

/// Where the faulting address `va` lies against a range `(start, bytes)` a
/// command reads (diagnostics).
#[cfg(feature = "diagnostics")]
fn fault_in(va: u64, (start, bytes): (u64, u64)) -> String {
    let end = start + bytes;
    if va < start {
        format!(" (fault {:#x} below its source)", start - va)
    } else if va < end {
        format!(" (fault at source +{:#x})", va - start)
    } else {
        format!(" (fault {:#x} past its source's end)", va - end)
    }
}

/// The commands that may have been running where a list stopped at
/// command `done`: those between the barriers around it (work between
/// barriers overlaps on the GPU), by kind, and what the markers just
/// before them say (`HostList::note_op`: diagnostics builds with
/// device-removed data on).
fn in_flight(history: &[D3D12_AUTO_BREADCRUMB_OP], markers: &[(u32, String)], done: usize) -> String {
    let barrier = |op: &D3D12_AUTO_BREADCRUMB_OP| *op == D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER;
    let from = history[..done].iter().rposition(barrier).map_or(0, |b| b + 1);
    let to = history[done..]
        .iter()
        .position(barrier)
        .map_or(history.len(), |b| (done + b).max(done + 1));
    let mut text = format!(", commands {from}..{to} between its barriers:");
    let mut shown = 0;
    // From just before the command the GPU had not finished: the first
    // twelve after the barrier named draws long done when a list of 79
    // mesh draws stopped at its 45th (run 41).
    for i in from.max(done.saturating_sub(4))..to {
        let op = history[i];
        if op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER || shown == 12 {
            continue;
        }
        shown += 1;
        let marker = i
            .checked_sub(1)
            .filter(|&m| history[m] == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
            .and_then(|m| markers.iter().find(|(at, _)| *at as usize == m))
            .map_or("no marker", |(_, s)| s.as_str());
        text.push_str(&format!("; command {i} (op {}): {marker}", op.0));
    }
    text
}

pub(crate) fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a borrowed pointer; the barrier never releases it.
                pResource: unsafe { std::mem::transmute_copy(resource) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// The words of `barrier` as `D3D12_RESOURCE_BARRIER` lays them out (type
/// and flags; the first resource; subresource and state before, or the
/// second resource; state after), read field by field: the union's
/// smaller variants leave bytes unwritten. Resource pointers stay
/// borrowed.
pub(crate) fn barrier_words(barrier: &D3D12_RESOURCE_BARRIER) -> [u64; 4] {
    let head = u64::from(barrier.Type.0 as u32) | (u64::from(barrier.Flags.0 as u32) << 32);
    // SAFETY: the variant read is the one `Type` names; pointers are copied
    // as addresses, never released.
    unsafe {
        match barrier.Type {
            D3D12_RESOURCE_BARRIER_TYPE_TRANSITION => {
                let t = &barrier.Anonymous.Transition;
                [
                    head,
                    std::mem::transmute_copy::<_, usize>(&t.pResource) as u64,
                    u64::from(t.Subresource) | (u64::from(t.StateBefore.0 as u32) << 32),
                    u64::from(t.StateAfter.0 as u32),
                ]
            }
            D3D12_RESOURCE_BARRIER_TYPE_UAV => {
                let u = &barrier.Anonymous.UAV;
                [head, std::mem::transmute_copy::<_, usize>(&u.pResource) as u64, 0, 0]
            }
            _ => {
                let a = &barrier.Anonymous.Aliasing;
                [
                    head,
                    std::mem::transmute_copy::<_, usize>(&a.pResourceBefore) as u64,
                    std::mem::transmute_copy::<_, usize>(&a.pResourceAfter) as u64,
                    0,
                ]
            }
        }
    }
}

/// A window of the given client size, owned by its own message thread.
fn window(width: u32, height: u32) -> windows::core::Result<HWND> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("victory-window".into())
        .spawn(move || {
            let made = make_window(width, height);
            let ok = made.is_ok();
            let _ = sender.send(made.map(|h| h.0 as usize));
            if ok {
                pump();
            }
        })
        .map_err(|_| windows::core::Error::empty())?;
    let hwnd = receiver
        .recv()
        .map_err(|_| windows::core::Error::empty())??;
    Ok(HWND(hwnd as *mut _))
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the default handling for our window.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn make_window(width: u32, height: u32) -> windows::core::Result<HWND> {
    const CLASS: PCWSTR = w!("VIctory");
    // SAFETY: registers our class once and creates a window of it.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassExW(&class);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false)?;
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("VIctory"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn pump() {
    let mut message = MSG::default();
    // SAFETY: the standard message loop of this thread's window.
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[cfg(test)]
mod trail {
    /// The trail keeps the newest things, and the report made when the
    /// host device is removed says them and the pipeline library's state
    /// (no card is asked for any of it).
    #[test]
    fn the_removal_report_says_the_newest_things_done() {
        for n in 0..super::TRAIL_MOST + 5 {
            super::trail(|| format!("thing {n} done"));
        }
        let text = super::removal_context();
        assert!(text.contains("the submitting thread was"));
        assert!(text.contains("pipeline library: no call running"));
        assert!(text.contains(&format!("thing {} done", super::TRAIL_MOST + 4)));
        assert!(!text.contains("thing 4 done"), "the oldest entries are dropped");
    }
}

#[cfg(test)]
mod spare {
    use super::Spare;

    /// Kept things come back by size, the last given first, and nothing is
    /// kept past the limit.
    #[test]
    fn kept_things_come_back_by_size_within_the_limit() {
        let mut spare: Spare<u32> = Spare::new();
        assert_eq!(spare.take(4), None);
        assert_eq!(spare.give(1, 4, 10), None);
        assert_eq!(spare.give(2, 1, 10), None);
        assert_eq!(spare.give(3, 4, 10), None);
        assert_eq!(spare.bytes, 9);
        // Past the limit: handed back, nothing changes.
        assert_eq!(spare.give(4, 4, 10), Some(4));
        assert_eq!(spare.bytes, 9);
        assert_eq!(spare.take(2), None, "no thing of that size");
        assert_eq!(spare.take(4), Some(3));
        assert_eq!(spare.take(4), Some(1));
        assert_eq!(spare.take(4), None);
        assert_eq!(spare.take(1), Some(2));
        assert_eq!(spare.bytes, 0);
        assert!(spare.kept.is_empty());
    }
}

#[cfg(test)]
mod batch {   // Batch::texture_due lives here; `mod tests` is already taken at the top of this file
    use super::Batch;

    /// Marvisak #2428: ranges to copy coalesce within a page; distant
    /// ranges do not cover the span between them.
    #[test]
    fn ranges_of_a_copy_stay_disjoint() {
        use super::{COPY_GAP, covered, push_range};
        use crate::watch::PAGE;
        assert_eq!(COPY_GAP, PAGE);
        let mut rs = vec![];
        push_range(&mut rs, COPY_GAP, 0x1_0000, 0x1_1000);
        push_range(&mut rs, COPY_GAP, 0x40_0000, 0x40_1000);
        assert_eq!(rs, [(0x1_0000, 0x1_1000), (0x40_0000, 0x40_1000)]);
        assert!(covered(&rs, 0x1_0000, 0x1_1000));
        assert!(!covered(&rs, 0x1_0000, 0x2_0000));
        push_range(&mut rs, COPY_GAP, 0x1_1000 + PAGE, 0x1_2000 + PAGE);
        assert_eq!(rs, [(0x1_0000, 0x1_2000 + PAGE), (0x40_0000, 0x40_1000)]);
        push_range(&mut rs, COPY_GAP, 0x1_2000 + 2 * PAGE + 1, 0x1_2900 + 2 * PAGE + 1);
        assert_eq!(rs.len(), 3);
        push_range(&mut rs, COPY_GAP, 0x1_3000 + PAGE, 0x50_0000);
        assert_eq!(rs, [(0x1_0000, 0x50_0000)]);
        assert!(covered(&rs, 0x2_0000, 0x4f_0000));
    }

    /// Marvisak #2428: copied ranges merge only where they touch (pages 0,
    /// 2 and 1 flushed in that order must copy page 1).
    #[test]
    fn the_gap_between_copied_ranges_is_not_covered() {
        use super::{covered, push_range};
        let mut copied = vec![(0x1_0000, 0x1_1000)];
        push_range(&mut copied, 0, 0x2_1000, 0x2_2000);
        assert_eq!(copied, [(0x1_0000, 0x1_1000), (0x2_1000, 0x2_2000)]);
        assert!(!covered(&copied, 0x1_1000, 0x2_1000));
        push_range(&mut copied, 0, 0x2_1000, 0x2_2000);
        push_range(&mut copied, 0, 0x1_1000, 0x2_1000);
        assert_eq!(copied, [(0x1_0000, 0x2_2000)]);
        assert!(covered(&copied, 0x1_1000, 0x2_1000));
    }
    use std::sync::Arc;

    #[test]
    fn a_texture_is_checked_once_per_submission() {
        let mut batch = Batch::default();
        let (a, b) = (Arc::new([0u8; 16]), Arc::new([0u8; 16]));
        // The first list sampling each texture checks it ...
        assert!(batch.texture_due(&a));
        assert!(batch.texture_due(&b));
        // ... later lists of the same submission do not.
        assert!(!batch.texture_due(&a));
        assert!(!batch.texture_due(&a.clone()));
        assert!(!batch.texture_due(&b));
    }

    #[test]
    fn title_writes_make_textures_due_again() {
        let mut batch = Batch::default();
        let a = Arc::new(1u32);
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A copy into title memory or work that may write it may have
        // changed the texture's memory.
        batch.title_written();
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A fresh submission starts over.
        let mut next = Batch::default();
        assert!(next.texture_due(&a));
    }

    #[test]
    fn spare_most_scales_with_ram() {
        const GIB: u64 = 1 << 30;
        const MIB: u64 = 1 << 20;
        assert_eq!(super::spare_most(None), 128 * MIB);
        assert_eq!(super::spare_most(Some(8 * GIB)), 64 * MIB);
        assert_eq!(super::spare_most(Some(16 * GIB)), 64 * MIB);
        assert_eq!(super::spare_most(Some(24 * GIB)), 128 * MIB);
        assert_eq!(super::spare_most(Some(31 * GIB)), 256 * MIB);
        assert_eq!(super::spare_most(Some(32 * GIB)), 256 * MIB);
        assert_eq!(super::spare_most(Some(64 * GIB)), 256 * MIB);
    }

    #[cfg(feature = "diagnostics")]
    #[test]
    #[ignore = "requires an isolated process with VICTORY_D3D12_WARP=1"]
    fn disjoint_refresh_leaves_the_dirty_gap_pending() {
        use super::{IDXGIAdapter1, Interface, Ordering, PREPARING, host, warp_adapter};
        use crate::{shadow, watch::PAGE};
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
            VirtualFree,
        };

        assert!(std::env::var_os("VICTORY_D3D12_WARP").is_some());
        let host = host().expect("WARP host device");
        // SAFETY: the WARP adapter and the device are both live.
        let (adapter, device_luid) = unsafe {
            (
                warp_adapter().unwrap().cast::<IDXGIAdapter1>().unwrap().GetDesc1().unwrap(),
                host.device.GetAdapterLuid(),
            )
        };
        assert_eq!(device_luid.HighPart, adapter.AdapterLuid.HighPart);
        assert_eq!(device_luid.LowPart, adapter.AdapterLuid.LowPart);
        println!("WARP {:04x}:{:04x}", adapter.VendorId, adapter.DeviceId);

        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        const SIZE: usize = 3 * PAGE;
        // SAFETY: a new committed, write-watched allocation owned by the test.
        let title = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                SIZE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(title, 0);
        let fill_page = move |page: usize, value: u8| {
            assert!(page < 3);
            // SAFETY: the test retains the committed allocation until all lists finish.
            unsafe { ((title + page * PAGE) as *mut u8).write_bytes(value, PAGE) };
        };
        shadow::register(title, title as u64, SIZE as u64);
        let shadow = shadow::find(title as u64, SIZE as u64).expect("shared shadow buffer");

        let submit = |ranges: &[(usize, usize)], write: bool| {
            let list = host.new_list().unwrap();
            let readback = host.read_back_buffer(SIZE as u64).unwrap();
            let copied = || PREPARING[1].load(Ordering::Relaxed);
            let before = copied();
            for &(offset, len) in ranges {
                shadow.refresh(&list, (title + offset) as u64, len as u64);
            }
            if write {
                shadow.gpu_writes(&list, title as u64, 4);
                list.defer_host(move || {
                    assert_eq!(copied() - before, (2 * PAGE) as u64, "GPU write drains both ranges");
                });
                // A title write after the declaration makes the third page stale again.
                list.defer(move || fill_page(2, 66));
                shadow.refresh(&list, (title + 2 * PAGE) as u64, PAGE as u64);
            }
            // SAFETY: both buffers cover SIZE bytes and live through synchronous execution.
            unsafe { list.list.CopyBufferRegion(&readback, 0, &shadow.buffer, 0, SIZE as u64) };
            let mut data = vec![0u8; SIZE];
            list.defer_read_back(readback, data.as_mut_ptr(), [SIZE, 1, SIZE]);
            list.close();
            host.execute(&[&list]);
            let markers = std::array::from_fn(|page| {
                let bytes = &data[page * PAGE..(page + 1) * PAGE];
                assert!(bytes.iter().all(|&byte| byte == bytes[0]), "page {page} is uniform");
                bytes[0]
            });
            (copied() - before, markers)
        };

        for (page, value) in [1, 2, 3].into_iter().enumerate() {
            fill_page(page, value);
        }
        assert_eq!(submit(&[(0, SIZE)], false), (SIZE as u64, [1, 2, 3]));
        for (page, value) in [11, 22, 33].into_iter().enumerate() {
            fill_page(page, value);
        }
        let first = submit(&[(2 * PAGE, PAGE), (0, PAGE)], false);
        let gap = submit(&[(PAGE, PAGE)], false);

        fill_page(0, 44);
        let overlap = submit(&[(PAGE / 2, PAGE / 2), (0, 3 * PAGE / 4)], false);
        fill_page(0, 55);
        fill_page(2, 60);
        let write = submit(&[(0, PAGE), (2 * PAGE, PAGE)], true);

        drop(shadow);
        shadow::unregister(title, title as u64);
        // SAFETY: the allocation is no longer used by any list or shadow.
        assert_ne!(unsafe { VirtualFree(title as *mut _, 0, MEM_RELEASE) }, 0);

        println!("copied bytes and page contents: first {first:?}, gap {gap:?}, overlap {overlap:?}, write {write:?}");
        assert_eq!(overlap, (PAGE as u64, [44, 22, 33]));
        assert_eq!(write, (SIZE as u64, [55, 22, 66]));
        assert_eq!(first, ((2 * PAGE) as u64, [11, 2, 33]));
        assert_eq!(gap, (PAGE as u64, [11, 22, 33]));
    }

    /// The hashing a submission does for textures in physical-page memory,
    /// with every list checking the textures it samples (before) and with
    /// each checked once (after). Run with
    /// `cargo test --release -p victory-gpu -- --ignored --nocapture texture_checks`.
    #[test]
    #[ignore = "benchmark"]
    fn texture_checks_per_submission() {
        const TEXTURES: usize = 48;
        const SIZE: usize = 4 << 20;
        const LISTS: usize = 12;
        let textures: Vec<Arc<Vec<u8>>> = (0..TEXTURES)
            .map(|t| Arc::new((0..SIZE).map(|i| (i * 31 + t) as u8).collect()))
            .collect();
        let submit = |dedupe: bool| {
            let mut batch = Batch::default();
            let mut hashed = 0usize;
            let started = std::time::Instant::now();
            for _ in 0..LISTS {
                for t in &textures {
                    if !dedupe || batch.texture_due(t) {
                        std::hint::black_box(blake3::hash(t));
                        hashed += 1;
                    }
                }
            }
            (started.elapsed(), hashed)
        };
        submit(false); // warm up
        let (before, n_before) = submit(false);
        let (after, n_after) = submit(true);
        assert_eq!((n_before, n_after), (TEXTURES * LISTS, TEXTURES));
        println!(
            "{LISTS} lists sampling {TEXTURES} textures of {} MiB: \
             before {n_before} hashes in {before:?}, after {n_after} in {after:?} ({:.1}x)",
            SIZE >> 20,
            before.as_secs_f64() / after.as_secs_f64()
        );
    }

    #[test]
    fn vram_idle_ms_scales_with_pressure() {
        use super::vram_idle_ms;
        let gib = 1024 * 1024 * 1024;
        // Normal (< 75%)
        assert_eq!(vram_idle_ms(Some([10 * gib, 0, 0, 0])), 5_000);
        assert_eq!(vram_idle_ms(Some([10 * gib, 7 * gib, 0, 0])), 5_000);
        assert_eq!(vram_idle_ms(Some([100, 74, 0, 0])), 5_000);
        // Moderate (75%..85%)
        assert_eq!(vram_idle_ms(Some([100, 75, 0, 0])), 2_500);
        assert_eq!(vram_idle_ms(Some([100, 84, 0, 0])), 2_500);
        // High (>= 85%, including over-budget)
        assert_eq!(vram_idle_ms(Some([100, 85, 0, 0])), 1_000);
        assert_eq!(vram_idle_ms(Some([10 * gib, 12 * gib, 0, 0])), 1_000);
        // Missing or zero budget falls back to normal
        assert_eq!(vram_idle_ms(None), 5_000);
        assert_eq!(vram_idle_ms(Some([0, 100, 0, 0])), 5_000);
    }

    #[test]
    fn ram_idle_ms_scales_with_pressure() {
        use super::ram_idle_ms;
        let gib = 1024 * 1024 * 1024;
        // Normal (>= 4 GiB)
        assert_eq!(ram_idle_ms(Some(8 * gib)), 10_000);
        assert_eq!(ram_idle_ms(Some(4 * gib)), 10_000);
        // Moderate (2 GiB..4 GiB)
        assert_eq!(ram_idle_ms(Some(4 * gib - 1)), 5_000);
        assert_eq!(ram_idle_ms(Some(2 * gib)), 5_000);
        // High (< 2 GiB)
        assert_eq!(ram_idle_ms(Some(2 * gib - 1)), 2_000);
        assert_eq!(ram_idle_ms(Some(0)), 2_000);
        // Missing falls back to normal
        assert_eq!(ram_idle_ms(None), 10_000);
    }
}

#[cfg(test)]
mod title_memory_io {
    use super::{copy_mapped, write_mapped};
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    const PAGE: usize = 4096;

    /// Two committed pages followed by one reserved (not committed) page.
    fn region() -> *mut u8 {
        // SAFETY: a fresh reservation of ours, of which two pages are committed.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), 3 * PAGE, MEM_RESERVE, PAGE_READWRITE);
            assert!(!base.is_null());
            let committed = VirtualAlloc(base, 2 * PAGE, MEM_COMMIT, PAGE_READWRITE);
            assert!(!committed.is_null());
            base.cast()
        }
    }

    fn release(base: *mut u8) {
        // SAFETY: the reservation made by `region`.
        unsafe { VirtualFree(base.cast(), 0, MEM_RELEASE) };
    }

    #[test]
    fn write_mapped_writes_mapped_pages_and_skips_the_rest() {
        let base = region();
        let data = vec![0xabu8; 3 * PAGE];
        // The third page is not committed: its piece is skipped, the others land.
        write_mapped(base, data.as_ptr(), 3 * PAGE);
        // SAFETY: the two committed pages of `region`.
        let written = unsafe { std::slice::from_raw_parts(base, 2 * PAGE) };
        assert!(written.iter().all(|&b| b == 0xab));
        release(base);
    }

    #[test]
    fn copy_into_title_memory_that_is_not_all_mapped_is_left_alone() {
        let base = region();
        let from = vec![0x5au8; 3 * PAGE];
        copy_mapped(base, from.as_ptr(), 3 * PAGE, true);
        // A whole copy into mapped memory works without asking the system first.
        copy_mapped(base, from.as_ptr(), 2 * PAGE, true);
        // SAFETY: the two committed pages of `region`.
        let now = unsafe { std::slice::from_raw_parts(base, 2 * PAGE) };
        assert!(now.iter().all(|&b| b == 0x5a));
        release(base);
    }
}

/// The host's own paths for a copy in the card's memory, end to end, on
/// the software adapter only: the title's bytes go up, a shader reads them
/// and writes, and what it wrote comes back into title memory. Run it with
/// the switches under test in the environment (they are read once per
/// process); it does nothing unless the host device is the software
/// adapter (`VICTORY_D3D12_WARP=1` in a diagnostics build) and the copy is
/// in the card's memory (`VICTORY_CARD_BUFFERS_MIN_MB=1`):
///
/// `VICTORY_D3D12_WARP=1 VICTORY_CARD_BUFFERS_MIN_MB=1 [VICTORY_QUEUE2=n]
///  [VICTORY_RELAY=2 | VICTORY_LATE_WAIT=3] cargo test -p victory-gpu
///  --features diagnostics queue2_tests`
///
/// The same title memory must come out whatever the switches: through the
/// direct queue alone, and through the copy queues.
#[cfg(test)]
mod queue2_tests {
    use super::*;

    pub(super) static GPU_VALIDATION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    pub(super) static VALIDATION_ERROR: Mutex<Option<String>> = Mutex::new(None);
    static VALIDATION_TEST: Mutex<()> = Mutex::new(());

    /// `memory[to] = memory[from] + add`, after `spin` steps.
    const ADD: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0), UAV(u0)"
cbuffer What : register(b0) { uint to; uint from; uint add; uint spin; };
RWByteAddressBuffer memory : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    uint h = 0u;
    [loop]
    for (uint i = 0u; i < spin; i++) {
        h = (h ^ memory.Load(from)) * 0x01000193u;
    }
    memory.Store(to, memory.Load(from) + add + (h & 0u));
}
"#;


    #[test]
    fn relay_settles_after_a_validation_error() {
        let _test = VALIDATION_TEST.lock().unwrap_or_else(|e| e.into_inner());
        let Some(host) = host() else { return };
        if !relay() || !GPU_VALIDATION.load(Ordering::Relaxed) { return }
        let queue = host.messages.as_ref().expect("D3D12 debug queue");
        let list = host.new_list().unwrap();
        list.close();
        // SAFETY: this inserts a diagnostic without issuing an invalid GPU command.
        unsafe {
            queue.AddApplicationMessage(D3D12_MESSAGE_SEVERITY_ERROR, windows::core::s!("relay validation test"))
        }.unwrap();
        assert_ne!(host.execute_own(&list), 0);
        let (settled, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            settle_now(WHY_SYNC);
            let _ = settled.send(());
        });
        done.recv_timeout(std::time::Duration::from_secs(10)).expect("relay did not settle");
        let error = VALIDATION_ERROR.lock().unwrap_or_else(|e| e.into_inner()).take();
        assert!(error.as_deref().is_some_and(|e| e.contains("relay validation test")), "{error:?}");
    }

    #[test]
    fn copies_the_builders_compacted_size_after_its_dispatch() {
        let enhanced = crate::draw::enhanced_barriers();
        check_builder_size_copy(enhanced, false);
        if enhanced { check_builder_size_copy(true, true); }
    }

    #[test]
    fn copies_the_builders_compacted_size_with_legacy_barriers() {
        check_builder_size_copy(false, false);
        check_builder_size_copy(false, true);
    }

    fn check_builder_size_copy(enhanced: bool, explicit_uav: bool) {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        let _test = VALIDATION_TEST.lock().unwrap_or_else(|e| e.into_inner());
        let host = host();
        if std::env::var_os("VICTORY_D3D12_GBV").is_some() {
            assert!(GPU_VALIDATION.load(Ordering::Relaxed), "GPU-based validation unavailable");
            assert!(host.is_some_and(|h| h.messages.is_some()), "no D3D12 debug message queue");
        }
        let Some(host) = host else { return };
        let code = compile(ADD, windows::core::s!("main"), windows::core::s!("cs_5_1")).unwrap();
        // SAFETY: the shader contains this root signature and accesses only allocated dwords.
        let (root, pso): (ID3D12RootSignature, ID3D12PipelineState) = unsafe {
            let root = host.device.CreateRootSignature(0, &code).unwrap();
            let pso = host
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.as_ptr().cast(),
                        BytecodeLength: code.len(),
                    },
                    ..Default::default()
                })
                .unwrap();
            (root, pso)
        };
        const SIZE: usize = 65536;
        let ptr = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                2 * SIZE,
                MEM_RESERVE | MEM_COMMIT | 0x20_0000,
                PAGE_READWRITE,
            )
        }
        .cast::<u32>();
        assert!(!ptr.is_null());
        let addr = ptr as u64;
        crate::shadow::register(ptr as usize, addr, SIZE as u64);
        let other = addr + SIZE as u64;
        crate::shadow::register(other as usize, other, SIZE as u64);
        let shadow = crate::shadow::find(addr, SIZE as u64).unwrap();
        let list = host.new_list().unwrap();
        unsafe { ptr.write(0x2700) };
        shadow.refresh(&list, addr, 12);
        shadow.gpu_writes(&list, addr + 4, 4);
        unsafe {
            // Exercise an already-established UAV state as well as implicit promotion.
            if explicit_uav {
                list.list.ResourceBarrier(&[transition(
                    &shadow.buffer, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                )]);
            }
            list.list.SetPipelineState(&pso);
            list.list.SetComputeRootSignature(&root);
            list.list
                .SetComputeRoot32BitConstants(0, 4, [4u32, 0, 0x80, 0].as_ptr().cast(), 0);
            list.list.SetComputeRootUnorderedAccessView(1, shadow.gpu(addr));
            list.list.Dispatch(1, 1, 1);
        }
        list.builder_report(&shadow, addr + 4, 4);
        assert!(!list.copy_builder_report_with_barriers(addr + 8, addr, 4, enhanced));
        assert!(list.copy_builder_report_with_barriers(addr + 8, addr + 4, 4, enhanced));
        assert!(list.copy_builder_report_with_barriers(other, addr + 4, 4, enhanced));
        // A later dispatch on this list consumes the copied word as a UAV again.
        shadow.gpu_writes(&list, addr + 12, 4);
        unsafe {
            list.list.SetComputeRoot32BitConstants(0, 4, [12u32, 8, 1, 0].as_ptr().cast(), 0);
            list.list.Dispatch(1, 1, 1);
        }
        // Turn the separate readback destination into a second producer. Copy into
        // another offset of that UAV, then copy its own result back to the first UAV.
        let second = crate::shadow::find(other, SIZE as u64).unwrap();
        assert_ne!(shadow.buffer, second.buffer);
        second.gpu_writes(&list, other + 4, 4);
        unsafe {
            if explicit_uav {
                list.list.ResourceBarrier(&[transition(
                    &second.buffer, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                )]);
            }
            list.list.SetComputeRootUnorderedAccessView(1, second.gpu(other));
            list.list.SetComputeRoot32BitConstants(0, 4, [4u32, 0, 2, 0].as_ptr().cast(), 0);
            list.list.Dispatch(1, 1, 1);
        }
        list.builder_report(&second, other + 4, 4);
        assert!(list.copy_builder_report_with_barriers(other + 8, addr + 4, 4, enhanced));
        assert!(list.copy_builder_report_with_barriers(addr + 16, other + 4, 4, enhanced));
        let mut readback = 0u32;
        assert!(list.copy_builder_report_to((&raw mut readback) as u64, other + 4, 4, enhanced, |_, _| None));
        // Overwrite the source after the fallback copy. Readback must retain the earlier size.
        unsafe {
            list.list.SetComputeRoot32BitConstants(0, 4, [4u32, 0, 3, 0].as_ptr().cast(), 0);
            list.list.Dispatch(1, 1, 1);
        }
        // Two adjacent builder words overlap the destination by four bytes.
        shadow.gpu_writes(&list, addr + 24, 8);
        unsafe {
            list.list.SetComputeRootUnorderedAccessView(1, shadow.gpu(addr));
            for (offset, add) in [(24u32, 1), (28, 2)] {
                list.list.SetComputeRoot32BitConstants(0, 4, [offset, 4, add, 0].as_ptr().cast(), 0);
                list.list.Dispatch(1, 1, 1);
            }
        }
        list.builder_report(&shadow, addr + 24, 8);
        assert!(list.copy_builder_report_with_barriers(addr + 28, addr + 24, 8, enhanced));
        list.close();
        host.execute(&[&list]);
        settle_now(WHY_SYNC);
        let error = VALIDATION_ERROR.lock().unwrap_or_else(|e| e.into_inner()).take();
        assert!(error.is_none(), "D3D12 validation: {error:?}");
        assert_eq!(unsafe { ptr.add(2).read_volatile() }, 0x2780);
        assert_eq!(unsafe { (other as *const u32).read_volatile() }, 0x2780);
        assert_eq!(unsafe { ptr.add(3).read_volatile() }, 0x2781);
        assert_eq!(unsafe { ((other + 8) as *const u32).read_volatile() }, 0x2780);
        assert_eq!(unsafe { ptr.add(4).read_volatile() }, 0x2782);
        assert_eq!(unsafe { ptr.add(7).read_volatile() }, 0x2781);
        assert_eq!(unsafe { ptr.add(8).read_volatile() }, 0x2782);
        assert_eq!(unsafe { ((other + 4) as *const u32).read_volatile() }, 0x2783);
        assert_eq!(readback, 0x2782);
        eprintln!("verified compacted-size readback (enhanced: {enhanced}, explicit UAV: {explicit_uav})");
        crate::shadow::unregister(other as usize, other);
        drop(list);
        crate::shadow::unregister(ptr as usize, addr);
        drop(shadow);
        unsafe {
            VirtualFree(ptr.cast(), 0, MEM_RELEASE);
        }
    }

    #[test]
    fn warp_title_bytes_go_up_and_shader_writes_come_back_whatever_the_queues() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        const MEM_WRITE_WATCH: u32 = 0x0020_0000;
        let Some(host) = host() else {
            return;
        };
        // Never on a real card.
        // SAFETY: a plain query.
        let software = host
            .adapter
            .as_ref()
            .and_then(|a| unsafe { a.GetDesc1() }.ok())
            .is_some_and(|d| d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0);
        if !software {
            eprintln!("queue2_tests: skipped, the host device is not the software adapter (VICTORY_D3D12_WARP=1, diagnostics)");
            return;
        }
        const PIECE: usize = 64 << 10;
        const LEN: usize = 4 << 20;
        const ROUNDS: usize = 12;
        // "Title memory": write-watched, as the title's is.
        // SAFETY: a plain allocation of our own.
        let title = unsafe { VirtualAlloc(std::ptr::null(), LEN, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) } as *mut u8;
        assert!(!title.is_null());
        let dword = |at: usize| unsafe { title.add(at).cast::<u32>() };
        for i in 0..LEN / 4 {
            // SAFETY: inside the allocation.
            unsafe { dword(i * 4).write_volatile(0x5A00_0000 | i as u32) };
        }
        crate::shadow::register(0x51_5151, title as u64, LEN as u64);
        let shadow = crate::shadow::find(title as u64, LEN as u64).expect("a host copy");
        if !shadow.carded() {
            eprintln!("queue2_tests: skipped, the copy is not in the card's memory (VICTORY_CARD_BUFFERS_MIN_MB=1)");
            return;
        }
        let code = compile(ADD, windows::core::s!("main"), windows::core::s!("cs_5_1")).expect("the test's shader");
        // SAFETY: the shader's own root signature; plain creation.
        let (root, pso): (ID3D12RootSignature, ID3D12PipelineState) = unsafe {
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &code).expect("a root signature");
            let pso = host
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.as_ptr().cast(),
                        BytecodeLength: code.len(),
                    },
                    ..Default::default()
                })
                .expect("a pipeline");
            (root, pso)
        };
        // Round k: the title writes a dword in piece 1+k; a submission's
        // shader adds k to it and stores the sum in piece 30+k, a range
        // declared as shader-written. Nothing is waited for between the
        // rounds where submissions are sent ahead.
        let from = |k: usize| (1 + k) * PIECE + 4 * k + 64;
        let to = |k: usize| (30 + k) * PIECE + 8 * k + 128;
        let titles = |k: usize| 0x0100_0000 + 0x1000 * k as u32 + 7;
        let mut lists = Vec::new();
        for k in 0..ROUNDS {
            // SAFETY: inside the allocation.
            unsafe { dword(from(k)).write_volatile(titles(k)) };
            let list = host.new_list().expect("a host list");
            shadow.refresh(&list, title as u64, LEN as u64);
            shadow.gpu_writes(&list, (title as usize + (30 + k) * PIECE) as u64, PIECE as u64);
            // SAFETY: an open list; the copy's buffer, with unordered
            // access; the shader's own root signature.
            unsafe {
                list.list.SetComputeRootSignature(&root);
                list.list.SetPipelineState(&pso);
                let constants = [to(k) as u32, from(k) as u32, k as u32, 30_000];
                list.list.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
                list.list.SetComputeRootUnorderedAccessView(1, shadow.buffer.GetGPUVirtualAddress());
                list.list.Dispatch(1, 1, 1);
            }
            list.close();
            if late() {
                host.execute_own(&list);
            } else {
                host.execute(&[&list]);
            }
            lists.push(list);
        }
        settle_now(WHY_SYNC);
        for k in 0..ROUNDS {
            // SAFETY: inside the allocation.
            let (sum, kept) = unsafe { (dword(to(k)).read_volatile(), dword(from(k)).read_volatile()) };
            assert_eq!(kept, titles(k), "round {k}: the title's own dword changed");
            assert_eq!(
                sum,
                titles(k) + k as u32,
                "round {k}: the shader read {:#x} where the title had written {:#x}, or its write did not come back",
                sum.wrapping_sub(k as u32),
                titles(k)
            );
        }
        // Nothing else moved.
        let touched: std::collections::HashSet<usize> = (0..ROUNDS).flat_map(|k| [from(k), to(k)]).collect();
        for i in (0..LEN / 4).filter(|i| !touched.contains(&(i * 4))) {
            // SAFETY: inside the allocation.
            let got = unsafe { dword(i * 4).read_volatile() };
            assert_eq!(got, 0x5A00_0000 | i as u32, "dword {i} of title memory changed");
        }
        // The queues the switches name did the work.
        if let Some(second) = crate::queue2::second() {
            let (down, up) = (second.down.stats.jobs.load(Ordering::Relaxed), second.up.stats.jobs.load(Ordering::Relaxed));
            // (Sends that found no landing buffer went the old way.)
            let no_room = second.landing[1].load(Ordering::Relaxed);
            eprintln!(
                "queue2_tests: VICTORY_QUEUE2={} late {} relay {}: {down} fetches and {up} sends through the copy queues ({no_room} more found no landing buffer), the direct queue told to wait {} times",
                crate::queue2::bits(),
                late(),
                relay(),
                second.held[0].load(Ordering::Relaxed)
            );
            assert_eq!(down > 0, second.fetches(), "fetches through the copy queue");
            assert_eq!(up + no_room > 0, second.uploads(), "sends through the copy queue");
            assert_eq!(up, second.held[0].load(Ordering::Relaxed), "the direct queue waits for every send");
        } else {
            eprintln!("queue2_tests: the direct queue alone (late {}, relay {})", late(), relay());
        }
        drop(lists);
    }

    /// `memory[to] = first` at once; then `spin` steps, each a store to
    /// `memory[to + 4]`; then `memory[to + 4] = 0` and `memory[to] = last`.
    /// Whatever `spin` is, the bytes it leaves are the same.
    const TWICE: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0), UAV(u0)"
cbuffer What : register(b0) { uint to; uint first; uint last; uint spin; };
RWByteAddressBuffer memory : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    memory.Store(to, first);
    [loop]
    for (uint i = 0u; i < spin; i++) {
        memory.Store(to + 4u, i);
    }
    memory.Store(to + 4u, 0u);
    memory.Store(to, last);
}
"#;

    /// The hole a fetch on the copy queue had (`VICTORY_QUEUE2` bit 1 with
    /// the relay): flight A writes a piece; flight B, sent behind it,
    /// writes the same piece through a half-way value and leaves it as A
    /// left it. A is landed while B is half way: the fetch, beside B on
    /// the copy queue, brings B's half-way bytes into title memory. B's
    /// compare finds the piece as A's compare left it and flags nothing,
    /// so B's landing fetched nothing and title memory kept the half-way
    /// bytes. With the fix B's landing fetches the piece all the same
    /// (`BESIDE`).
    ///
    /// `VICTORY_D3D12_WARP=1 VICTORY_CARD_BUFFERS_MIN_MB=1 VICTORY_QUEUE2=1
    ///  VICTORY_RELAY=1 cargo test -p victory-gpu --features diagnostics
    ///  fetched_again -- --nocapture` (alone: it lands its two flights
    /// itself; `VICTORY_QUEUE2_REFETCH=0` shows the fault).
    #[test]
    fn warp_a_piece_fetched_while_a_later_flight_writes_it_is_fetched_again() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        const MEM_WRITE_WATCH: u32 = 0x0020_0000;
        let Some(host) = host() else {
            return;
        };
        // Never on a real card.
        // SAFETY: a plain query.
        let software = host
            .adapter
            .as_ref()
            .and_then(|a| unsafe { a.GetDesc1() }.ok())
            .is_some_and(|d| d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0);
        if !software {
            eprintln!("queue2_tests: skipped, the host device is not the software adapter (VICTORY_D3D12_WARP=1, diagnostics)");
            return;
        }
        if !relay() || !crate::queue2::second().is_some_and(|s| s.fetches()) {
            eprintln!("queue2_tests: skipped, fetches do not go to a copy queue beside the relay (VICTORY_RELAY=1 VICTORY_QUEUE2=1)");
            return;
        }
        const PIECE: usize = 64 << 10;
        const LEN: usize = 4 << 20;
        // SAFETY: a plain allocation of our own.
        let title = unsafe { VirtualAlloc(std::ptr::null(), LEN, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) } as *mut u8;
        assert!(!title.is_null());
        let dword = |at: usize| unsafe { title.add(at).cast::<u32>() };
        for i in 0..LEN / 4 {
            // SAFETY: inside the allocation.
            unsafe { dword(i * 4).write_volatile(0x5A00_0000 | i as u32) };
        }
        crate::shadow::register(0x52_5252, title as u64, LEN as u64);
        let shadow = crate::shadow::find(title as u64, LEN as u64).expect("a host copy");
        if !shadow.carded() {
            eprintln!("queue2_tests: skipped, the copy is not in the card's memory (VICTORY_CARD_BUFFERS_MIN_MB=1)");
            return;
        }
        let code = compile(TWICE, windows::core::s!("main"), windows::core::s!("cs_5_1")).expect("the test's shader");
        // SAFETY: the shader's own root signature; plain creation.
        let (root, pso): (ID3D12RootSignature, ID3D12PipelineState) = unsafe {
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &code).expect("a root signature");
            let pso = host
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.as_ptr().cast(),
                        BytecodeLength: code.len(),
                    },
                    ..Default::default()
                })
                .expect("a pipeline");
            (root, pso)
        };
        // A list whose shader writes the dword at 256 of `piece`, declared.
        let record = |piece: usize, first: u32, last: u32, spin: u32| {
            let list = host.new_list().expect("a host list");
            shadow.refresh(&list, title as u64, LEN as u64);
            shadow.gpu_writes(&list, (title as usize + piece * PIECE) as u64, PIECE as u64);
            // SAFETY: an open list; the copy's buffer, with unordered
            // access; the shader's own root signature.
            unsafe {
                list.list.SetComputeRootSignature(&root);
                list.list.SetPipelineState(&pso);
                let constants = [(piece * PIECE + 256) as u32, first, last, spin];
                list.list.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
                list.list.SetComputeRootUnorderedAccessView(1, shadow.buffer.GetGPUVirtualAddress());
                list.list.Dispatch(1, 1, 1);
            }
            list.close();
            list
        };
        // SAFETY: a plain query.
        let completed = || unsafe { host.fence.GetCompletedValue() };
        let wait_for = |value: u64| {
            while completed() < value {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        // How many steps keep the card busy for some seconds: timed on
        // another piece, waited for.
        const TRIAL: u32 = 1 << 20;
        let trial = record(40, 1, 2, TRIAL);
        let timing = std::time::Instant::now();
        host.execute(&[&trial]);
        let took = timing.elapsed().as_secs_f64().max(1e-4);
        let spin = (f64::from(TRIAL) * 3.0 / took).clamp(f64::from(TRIAL), 2.0e9) as u32;
        // SAFETY: inside the allocation.
        assert_eq!(unsafe { dword(40 * PIECE + 256).read_volatile() }, 2, "the trial's write did not come back");
        const AT: usize = 20 * PIECE + 256;
        const HALF_WAY: u32 = 0xDEAD_0001;
        const LEFT: u32 = 0x600D_0002;
        // Flight A, and the card has run it.
        let a = record(20, HALF_WAY, LEFT, 16);
        let (submitted, mut sent_a) = host.submit(&[&a], true).expect("flight A sent ahead");
        let fence_a = *submitted;
        drop(submitted);
        wait_for(fence_a);
        // Flight B: the same piece, through the half-way value, for seconds.
        let b = record(20, HALF_WAY, LEFT, spin);
        let (submitted, mut sent_b) = host.submit(&[&b], true).expect("flight B sent ahead");
        let fence_b = *submitted;
        drop(submitted);
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            completed() < fence_b,
            "flight B had run after 0.3 s ({spin} steps; the trial's {TRIAL} took {took:.3} s): nothing to show"
        );
        // A lands, as `Host::relay_land` lands it, while B is half way.
        let mut fetch = None;
        {
            let plan = sent_a.card.as_ref().expect("flight A has a plan for the card's copies");
            host.relay_card_back(&[(sent_a.number, plan)], &mut fetch, fence_a, sent_a.number);
        }
        for (copy, ranges) in &sent_a.back {
            copy.copy_back_done(ranges);
        }
        // SAFETY: inside the allocation.
        let seen = unsafe { dword(AT).read_volatile() };
        let b_ran = completed() >= fence_b;
        eprintln!(
            "queue2_tests: after A's landing title memory holds {seen:#x} (half way {HALF_WAY:#x}, left {LEFT:#x}); B had run: {b_ran}; {spin} steps"
        );
        assert_eq!(
            seen, HALF_WAY,
            "the fetch for A did not come down beside B (it waited for B, or B had not begun): this machine cannot show the fault"
        );
        // B has run; it left the piece as A left it, so its compare flags
        // nothing. Its landing must bring the piece down all the same.
        wait_for(fence_b);
        {
            let plan = sent_b.card.as_ref().expect("flight B has a plan for the card's copies");
            host.relay_card_back(&[(sent_b.number, plan)], &mut fetch, fence_b, sent_b.number);
        }
        for (copy, ranges) in &sent_b.back {
            copy.copy_back_done(ranges);
        }
        // SAFETY: inside the allocation.
        let (left, beside) = unsafe { (dword(AT).read_volatile(), dword(AT + 4).read_volatile()) };
        assert_eq!(
            left, LEFT,
            "title memory kept flight B's half-way bytes: B's compare found the piece as A left it and B's landing fetched nothing"
        );
        assert_eq!(beside, 0, "the dword beside it holds a half-way count");
        for sent in [&mut sent_a, &mut sent_b] {
            if let Some(plan) = sent.card.take() {
                host.card_done(plan);
            }
        }
        drop((a, b, trial));
    }
}

/// `VICTORY_COPY_NOQUERY`: the record's paths read the same bytes as the
/// paths that ask Windows, over a fragmented placeholder pool with holes,
/// a view the record never saw, memory it does not know, and views
/// remapped while copies run.
#[cfg(test)]
mod noquery_tests {
    use super::{copy_os, read_sparse_query, read_sparse_runs};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
        MEM_RESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3, PAGE_NOACCESS, PAGE_READWRITE,
        SEC_COMMIT, UnmapViewOfFile2, VirtualAlloc2, VirtualFree,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    const K64: usize = 64 << 10;
    /// 6 MiB: two 4 MiB chunks of `regions::copy`.
    const SLOTS: usize = 96;

    struct Pool {
        base: usize,
        section: usize,
        /// What each slot reads as: its section bytes if mapped, else zeros.
        expected: Vec<u8>,
    }

    fn map(pool: &Pool, slot: usize) -> bool {
        // SAFETY: a placeholder slot of the pool, split to 64 KiB.
        let v = unsafe {
            MapViewOfFile3(pool.section as _, GetCurrentProcess(), (pool.base + slot * K64) as _, (slot * K64) as u64, K64,
                MEM_REPLACE_PLACEHOLDER, PAGE_READWRITE, std::ptr::null_mut(), 0)
        };
        !v.Value.is_null()
    }

    fn unmap(pool: &Pool, slot: usize) {
        // SAFETY: a view of the pool mapped by `map`.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), MEMORY_MAPPED_VIEW_ADDRESS { Value: (pool.base + slot * K64) as _ }, MEM_PRESERVE_PLACEHOLDER)
        };
    }

    /// Slots mapped and told (2 in 3, in runs), holes, slot 41 mapped but
    /// not told (the record says hole), and slots 70-71 left out of the
    /// record (unknown).
    fn pool() -> Pool {
        let size = SLOTS * K64;
        // SAFETY: a fresh section and placeholder of our own.
        let (section, base) = unsafe {
            let s = CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE | SEC_COMMIT, 0, size as u32, std::ptr::null());
            let b = VirtualAlloc2(GetCurrentProcess(), std::ptr::null(), size, MEM_RESERVE | MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS, std::ptr::null_mut(), 0);
            assert!(!s.is_null() && !b.is_null());
            for i in 0..SLOTS - 1 {
                VirtualFree(b.cast::<u8>().add(i * K64).cast(), K64, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
            }
            (s as usize, b as usize)
        };
        crate::regions::title_memory_placeholder(base, size);
        let mut pool = Pool { base, section, expected: vec![0; size] };
        for slot in 0..SLOTS {
            let told = slot % 3 != 2 || (10..14).contains(&slot);
            if !(told || slot == 41) {
                continue;
            }
            assert!(map(&pool, slot));
            let at = pool.base + slot * K64;
            for i in 0..K64 {
                let b = ((slot * 131 + i * 7) % 251) as u8 | 1;
                // SAFETY: the view just mapped.
                unsafe { *((at + i) as *mut u8) = b };
                pool.expected[slot * K64 + i] = b;
            }
            if told {
                crate::regions::mapped(at, K64);
            }
        }
        crate::regions::title_memory_not_placeholder(base + 70 * K64, 2 * K64);
        pool
    }

    fn runs(pool: &Pool) -> Vec<victory_core::memmap::Run> {
        let r = crate::regions::sparse_runs_any(pool.base, SLOTS * K64).expect("the record knows the pool");
        assert!(r.iter().any(|r| r.kind == victory_core::memmap::Kind::Unknown));
        r
    }

    #[test]
    fn sparse_read_by_the_record_reads_what_queries_read() {
        let pool = pool();
        let size = SLOTS * K64;
        let (mut old, mut new) = (vec![0xeeu8; size], vec![0xddu8; size]);
        read_sparse_query(pool.base as *const u8, old.as_mut_ptr(), size);
        read_sparse_runs(pool.base as *const u8, new.as_mut_ptr(), &runs(&pool));
        assert!(old == pool.expected, "the query path read other bytes");
        assert!(new == old, "the record's path read other bytes than the query path");
    }

    #[test]
    fn copies_under_remaps_read_the_same_bytes() {
        let pool = Arc::new(pool());
        let size = SLOTS * K64;
        let stop = Arc::new(AtomicBool::new(false));
        // Views of told slots remapped (the same section bytes) under the
        // runtime's change hold, as xmem does.
        let remapper = {
            let (pool, stop) = (pool.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut n = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    let slot = [0usize, 1, 13, 64, 66, 90][n % 6];
                    let at = pool.base + slot * K64;
                    {
                        let _changing = crate::watch::title_memory_remapping(at, K64);
                        unmap(&pool, slot);
                        crate::regions::unmapped(at, K64);
                        assert!(map(&pool, slot));
                        crate::regions::mapped(at, K64);
                    }
                    n += 1;
                    std::thread::sleep(std::time::Duration::from_micros(100));
                }
                n
            })
        };
        let os = |d: *mut u8, s: *const u8, n: usize| copy_os(d, s, n, false);
        for _ in 0..200 {
            let mut waited = vec![0xaau8; size];
            assert!(crate::regions::copy_by(waited.as_mut_ptr(), pool.base as *const u8, size, os, true));
            assert!(waited == pool.expected, "a copy that waits for busy chunks read other bytes");
            // Both sparse paths with the mapping held still between remaps.
            let (mut old, mut new) = (vec![1u8; size], vec![2u8; size]);
            {
                let _still = crate::watch::title_memory_reading(pool.base, size);
                read_sparse_query(pool.base as *const u8, old.as_mut_ptr(), size);
                read_sparse_runs(pool.base as *const u8, new.as_mut_ptr(), &runs(&pool));
            }
            assert!(old == pool.expected && new == old, "the sparse paths read other bytes under remaps");
        }
        stop.store(true, Ordering::Relaxed);
        assert!(remapper.join().unwrap() > 0);
    }

    /// A busy chunk is waited for, not read through the kernel.
    #[test]
    fn a_busy_chunk_is_waited_for() {
        let pool = pool();
        let size = SLOTS * K64;
        let at = pool.base + K64;
        let (held, wait) = std::sync::mpsc::channel();
        let let_go = std::thread::spawn(move || {
            let changing = crate::watch::title_memory_remapping(at, K64);
            held.send(()).unwrap();
            // A change's length, spun (a sleep is a millisecond or more).
            let begun = std::time::Instant::now();
            while begun.elapsed() < std::time::Duration::from_micros(300) {
                std::hint::spin_loop();
            }
            drop(changing);
        });
        wait.recv().unwrap();
        let through_kernel = AtomicBool::new(false);
        let mut got = vec![0u8; size];
        let os = |d: *mut u8, s: *const u8, n: usize| {
            if (s as usize) < pool.base + (4 << 20) && n >= 4 << 20 {
                through_kernel.store(true, Ordering::Relaxed);
            }
            copy_os(d, s, n, false)
        };
        assert!(crate::regions::copy_by(got.as_mut_ptr(), pool.base as *const u8, size, os, true));
        let_go.join().unwrap();
        assert!(got == pool.expected);
        assert!(!through_kernel.load(Ordering::Relaxed), "the busy chunk went through the kernel");
    }
}
