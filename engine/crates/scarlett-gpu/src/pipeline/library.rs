//! Compiled host pipelines kept on disk, and the title pipelines worth
//! starting early (decision 0007, "Pipeline cache").
//!
//! 1. Every host pipeline goes into the host's pipeline library, named by
//!    a hash of all that its host description holds (the translated
//!    shaders' code, which changes when a translation does, our root
//!    signature, the graphics state). The library is saved under the cache
//!    folder; the host refuses a library of another adapter or driver, and
//!    an entry whose description differs, so neither is ever used.
//! 2. The title pipelines a run drew or dispatched with are listed by a key
//!    known when the title creates them (their shader containers and
//!    graphics state); the next run hands those to the workers at
//!    creation, during loading.
//!
//! Library calls overlap: the host documents the library as safe to use
//! from several threads ("The pipeline library is thread-safe to use, and
//! will internally synchronize as necessary", `CreatePipelineLibrary`,
//! Thread Safety), with one exception kept here: two loads of the same
//! pipeline are made one after the other ([`same_name`]). Under one lock of
//! ours for every call, the eight workers made pipelines one at a time:
//! 87 to 96 of 110 looks at each worker found it waiting for that lock
//! (run 49), and an earlier run's 2,300 pipelines, all in the library, were
//! still being loaded 46 s into the opening cutscene.
//! `SCARLETT_LIBRARY_LOCK=1`: one call at a time, as before.
//!
//! The library's file is mapped, not read into memory ([`Blob`]): the host
//! keeps pointing into the bytes it was made from for as long as it lives,
//! and read into a buffer they were 1,352 MB of the process's own memory
//! for the whole run (run 60), all of it in physical memory. Mapped, the
//! pages are the file's: only those the host reads are in memory, and the
//! system can drop them without writing them anywhere. A save writes the
//! new library through a mapping of the new file in the same way (a buffer
//! of the library's size was made, filled and written out at each save).
//! `SCARLETT_LIBRARY_MAP=0`: read and written through buffers, as before.
//!
//! The mapped file's handle stays open for as long as the mapping lives
//! ([`holds`]). A save renames the new library over the file this run has
//! mapped, and with the handle closed Windows then gives up the old file's
//! contents: every page of the mapping that is not in memory at that
//! moment, or leaves memory later (the trim below, or Windows taking pages
//! back), reads as zeros from then on. The host's library reads such pages
//! as stored pipelines; the driver, handed a pipeline of zeros, removes the
//! device with DXGI_ERROR_DRIVER_INTERNAL_ERROR (runs 72, 73 and 82, each
//! within 5 ms of the first library call after a save and a trim; and the
//! library run 77 saved from such pages was refused by run 78). Measured
//! with a 64 MB file written past the system's cache, mapped and replaced:
//! 16,352 of 16,384 pages read zeros with the handle closed, none with it
//! open (the test below). `SCARLETT_LIBRARY_HANDLE=0`: closed, as before.
//!
//! A save no longer renames the new library over a file this run has
//! mapped: it is left beside it as [`NEXT_FILE`], which the next start
//! takes ([`Blob::of`]). RX 6600 / 6700 XT players (issues #84, #78) lost
//! the device with the file replaced under the mapping twice in that run;
//! with the handle open the old pages should still read, but nothing
//! needs the replacement during a run, so it is not risked.
//! `SCARLETT_LIBRARY_REPLACE_MAPPED=1`: renamed over it, as before.
//!
//! On AMD cards mesh pipelines are made one at a time ([`made_alone`]):
//! the same reports had three mesh+pixel pipelines being made at once,
//! each for 1.8 s, when the device was removed with nothing of ours on the
//! card. Other pipelines, and loads from the library, still overlap.
//! `SCARLETT_MESH_MAKE_ONE=0`: overlapping, as before; `=1`: one at a time
//! on every card.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, RwLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{HSTRING, Interface, PCWSTR};

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Keeps the pipeline library and the early list in `dir` from now on.
pub fn use_pipeline_cache(dir: PathBuf) {
    let _ = DIR.set(dir);
}

/// The folder the library is kept in.
pub(super) fn folder() -> Option<PathBuf> {
    DIR.get().cloned()
}

/// Host pipelines created, loaded from the library, and title pipelines
/// started at creation, since the start.
pub(crate) static MADE: AtomicU64 = AtomicU64::new(0);
pub(crate) static LOADED: AtomicU64 = AtomicU64::new(0);
pub(crate) static EARLY: AtomicU64 = AtomicU64::new(0);
/// For the memory lines: the bytes of the library's file, kept in memory
/// while the library lives, and the bytes the last save wrote (a buffer of
/// that size is made, filled and written every time pipelines were added,
/// at most every 20 s).
pub(crate) static BLOB: AtomicU64 = AtomicU64::new(0);
pub(crate) static SAVED: AtomicU64 = AtomicU64::new(0);

/// Whether the library's file is mapped ([`Blob`]) and saved through a
/// mapping. On by default; `SCARLETT_LIBRARY_MAP=0` uses buffers, as before.
fn mapped() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SCARLETT_LIBRARY_MAP").is_none_or(|v| v != "0"))
}

/// Whether the mapped library file's handle is kept open while it is
/// mapped (see the module's notes: without it a save in the same run turns
/// the mapping's pages into zeros as they leave memory). On by default;
/// `SCARLETT_LIBRARY_HANDLE=0` closes it after mapping, as before.
fn holds() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SCARLETT_LIBRARY_HANDLE").is_none_or(|v| v != "0"))
}

/// A whole file mapped into memory: read-only (the library's file) or
/// writable (the file a save fills). Unmapped when dropped.
struct Mapped {
    at: windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
    /// The file, for a mapping made by [`Mapped::open`] ([`holds`]): open
    /// until after the view is unmapped, so that the file a rename took
    /// the name from keeps its contents for the view.
    _file: Option<std::fs::File>,
}

impl Mapped {
    /// Maps `file` (`len` bytes, not zero) for reading, or for reading and
    /// writing.
    fn map(file: &std::fs::File, len: usize, write: bool) -> Option<Mapped> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, PAGE_READONLY, PAGE_READWRITE,
        };
        if len == 0 {
            return None;
        }
        // SAFETY: a file we hold open; the view keeps the file's pages
        // after the mapping object and the file are closed.
        let at = unsafe {
            let mapping = CreateFileMappingW(
                file.as_raw_handle(),
                std::ptr::null(),
                if write { PAGE_READWRITE } else { PAGE_READONLY },
                0,
                0,
                std::ptr::null(),
            );
            if mapping.is_null() {
                return None;
            }
            let at = MapViewOfFile(mapping, if write { FILE_MAP_WRITE } else { FILE_MAP_READ }, 0, 0, 0);
            CloseHandle(mapping);
            at
        };
        (!at.Value.is_null()).then_some(Mapped { at, len, _file: None })
    }

    /// The file at `path`, mapped for reading; `None` if it is not there,
    /// is empty or cannot be mapped.
    fn open(path: &std::path::Path) -> Option<Mapped> {
        Mapped::open_holding(path, holds())
    }

    /// As [`Mapped::open`]; `hold` says whether the file stays open with
    /// the mapping.
    fn open_holding(path: &std::path::Path, hold: bool) -> Option<Mapped> {
        // Opened as the standard library opens a file to read: others may
        // read, write, rename and delete it meanwhile, so a save still
        // renames the new library over this one.
        let file = std::fs::File::open(path).ok()?;
        let len = usize::try_from(file.metadata().ok()?.len()).ok()?;
        let mut mapped = Mapped::map(&file, len, false)?;
        if hold {
            mapped._file = Some(file);
        }
        Some(mapped)
    }

    /// A new file of `len` bytes at `path` (replacing what is there),
    /// mapped for writing.
    fn create(path: &std::path::Path, len: usize) -> Option<Mapped> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .ok()?;
        file.set_len(len as u64).ok()?;
        Mapped::map(&file, len, true)
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the view holds `len` readable bytes until dropped.
        unsafe { std::slice::from_raw_parts(self.at.Value.cast(), self.len) }
    }

    /// Only for a view made by [`Mapped::create`].
    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: a writable view of `len` bytes, ours alone.
        unsafe { std::slice::from_raw_parts_mut(self.at.Value.cast(), self.len) }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: our view.
        unsafe { windows_sys::Win32::System::Memory::UnmapViewOfFile(self.at) };
    }
}

/// The bytes a library is made from: the host reads them for as long as
/// the library lives, so they are kept beside it.
enum Blob {
    /// No file (an empty library).
    None,
    /// The file read into memory (`SCARLETT_LIBRARY_MAP=0`, tests, or a file
    /// that could not be mapped).
    Held(Vec<u8>),
    /// The file itself, mapped.
    Mapped(Mapped),
}

impl Blob {
    /// The library file in `dir`: a library a save left beside it
    /// ([`NEXT_FILE`]) takes its place first.
    fn of(dir: &std::path::Path) -> Blob {
        let path = dir.join(LIBRARY_FILE);
        let next = dir.join(NEXT_FILE);
        if next.exists() && std::fs::rename(&next, &path).is_err() {
            tracing::warn!("the host pipeline library saved last could not take the old one's place");
        }
        if mapped()
            && let Some(file) = Mapped::open(&path)
        {
            return Blob::Mapped(file);
        }
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => Blob::Held(bytes),
            _ => Blob::None,
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Blob::None => &[],
            Blob::Held(bytes) => bytes,
            Blob::Mapped(file) => file.bytes(),
        }
    }
}

/// The mapped library file's view (address, bytes; 0: not mapped), when a
/// pipeline was last loaded from the library (`crate::host::now_ms`), and
/// whether the view's pages were let go since ([`trim`]).
static VIEW: [std::sync::atomic::AtomicUsize; 2] = [const { std::sync::atomic::AtomicUsize::new(0) }; 2];
static LOADED_AT: AtomicU64 = AtomicU64::new(0);
static TRIMMED: AtomicBool = AtomicBool::new(false);
/// The bytes of the view let go so far, for the memory line.
pub(crate) static TRIMMED_BYTES: AtomicU64 = AtomicU64::new(0);
/// When the view's pages were last let go (`crate::host::now_ms`), and how
/// often and when a save of this run last renamed a new library over the
/// mapped file: for [`removal_note`].
static TRIMMED_AT: AtomicU64 = AtomicU64::new(0);
static REPLACED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The library calls running now: thread, what (load, make, store,
/// serialise), the pipeline, and since when (`crate::host::now_us`).
static CALLS: Mutex<Vec<(u32, &'static str, String, u64)>> = Mutex::new(Vec::new());

thread_local! {
    /// The pipeline this thread is making (its shaders), for [`CALLS`].
    static LABEL: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Names the pipeline this thread makes next.
pub(crate) fn label(text: String) {
    LABEL.with(|l| *l.borrow_mut() = text);
}

/// A library call of this thread, listed in [`CALLS`] until dropped.
struct Call(u32, u64);

fn call(what: &'static str, name: &str) -> Call {
    let (thread, since) = (crate::host::thread_id(), crate::host::now_us());
    let label = LABEL.with(|l| l.borrow().clone());
    CALLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((thread, what, format!("{name} {label}"), since));
    Call(thread, since)
}

impl Drop for Call {
    fn drop(&mut self) {
        CALLS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|c| (c.0, c.3) != (self.0, self.1));
    }
}

/// For the report when the host device is removed: the library calls
/// running at that moment, whether this run replaced the mapped file and
/// let its pages go, and how many pages of the mapping read as zeros now.
/// A library holds no page of zeros (the 259 MB one of run 83 has no run of
/// 2,048 zero bytes): each such page is one whose contents were lost (see
/// the module's notes), and a pipeline stored there is what the driver was
/// handed.
pub(crate) fn removal_note() -> String {
    use std::fmt::Write;
    let now = crate::host::now_us();
    let mut text = String::from("pipeline library: ");
    {
        let calls = CALLS.lock().unwrap_or_else(|e| e.into_inner());
        if calls.is_empty() {
            text.push_str("no call running");
        }
        for (i, (thread, what, name, since)) in calls.iter().enumerate() {
            let _ = write!(
                text,
                "{}{what} of {name} running for {:.1} ms on thread {thread}",
                if i == 0 { "" } else { ", " },
                now.saturating_sub(*since) as f64 / 1000.0
            );
        }
    }
    let (at, len) = (VIEW[0].load(Ordering::Relaxed), VIEW[1].load(Ordering::Relaxed));
    if at == 0 {
        text.push_str("; its file is not mapped");
        return text;
    }
    let ago = |ms: u64| crate::host::now_ms().saturating_sub(ms) as f64 / 1000.0;
    let _ = write!(
        text,
        "; its file is mapped ({len} bytes), the handle {}",
        if holds() { "kept open" } else { "closed (SCARLETT_LIBRARY_HANDLE=0)" }
    );
    match REPLACED[0].load(Ordering::Relaxed) {
        0 => text.push_str("; this run has not replaced the file"),
        n => {
            let _ = write!(
                text,
                "; this run replaced the file under the mapping {n} times, last {:.1} s ago",
                ago(REPLACED[1].load(Ordering::Relaxed))
            );
        }
    }
    if TRIMMED_BYTES.load(Ordering::Relaxed) > 0 {
        let _ = write!(text, "; its pages were last let go {:.1} s ago", ago(TRIMMED_AT.load(Ordering::Relaxed)));
    }
    // Within two seconds: pages not in memory come from the disk.
    let started = std::time::Instant::now();
    // SAFETY: the view lives as long as the library, which is never dropped.
    let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, len) };
    let (mut zero, mut looked, mut first) = (0u64, 0u64, None);
    for (n, page) in bytes.chunks(4096).enumerate() {
        if n % 4096 == 0 && started.elapsed() > std::time::Duration::from_secs(2) {
            break;
        }
        looked += 1;
        if page.iter().all(|&b| b == 0) {
            zero += 1;
            if first.is_none() {
                first = Some(n);
            }
        }
    }
    let _ = write!(text, "; {zero} of the {looked} pages looked at read as zeros now");
    if let Some(first) = first {
        let _ = write!(text, " (the first is page {first}): the library's contents were lost there");
    }
    text
}

/// Lets the mapped library file's pages go from the process's memory once
/// no pipeline has been loaded for ten seconds. The host reads nearly the
/// whole file while the early pipelines load (run 65: all 1,490 MB of it
/// were in the process's memory a minute in, and 1,300 MB still at the
/// end, on a PC with 2 GB free); after that it reads the file again only
/// when a new pipeline loads. Pages let go stay in Windows' file cache
/// while nothing else wants the memory, and come back on the next read.
/// Called once a second by the sweep; does its work on a thread of its own
/// (hundreds of thousands of pages), once after each burst of loads.
/// `SCARLETT_LIBRARY_TRIM=0`: the pages stay until Windows takes them.
pub(crate) fn trim() {
    static ON: OnceLock<bool> = OnceLock::new();
    let (at, len) = (VIEW[0].load(Ordering::Relaxed), VIEW[1].load(Ordering::Relaxed));
    if at == 0
        || TRIMMED.load(Ordering::Relaxed)
        || !*ON.get_or_init(|| std::env::var_os("SCARLETT_LIBRARY_TRIM").is_none_or(|v| v != "0"))
    {
        return;
    }
    let quiet_ms = crate::host::ram_idle_ms(crate::host::available_physical_ram());
    if crate::host::now_ms() < LOADED_AT.load(Ordering::Relaxed) + quiet_ms
        || TRIMMED.swap(true, Ordering::Relaxed)
    {
        return;
    }
    let work = move || {
        // SAFETY: the view lives as long as the library, which is never
        // dropped. Unlocking pages that are not locked fails, as expected,
        // and takes them out of the working set: the documented way to let
        // pages of a mapped file go.
        unsafe { windows_sys::Win32::System::Memory::VirtualUnlock(at as *const _, len) };
        TRIMMED_BYTES.fetch_add(len as u64, Ordering::Relaxed);
        TRIMMED_AT.store(crate::host::now_ms(), Ordering::Relaxed);
        crate::host::trail(|| format!("pipeline library: the mapped file's pages let go from memory ({len} bytes)"));
    };
    if std::thread::Builder::new().name("scarlett-library-trim".into()).spawn(work).is_err() {
        TRIMMED.store(false, Ordering::Relaxed);
    }
}

struct Library {
    library: ID3D12PipelineLibrary1,
    /// The blob the library was made from: it must outlive the library.
    _blob: Blob,
    /// Pipelines stored since the last save.
    added: AtomicU64,
    /// Stores share it and a save takes it alone: the size the library
    /// asks for must still be its size when it is written out.
    saving: RwLock<()>,
}

// SAFETY: D3D12 objects are free-threaded, and the library synchronises
// its own calls (see the module's notes); what it does not, two loads of
// one name, is serialised by `same_name`.
unsafe impl Send for Library {}
// SAFETY: as above; the other fields are an atomic and a lock.
unsafe impl Sync for Library {}

/// Held by a load of the pipeline named `name`: loads of the same name
/// wait for each other (the host's one exception to the library's thread
/// safety: such a load may change that pipeline's entry). Names share 64
/// locks, so two different names seldom wait.
fn same_name(name: &str) -> MutexGuard<'static, ()> {
    static LOADING: [Mutex<()>; 64] = [const { Mutex::new(()) }; 64];
    let at = name.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(usize::from(b)));
    LOADING[at % LOADING.len()].lock().unwrap_or_else(|e| e.into_inner())
}

/// With `SCARLETT_LIBRARY_LOCK=1`: held by every library call, as before.
fn one_call() -> Option<MutexGuard<'static, ()>> {
    static ON: OnceLock<bool> = OnceLock::new();
    static CALLING: Mutex<()> = Mutex::new(());
    ON.get_or_init(|| std::env::var_os("SCARLETT_LIBRARY_LOCK").is_some_and(|v| v == "1"))
        .then(|| CALLING.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Whether mesh pipelines are made one at a time on `device` (see the
/// module's notes): on AMD cards unless `SCARLETT_MESH_MAKE_ONE=0`, on every
/// card with `=1`.
pub(crate) fn mesh_one_at_a_time(device: &ID3D12Device) -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let switch = std::env::var("SCARLETT_MESH_MAKE_ONE").ok();
        let on = one_at_a_time_for(switch.as_deref(), || crate::indirect::amd(device));
        tracing::info!(on, "mesh pipelines made one at a time (SCARLETT_MESH_MAKE_ONE)");
        on
    })
}

/// The switch's value (`None`: unset), and whether the card is AMD's.
fn one_at_a_time_for(switch: Option<&str>, amd: impl FnOnce() -> bool) -> bool {
    match switch {
        Some("0") => false,
        Some("1") => true,
        _ => amd(),
    }
}

/// Runs `make`, alone among the makes that are also `alone`.
pub(crate) fn made_alone<T>(alone: bool, make: impl FnOnce() -> T) -> T {
    static MAKING: Mutex<()> = Mutex::new(());
    let _one = alone.then(|| MAKING.lock().unwrap_or_else(|e| e.into_inner()));
    make()
}

/// Whether a save may rename the new library over the library's file,
/// which this run has `mapped`: not by default (see the module's notes);
/// `SCARLETT_LIBRARY_REPLACE_MAPPED=1` does, as before.
fn replaces_mapped(mapped: bool) -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    !mapped
        || *ON.get_or_init(|| std::env::var_os("SCARLETT_LIBRARY_REPLACE_MAPPED").is_some_and(|v| v == "1"))
}

const LIBRARY_FILE: &str = "host-pipelines.bin";
/// A saved library that could not replace [`LIBRARY_FILE`] while this run
/// had it mapped (a system that refuses to replace a mapped file): the next
/// start renames it over the library first ([`Blob::of`]).
const NEXT_FILE: &str = "host-pipelines.next";
const EARLY_FILE: &str = "early-pipelines.txt";
/// Title pipelines listed for an early start, at most.
const EARLY_MAX: usize = 100_000;

fn library() -> Option<&'static Library> {
    static LIBRARY: OnceLock<Option<Library>> = OnceLock::new();
    LIBRARY
        .get_or_init(|| {
            let device = crate::host::host()?.device.cast::<ID3D12Device1>().ok()?;
            let blob = DIR.get().map_or(Blob::None, |d| Blob::of(d));
            let library = open(&device, blob);
            if library.is_none() {
                tracing::warn!("no host pipeline library: pipelines are not kept");
            } else {
                start_saver();
            }
            library
        })
        .as_ref()
}

/// The library in `blob`, or an empty one if the host rejects the blob (an
/// adapter or driver change, damage).
fn open(device: &ID3D12Device1, blob: Blob) -> Option<Library> {
    let make = |bytes: &[u8]| -> windows::core::Result<ID3D12PipelineLibrary1> {
        // SAFETY: the bytes outlive the library (kept beside it).
        unsafe {
            device.CreatePipelineLibrary::<ID3D12PipelineLibrary>(bytes)?.cast()
        }
    };
    if !blob.bytes().is_empty() {
        match make(blob.bytes()) {
            Ok(library) => {
                let bytes = blob.bytes().len();
                BLOB.store(bytes as u64, Ordering::Relaxed);
                tracing::info!(bytes, mapped = matches!(blob, Blob::Mapped(_)), "host pipeline library loaded");
                if let Blob::Mapped(file) = &blob {
                    VIEW[1].store(file.len, Ordering::Relaxed);
                    VIEW[0].store(file.at.Value as usize, Ordering::Relaxed);
                    LOADED_AT.store(crate::host::now_ms(), Ordering::Relaxed);
                }
                return Some(Library {
                    library,
                    _blob: blob,
                    added: AtomicU64::new(0),
                    saving: RwLock::new(()),
                });
            }
            Err(e) => tracing::info!(error = %e, "host pipeline library rejected; starting an empty one"),
        }
    } else {
        tracing::info!("no host pipeline library yet; starting an empty one");
    }
    // The rejected bytes are let go (a mapped file is unmapped) before the
    // empty library is made: nothing reads them any more.
    drop(blob);
    let library = make(&[]).ok()?;
    Some(Library {
        library,
        _blob: Blob::None,
        added: AtomicU64::new(0),
        saving: RwLock::new(()),
    })
}

/// A pipeline's name in the library: the hash of what its host
/// description holds.
pub(crate) fn name(kind: &str, root: &[u8], code: &[&[u8]], state: &str) -> String {
    let mut h = blake3::Hasher::new();
    for part in [kind.as_bytes(), root].into_iter().chain(code.iter().copied()).chain([state.as_bytes()]) {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part);
    }
    h.finalize().to_hex()[..32].to_owned()
}

/// The pipeline named `name`: loaded from the library by `load` if it is
/// there with the same description, else made by `create` and stored.
/// A name the library already holds with a description that differs (an
/// entry an earlier build stored: entries are never replaced) can never
/// be stored again, so such a pipeline is stored and looked up under
/// [`second_name`] as well.
pub(crate) fn pipeline(
    name: &str,
    load: impl Fn(&ID3D12PipelineLibrary1, PCWSTR) -> windows::core::Result<ID3D12PipelineState>,
    create: impl FnOnce() -> windows::core::Result<ID3D12PipelineState>,
) -> windows::core::Result<ID3D12PipelineState> {
    let wide = HSTRING::from(name);
    let Some(library) = library() else {
        MADE.fetch_add(1, Ordering::Relaxed);
        return create();
    };
    let began = std::time::Instant::now();
    let loaded = {
        let _all = one_call();
        let _one = same_name(name);
        let _call = call("load", name);
        load(&library.library, PCWSTR(wide.as_ptr())).or_else(|first| {
            let second = HSTRING::from(second_name(name));
            load(&library.library, PCWSTR(second.as_ptr())).map_err(|_| first)
        })
    };
    // The load read the library's file (see `trim`).
    LOADED_AT.store(crate::host::now_ms(), Ordering::Relaxed);
    TRIMMED.store(false, Ordering::Relaxed);
    let refused = match loaded {
        Ok(pso) => {
            LOADED.fetch_add(1, Ordering::Relaxed);
            crate::host::trail(|| {
                let label = LABEL.with(|l| l.borrow().clone());
                format!("pipeline library: loaded {label} in {:.1} ms", began.elapsed().as_secs_f64() * 1e3)
            });
            return Ok(pso);
        }
        Err(e) => e,
    };
    // Compiled outside any lock: compiles take up to seconds.
    let pso = {
        let _call = call("make", name);
        create()?
    };
    MADE.fetch_add(1, Ordering::Relaxed);
    let stored = {
        let _all = one_call();
        let _stores = library.saving.read().unwrap_or_else(|e| e.into_inner());
        let _call = call("store", name);
        // SAFETY: a live pipeline; a name already stored (a description that
        // differs) is refused, which leaves the library as it was.
        let stored = unsafe { library.library.StorePipeline(PCWSTR(wide.as_ptr()), &pso) };
        match &stored {
            Err(e) if e.code() == windows::Win32::Foundation::E_INVALIDARG => {
                let second = HSTRING::from(second_name(name));
                // SAFETY: as above.
                let again = unsafe { library.library.StorePipeline(PCWSTR(second.as_ptr()), &pso) };
                if again.is_ok() {
                    library.added.fetch_add(1, Ordering::Relaxed);
                }
            }
            _ => {}
        }
        stored
    };
    if stored.is_ok() {
        library.added.fetch_add(1, Ordering::Relaxed);
    }
    let why = why_made(&refused, &stored);
    if matches!(why, Why::Differs) {
        DIFFERED.fetch_add(1, Ordering::Relaxed);
        let label = LABEL.with(|l| l.borrow().clone());
        tracing::warn!(
            pipeline = %label,
            name,
            "pipeline library: this name is stored with a description that differs; made again and stored under a second name"
        );
    }
    crate::host::trail(|| {
        let label = LABEL.with(|l| l.borrow().clone());
        format!(
            "pipeline library: made {label} in {:.1} ms ({})",
            began.elapsed().as_secs_f64() * 1e3,
            why.text(&refused, &stored)
        )
    });
    Ok(pso)
}

/// The name a pipeline is also stored under when its own name is taken by
/// a description that differs.
fn second_name(name: &str) -> String {
    format!("{name}+")
}

/// Pipelines whose name was in the library with a description that differs
/// from the one asked for: the library refused them although it holds them.
pub(crate) static DIFFERED: AtomicU64 = AtomicU64::new(0);

/// Why a pipeline was made rather than loaded. A load answers E_INVALIDARG
/// both when the name is not in the library and when the description
/// differs from the stored one (the documented meanings); the store that
/// follows tells them apart, since it answers E_INVALIDARG only for a name
/// that is already there.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Why {
    /// Not in the library yet: the first time this pipeline is made.
    New,
    /// In the library under this name, stored with another description.
    Differs,
    /// The load failed for another reason.
    Other,
}

pub(crate) fn why_made(load: &windows::core::Error, store: &windows::core::Result<()>) -> Why {
    use windows::Win32::Foundation::E_INVALIDARG;
    if load.code() != E_INVALIDARG {
        return Why::Other;
    }
    match store {
        Ok(()) => Why::New,
        Err(e) if e.code() == E_INVALIDARG => Why::Differs,
        Err(_) => Why::Other,
    }
}

impl Why {
    fn text(&self, load: &windows::core::Error, store: &windows::core::Result<()>) -> String {
        let kept = match store {
            Ok(()) => "stored for the next start".to_owned(),
            Err(e) => format!("not stored: {e}"),
        };
        match self {
            Why::New => format!("not in the library yet; {kept}"),
            Why::Differs => format!(
                "the library holds this name with a description that differs from this one, so it refused it ({load}); stored under a second name for the next start"
            ),
            Why::Other => format!("the library did not load it: {load}; {kept}"),
        }
    }
}

/// How often the saver thread looks for pipelines not yet on disk
/// (`SCARLETT_LIBRARY_SAVE_S`, seconds; 0 turns it off).
fn saver_every() -> Option<std::time::Duration> {
    let s = std::env::var("SCARLETT_LIBRARY_SAVE_S").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(30);
    (s > 0).then(|| std::time::Duration::from_secs(s))
}

/// Whether the saver thread writes the library now: something was added and
/// either enough pipelines are waiting or the last save is old enough.
pub(crate) fn saver_due(added: u64, since_save: std::time::Duration, every: std::time::Duration) -> bool {
    const ENOUGH: u64 = 64;
    added > 0 && (added >= ENOUGH || since_save >= every)
}

/// A thread that keeps the library on disk while the title runs. The
/// workers' saves skip pipelines added within 20 s of the last save and
/// pipelines made off the workers, and the save at a clean exit never
/// comes when the game is killed (bench, the launcher's Stop, a crash):
/// those pipelines were made again on every start.
fn start_saver() {
    let Some(every) = saver_every() else { return };
    let spawned = std::thread::Builder::new().name("scarlett-library-saver".into()).spawn(move || {
        // SAFETY: a plain call on our own thread.
        unsafe {
            use windows::Win32::System::Threading::*;
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST);
        }
        let tick = std::time::Duration::from_secs(5).min(every);
        let mut last = std::time::Instant::now();
        loop {
            std::thread::sleep(tick);
            let Some(library) = library() else { return };
            if saver_due(library.added.load(Ordering::Relaxed), last.elapsed(), every) {
                save(true);
                last = std::time::Instant::now();
            }
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "no pipeline library saver");
    }
}

/// Saves what the library holds once the host device is removed: the
/// process usually ends soon after, without a clean exit.
pub(crate) fn save_on_removal() {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !DONE.swap(true, Ordering::Relaxed) {
        save(true);
    }
}

/// Writes the library and the early list if pipelines were added since the
/// last save and `SAVE_EVERY` has passed (or `now`).
pub(crate) fn save(now: bool) {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    const SAVE_EVERY: std::time::Duration = std::time::Duration::from_secs(20);
    let (Some(dir), Some(library)) = (DIR.get(), library()) else { return };
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if !now && last.is_some_and(|t| t.elapsed() < SAVE_EVERY) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!(error = %e, "host pipeline library not saved");
        return;
    }
    let tmp = dir.join(LIBRARY_FILE).with_extension("tmp");
    // The bytes of the library written to `tmp`; 0 when no pipeline was
    // added: the library on disk is then what it was, and only the early
    // list is written (a title pipeline first drawn with wrote all 1,352 MB
    // of the library again: run 60's second save).
    let bytes = {
        let _all = one_call();
        // No store between asking the size and writing; loads go on.
        let _alone = library.saving.write().unwrap_or_else(|e| e.into_inner());
        let added = library.added.load(Ordering::Relaxed);
        if !USED_CHANGED.swap(false, Ordering::Relaxed) && added == 0 {
            return;
        }
        if added == 0 {
            0
        } else {
            // SAFETY: a plain query.
            let size = unsafe { library.library.GetSerializedSize() };
            let _call = call("serialise", "the whole library");
            if let Err(e) = serialise(&library.library, &tmp, size) {
                tracing::warn!(error = %e, "host pipeline library not serialised");
                let _ = std::fs::remove_file(&tmp);
                return;
            }
            library.added.store(0, Ordering::Relaxed);
            SAVED.store(size as u64, Ordering::Relaxed);
            size
        }
    };
    // Through `used`: a process that asked for none yet (`scarlett shaders`)
    // must not write the earlier runs' list over with nothing.
    let used: Vec<String> = used().iter().flatten().cloned().collect();
    let in_use = VIEW[0].load(Ordering::Relaxed) != 0;
    let library_written = if bytes == 0 {
        Ok(())
    } else if replaces_mapped(in_use) {
        replace_library(dir, &tmp)
    } else {
        std::fs::rename(&tmp, dir.join(NEXT_FILE))
    };
    if bytes > 0 && library_written.is_ok() {
        // In place unless the system refused and the new one waits beside it.
        let in_place = !dir.join(NEXT_FILE).exists();
        if in_place && in_use {
            REPLACED[1].store(crate::host::now_ms(), Ordering::Relaxed);
            REPLACED[0].fetch_add(1, Ordering::Relaxed);
        }
        crate::host::trail(|| {
            format!(
                "pipeline library: saved {bytes} bytes, {}",
                if in_place { "renamed over the library's file" } else { "left beside the library's file for the next start" }
            )
        });
    }
    let slow: Vec<String> = slow_ones().iter().flatten().map(|(key, ms)| format!("{key} {ms}")).collect();
    let written = library_written
        .and_then(|()| write_atomically(&dir.join(SLOW_FILE), slow.join("\n").as_bytes()))
        .and_then(|()| write_atomically(&dir.join(EARLY_FILE), used.join("\n").as_bytes()));
    match written {
        Ok(()) => tracing::info!(
            bytes,
            early = used.len(),
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            differed = DIFFERED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipeline library saved"
        ),
        Err(e) => tracing::warn!(error = %e, "host pipeline library not saved"),
    }
}

/// Writes the library's `size` serialised bytes to the file `tmp`: through
/// a mapping of that file, so that the bytes are the file's pages and not
/// 1.3 GB more of the process's own memory for the seconds a save takes;
/// through a buffer where the file cannot be mapped (and with
/// `SCARLETT_LIBRARY_MAP=0`).
fn serialise(library: &ID3D12PipelineLibrary1, tmp: &std::path::Path, size: usize) -> std::io::Result<()> {
    let failed = |e: windows::core::Error| std::io::Error::other(e.to_string());
    if mapped()
        && let Some(mut file) = Mapped::create(tmp, size)
    {
        // SAFETY: a buffer of the size the library asks for.
        return unsafe { library.Serialize(file.bytes_mut()) }.map_err(failed);
    }
    let mut bytes = vec![0u8; size];
    // SAFETY: a buffer of the size the library asks for.
    unsafe { library.Serialize(&mut bytes) }.map_err(failed)?;
    std::fs::write(tmp, &bytes)
}

/// Renames the written library `tmp` over the library's file in `dir`. This
/// run may have that file mapped: Windows 11 replaces a mapped file, and
/// the mapping goes on reading the old one for as long as a handle on it
/// is open ([`holds`]; with none, only the pages in memory at that moment
/// stay: the rest read as zeros). Where a system refuses, the new library
/// is left beside it as [`NEXT_FILE`] for the next start.
fn replace_library(dir: &std::path::Path, tmp: &std::path::Path) -> std::io::Result<()> {
    std::fs::rename(tmp, dir.join(LIBRARY_FILE)).or_else(|_| std::fs::rename(tmp, dir.join(NEXT_FILE)))
}

/// Writes `bytes` to a temporary file beside `path`, then renames it over
/// `path`: a reader never sees a part. Serialised so concurrent worker and
/// shutdown saves do not race on the same staging file or target rename.
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::AtomicU64;
    static WRITING: Mutex<()> = Mutex::new(());
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let _guard = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp-{}-{n}", std::process::id()));
    let tmp = path.with_file_name(name);
    let res = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// How long each title pipeline that took long to make took, in ms (beside
/// the early list; a key may be there more than once).
const SLOW_FILE: &str = "slow-pipelines.txt";
/// A pipeline that takes this long is one ([`slow`]).
pub(crate) const SLOW_MS: u64 = 1000;
static SLOW: Mutex<Option<std::collections::HashMap<String, u64>>> = Mutex::new(None);

fn slow_ones() -> std::sync::MutexGuard<'static, Option<std::collections::HashMap<String, u64>>> {
    let mut slow = SLOW.lock().unwrap_or_else(|e| e.into_inner());
    if slow.is_none() {
        let listed = DIR.get().and_then(|d| std::fs::read_to_string(d.join(SLOW_FILE)).ok()).unwrap_or_default();
        *slow = Some(
            listed
                .lines()
                .filter_map(|l| l.split_once(' '))
                .filter_map(|(key, ms)| Some((key.to_owned(), ms.parse().ok()?)))
                .collect(),
        );
    }
    slow
}

/// Whether making the title pipeline with `key` took [`SLOW_MS`] in this or
/// an earlier run.
pub(crate) fn slow(key: &str) -> bool {
    slow_ones().as_ref().is_some_and(|s| s.contains_key(key))
}

/// The title pipeline with `key` took `ms` to make.
pub(crate) fn note_time(key: &str, ms: u64) {
    if ms < SLOW_MS {
        return;
    }
    let mut slow = slow_ones();
    let known = slow.get_or_insert_default().entry(key.to_owned()).or_insert(0);
    if ms > *known {
        *known = ms;
        // With the next save of the early list.
        USED_CHANGED.store(true, Ordering::Relaxed);
    }
}

/// Title pipeline keys requested by draws or dispatches (this run and the last).
static USED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
static USED_CHANGED: AtomicBool = AtomicBool::new(false);

fn used() -> std::sync::MutexGuard<'static, Option<HashSet<String>>> {
    let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
    if used.is_none() {
        let listed = DIR
            .get()
            .and_then(|d| std::fs::read_to_string(d.join(EARLY_FILE)).ok())
            .unwrap_or_default();
        let set: HashSet<String> = listed.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect();
        tracing::info!(pipelines = set.len(), "title pipelines to start early");
        *used = Some(set);
    }
    used
}

/// Whether the title pipeline with `key` was requested in this or an earlier run.
///
/// `SCARLETT_EARLY_ALL=1`: every title pipeline is started at creation,
/// requested before or not, so that the driver's own cache holds it for
/// later runs.
pub(crate) fn wanted_early(key: &str) -> bool {
    static ALL: OnceLock<bool> = OnceLock::new();
    if *ALL.get_or_init(|| std::env::var_os("SCARLETT_EARLY_ALL").is_some_and(|v| v == "1")) {
        return true;
    }
    used().as_ref().is_some_and(|s| s.contains(key))
}

/// A draw or dispatch requested the title pipeline with `key`.
pub(crate) fn note_used(key: &str) {
    let mut used = used();
    let set = used.get_or_insert_with(HashSet::new);
    if set.len() < EARLY_MAX && set.insert(key.to_owned()) {
        USED_CHANGED.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesh_makes_are_one_at_a_time_on_amd_unless_switched() {
        assert!(one_at_a_time_for(None, || true));
        assert!(!one_at_a_time_for(None, || false));
        assert!(!one_at_a_time_for(Some("0"), || true));
        assert!(one_at_a_time_for(Some("1"), || false));
        assert!(one_at_a_time_for(Some("yes"), || true));
    }

    #[test]
    fn made_alone_makes_do_not_overlap() {
        use std::sync::atomic::AtomicUsize;
        static NOW: AtomicUsize = AtomicUsize::new(0);
        static MOST: AtomicUsize = AtomicUsize::new(0);
        let threads: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..20 {
                        made_alone(true, || {
                            let n = NOW.fetch_add(1, Ordering::SeqCst) + 1;
                            MOST.fetch_max(n, Ordering::SeqCst);
                            std::thread::sleep(std::time::Duration::from_millis(1));
                            NOW.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(MOST.load(Ordering::SeqCst), 1);
        assert_eq!(made_alone(false, || 7), 7);
    }

    #[test]
    fn a_library_not_mapped_is_replaced() {
        assert!(replaces_mapped(false));
    }

    #[test]
    fn names_change_with_what_goes_into_the_pipeline() {
        let a = name("graphics", b"root", &[b"vs", b"ps"], "state");
        assert_eq!(a, name("graphics", b"root", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps2"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps"], "state2"));
        assert_ne!(a, name("graphics", b"root2", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("mesh", b"root", &[b"vs", b"ps"], "state"));
        // Parts do not run into each other.
        assert_ne!(a, name("graphics", b"root", &[b"vsp", b"s"], "state"));
    }

    #[test]
    fn the_saver_writes_what_a_killed_run_would_lose() {
        use std::time::Duration;
        let every = Duration::from_secs(30);
        // Nothing new: never written, however long ago the last save was.
        assert!(!saver_due(0, Duration::from_secs(3600), every));
        // A few new pipelines: written once the interval has passed.
        assert!(!saver_due(3, Duration::from_secs(10), every));
        assert!(saver_due(3, every, every));
        // Many new pipelines: written without waiting for the interval.
        assert!(saver_due(64, Duration::ZERO, every));
    }

    #[test]
    fn a_rejected_blob_gives_an_empty_library() {
        let Some(device) = crate::host::host().and_then(|h| h.device.cast::<ID3D12Device1>().ok()) else {
            return; // no host GPU here
        };
        let library = open(&device, Blob::Held(b"not a pipeline library".repeat(16))).expect("an empty library");
        // SAFETY: a plain query on a live library.
        assert!(unsafe { library.library.GetSerializedSize() } > 0);
        assert!(matches!(library._blob, Blob::None));
    }

    /// A mapped file reads as the file's bytes, a file written through a
    /// mapping holds what was written, and the library's file can be
    /// replaced while it is mapped (or the new one waits as the next).
    #[test]
    fn the_library_file_is_mapped_and_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LIBRARY_FILE);
        assert!(matches!(Blob::of(dir.path()), Blob::None), "no file yet");
        std::fs::write(&path, b"").unwrap();
        assert!(matches!(Blob::of(dir.path()), Blob::None), "an empty file");
        let old: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        std::fs::write(&path, &old).unwrap();
        let blob = Blob::of(dir.path());
        assert!(matches!(blob, Blob::Mapped(_)) || !mapped());
        assert_eq!(blob.bytes(), &old[..]);
        // A save while the old file is mapped.
        let tmp = path.with_extension("tmp");
        let new: Vec<u8> = (0..300_000u32).map(|i| (i * 7) as u8).collect();
        {
            let mut file = Mapped::create(&tmp, new.len()).expect("a new file, mapped");
            file.bytes_mut().copy_from_slice(&new);
        }
        assert_eq!(std::fs::read(&tmp).unwrap(), new);
        replace_library(dir.path(), &tmp).unwrap();
        assert!(!tmp.exists());
        // The mapping still reads the old library.
        assert_eq!(blob.bytes(), &old[..]);
        drop(blob);
        // The next start reads the new one, wherever the save left it.
        assert_eq!(Blob::of(dir.path()).bytes(), &new[..]);
        assert!(!dir.path().join(NEXT_FILE).exists());
    }

    /// The bytes at `at` of the pattern the test below writes.
    fn pattern(at: usize, out: &mut [u8]) {
        for (i, word) in out.chunks_exact_mut(4).enumerate() {
            let v = ((at / 4 + i) as u32).wrapping_mul(2_654_435_761).wrapping_add(1);
            word.copy_from_slice(&v.to_le_bytes());
        }
    }

    /// A file of `len` bytes (whole MiB) of [`pattern`], written past the
    /// system's cache: none of its pages is in memory afterwards. False
    /// where the folder's file system does not take such writes.
    fn write_uncached(path: &std::path::Path, len: usize) -> bool {
        use std::io::Write;
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH
        const UNCACHED: u32 = 0x2000_0000 | 0x8000_0000;
        const CHUNK: usize = 1 << 20;
        let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(UNCACHED)
            .open(path)
        else {
            return false;
        };
        // Such writes want a buffer on a sector boundary.
        let mut store = vec![0u8; CHUNK + 4096];
        let start = store.as_ptr().align_offset(4096);
        for at in (0..len).step_by(CHUNK) {
            let chunk = &mut store[start..start + CHUNK];
            pattern(at, chunk);
            if file.write_all(chunk).is_err() {
                return false;
            }
        }
        true
    }

    /// The device loss of runs 72, 73 and 82: the library's file is
    /// replaced while it is mapped, and pages of the mapping that were not
    /// in memory then are read afterwards (in the runs: pages the trim let
    /// go and Windows gave to something else). With the file's handle kept
    /// open they still read as the old file. (With it closed, as before,
    /// all but the first few read as zeros on the PC this was found on.)
    #[test]
    fn a_replaced_library_file_still_reads_from_the_disk() {
        const LEN: usize = 8 << 20;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LIBRARY_FILE);
        if !write_uncached(&path, LEN) {
            return; // no uncached writes here: nothing to show
        }
        let mapped = Mapped::open_holding(&path, true).expect("the file, mapped");
        // A save: the new library, renamed over the mapped one.
        let tmp = path.with_extension("tmp");
        {
            let mut file = Mapped::create(&tmp, LEN).expect("a new file, mapped");
            file.bytes_mut().fill(0xEE);
        }
        replace_library(dir.path(), &tmp).unwrap();
        // Every page of the old mapping, read for the first time now.
        let mut expected = vec![0u8; 4096];
        for (page, bytes) in mapped.bytes().chunks(4096).enumerate() {
            pattern(page * 4096, &mut expected);
            assert!(bytes == &expected[..], "page {page} of the replaced library no longer reads as the old file");
        }
    }

    #[test]
    fn concurrent_write_atomically_never_fails_or_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("host-pipelines.bin");
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let target = target.clone();
                std::thread::spawn(move || {
                    let payload = vec![b'a' + t as u8; 4096];
                    for _ in 0..30 {
                        write_atomically(&target, &payload).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let final_bytes = std::fs::read(&target).unwrap();
        assert_eq!(final_bytes.len(), 4096);
        assert!(final_bytes.iter().all(|&b| b == final_bytes[0]));
    }
}

#[cfg(test)]
mod loads {
    //! Each kind of pipeline the host makes (graphics, compute, a mesh
    //! stream) is stored, then loaded with the very description it is made
    //! with, from the live library and from one made again from its saved
    //! bytes; a name not in the library and a description that differs are
    //! told apart as `pipeline` tells them apart. Skipped without an adapter
    //! (mesh: without a mesh-shader adapter or dxc).
    use super::{Why, second_name, why_made};
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL_12_0, ID3DBlob};
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::core::{HSTRING, Interface, PCSTR, PCWSTR, s};

    const GRAPHICS: &str = r#"
#define RS "RootFlags(0), RootConstants(num32BitConstants=4, b0)"
cbuffer C : register(b0) { float4 tint; };
[RootSignature(RS)] float4 vs(uint i : SV_VertexID) : SV_Position { return float4(i & 1, i >> 1, 0, 1); }
[RootSignature(RS)] float4 ps() : SV_Target { return tint; }
"#;
    const COMPUTE: &str = r#"
RWByteAddressBuffer o : register(u0);
[RootSignature("UAV(u0)")] [numthreads(64, 1, 1)] void cs(uint i : SV_DispatchThreadID) { o.Store(i * 4, i); }
[RootSignature("UAV(u0)")] [numthreads(64, 1, 1)] void cs2(uint i : SV_DispatchThreadID) { o.Store(i * 4, i * 2); }
"#;
    const MESH: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0)"
cbuffer C : register(b0) { float4 tint; };
struct V { float4 at : SV_Position; };
[RootSignature(RS)] [outputtopology("triangle")] [numthreads(1, 1, 1)]
void ms(out vertices V v[3], out indices uint3 t[1]) {
    SetMeshOutputCounts(3, 1);
    v[0].at = float4(0, 0, 0, 1); v[1].at = float4(1, 0, 0, 1); v[2].at = float4(0, 1, 0, 1);
    t[0] = uint3(0, 1, 2);
}
[RootSignature(RS)] float4 ps(V v) : SV_Target { return tint; }
"#;

    fn fxc(source: &str, entry: PCSTR, target: PCSTR) -> Vec<u8> {
        let (mut code, mut errors): (Option<ID3DBlob>, Option<ID3DBlob>) = (None, None);
        // SAFETY: the source outlives the call.
        let made = unsafe {
            D3DCompile(
                source.as_ptr().cast(),
                source.len(),
                None,
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
        made.unwrap();
        let code = code.unwrap();
        // SAFETY: the blob's own pointer and size.
        unsafe {
            std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
                .to_vec()
        }
    }

    fn dxc(source: &str, entry: &str, target: &str) -> Option<Vec<u8>> {
        let exe = std::fs::read_dir("C:/Program Files (x86)/Windows Kits/10/bin")
            .ok()?
            .flatten()
            .map(|e| e.path().join("x64/dxc.exe"))
            .filter(|p| p.exists())
            .max()?;
        let dir = tempfile::tempdir().ok()?;
        let (from, to) = (dir.path().join("a.hlsl"), dir.path().join("a.dxil"));
        std::fs::write(&from, source).ok()?;
        let out = std::process::Command::new(exe)
            .args(["-T", target, "-E", entry, "-Fo"])
            .arg(&to)
            .arg(&from)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| std::fs::read(to).ok())
            .flatten()
    }

    fn code(b: &[u8]) -> D3D12_SHADER_BYTECODE {
        D3D12_SHADER_BYTECODE {
            pShaderBytecode: b.as_ptr().cast(),
            BytecodeLength: b.len(),
        }
    }

    fn library(device: &ID3D12Device, bytes: &[u8]) -> ID3D12PipelineLibrary1 {
        // SAFETY: the bytes outlive the library (the callers keep them).
        unsafe {
            device
                .cast::<ID3D12Device1>()
                .unwrap()
                .CreatePipelineLibrary::<ID3D12PipelineLibrary>(bytes)
                .unwrap()
                .cast()
                .unwrap()
        }
    }

    fn saved(library: &ID3D12PipelineLibrary1) -> Vec<u8> {
        // SAFETY: a buffer of the size asked for.
        unsafe {
            let mut bytes = vec![0u8; library.GetSerializedSize()];
            library.Serialize(&mut bytes).unwrap();
            bytes
        }
    }

    type Load<'a> = dyn Fn(&ID3D12PipelineLibrary1, PCWSTR, bool) -> windows::core::Result<ID3D12PipelineState>
        + 'a;

    /// `load(library, name, other)` loads with the description used to make
    /// the pipeline, or with one that differs (`other`).
    fn check(
        kind: &str,
        device: &ID3D12Device,
        make: impl Fn(bool) -> ID3D12PipelineState,
        load: &Load,
    ) {
        let name = HSTRING::from(kind);
        let name = PCWSTR(name.as_ptr());
        let live = library(device, &[]);
        let pso = make(false);
        let missing = load(&live, name, false).expect_err("a name not in the library loads");
        // SAFETY: a live pipeline.
        let stored = unsafe { live.StorePipeline(name, &pso) };
        assert_eq!(why_made(&missing, &stored), Why::New, "{kind}");
        load(&live, name, false).unwrap_or_else(|e| panic!("{kind}: stored, then refused: {e}"));
        let bytes = saved(&live);
        let again = library(device, &bytes);
        load(&again, name, false).unwrap_or_else(|e| panic!("{kind}: saved, then refused: {e}"));
        let differs = load(&again, name, true).expect_err("another description loads");
        // SAFETY: as above.
        let stored = unsafe { again.StorePipeline(name, &make(false)) };
        assert_eq!(why_made(&differs, &stored), Why::Differs, "{kind}");
        // The other description under the second name, as `pipeline` keeps it.
        let second = HSTRING::from(second_name(kind));
        let second = PCWSTR(second.as_ptr());
        // SAFETY: as above.
        unsafe { again.StorePipeline(second, &make(true)) }.unwrap();
        let bytes2 = saved(&again);
        let third = library(device, &bytes2);
        load(&third, name, false).unwrap_or_else(|e| panic!("{kind}: first name refused: {e}"));
        load(&third, second, true).unwrap_or_else(|e| panic!("{kind}: second name refused: {e}"));
        drop((third, again));
        drop((bytes, bytes2));
    }

    #[test]
    fn each_kind_stores_and_loads() {
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: plain creation.
        if unsafe {
            windows::Win32::Graphics::Direct3D12::D3D12CreateDevice(
                None,
                D3D_FEATURE_LEVEL_12_0,
                &mut device,
            )
        }
        .is_err()
        {
            eprintln!("no adapter: skipped");
            return;
        }
        let device = device.unwrap();
        // SAFETY: creation from compiled code carrying its root signature.
        unsafe {
            let (vs, ps) = (
                fxc(GRAPHICS, s!("vs"), s!("vs_5_1")),
                fxc(GRAPHICS, s!("ps"), s!("ps_5_1")),
            );
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &vs).unwrap();
            let graphics = |other: bool| {
                let mut d = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    VS: code(&vs),
                    PS: code(&ps),
                    SampleMask: u32::MAX,
                    RasterizerState: D3D12_RASTERIZER_DESC {
                        FillMode: D3D12_FILL_MODE_SOLID,
                        CullMode: D3D12_CULL_MODE_NONE,
                        ..Default::default()
                    },
                    PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                    NumRenderTargets: 1,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    ..Default::default()
                };
                d.BlendState.RenderTarget[0].RenderTargetWriteMask = 0xf;
                d.RTVFormats[0] = if other {
                    DXGI_FORMAT_R16G16B16A16_FLOAT
                } else {
                    DXGI_FORMAT_R8G8B8A8_UNORM
                };
                d
            };
            check(
                "graphics",
                &device,
                |o| {
                    device
                        .CreateGraphicsPipelineState(&graphics(o))
                        .unwrap()
                },
                &|l, n, o| l.LoadGraphicsPipeline(n, &graphics(o)),
            );

            let cs = fxc(COMPUTE, s!("cs"), s!("cs_5_1"));
            let cs2 = fxc(COMPUTE, s!("cs2"), s!("cs_5_1"));
            let croot: ID3D12RootSignature = device.CreateRootSignature(0, &cs).unwrap();
            let compute = |other: bool| D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&croot),
                CS: code(if other { &cs2 } else { &cs }),
                ..Default::default()
            };
            check(
                "compute",
                &device,
                |o| device.CreateComputePipelineState(&compute(o)).unwrap(),
                &|l, n, o| l.LoadComputePipeline(n, &compute(o)),
            );

            let mut o7 = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
            let size = size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS7>() as u32;
            let tier = device
                .CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS7, (&raw mut o7).cast(), size)
                .is_ok()
                && o7.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED;
            let mesh = (dxc(MESH, "ms", "ms_6_5"), dxc(MESH, "ps", "ps_6_5"));
            let (true, (Some(ms), Some(mps))) = (tier, mesh) else {
                eprintln!("mesh: skipped (no mesh shaders or no dxc)");
                return;
            };
            #[repr(C, align(8))]
            struct Sub<T>(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE, T);
            #[repr(C)]
            struct Stream {
                root: Sub<*mut std::ffi::c_void>,
                ms: Sub<D3D12_SHADER_BYTECODE>,
                ps: Sub<D3D12_SHADER_BYTECODE>,
                raster: Sub<D3D12_RASTERIZER_DESC>,
                targets: Sub<D3D12_RT_FORMAT_ARRAY>,
                topology: Sub<D3D12_PRIMITIVE_TOPOLOGY_TYPE>,
            }
            let mroot: ID3D12RootSignature = device.CreateRootSignature(0, &ms).unwrap();
            let stream = |other: bool| {
                let mut formats = D3D12_RT_FORMAT_ARRAY {
                    NumRenderTargets: 1,
                    ..Default::default()
                };
                formats.RTFormats[0] = if other {
                    DXGI_FORMAT_R16G16B16A16_FLOAT
                } else {
                    DXGI_FORMAT_R8G8B8A8_UNORM
                };
                Stream {
                    root: Sub(
                        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE,
                        mroot.as_raw(),
                    ),
                    ms: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, code(&ms)),
                    ps: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, code(&mps)),
                    raster: Sub(
                        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER,
                        D3D12_RASTERIZER_DESC {
                            FillMode: D3D12_FILL_MODE_SOLID,
                            CullMode: D3D12_CULL_MODE_NONE,
                            ..Default::default()
                        },
                    ),
                    targets: Sub(
                        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS,
                        formats,
                    ),
                    // As `mesh_pipeline` names it: without a topology the
                    // library refuses to load a stored mesh pipeline with
                    // the very stream it was made from (E_INVALIDARG).
                    topology: Sub(
                        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY,
                        D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                    ),
                }
            };
            let desc = |s: &mut Stream| D3D12_PIPELINE_STATE_STREAM_DESC {
                SizeInBytes: size_of::<Stream>(),
                pPipelineStateSubobjectStream: std::ptr::from_mut(s).cast(),
            };
            let device2 = device.cast::<ID3D12Device2>().unwrap();
            check(
                "mesh",
                &device,
                |o| {
                    let mut s = stream(o);
                    device2.CreatePipelineState(&desc(&mut s)).unwrap()
                },
                &|l, n, o| {
                    let mut s = stream(o);
                    l.LoadPipeline(n, &desc(&mut s))
                },
            );
        }
    }
}
