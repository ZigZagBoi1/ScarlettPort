//! Says so when the process runs out of memory.
//!
//! A heap allocation that fails usually ends the process: std prints "memory
//! allocation of N bytes failed" and aborts with 0xC0000409, past every
//! crash handler, which reads like any other crash. [`Reporting`] is the
//! system's allocator that, when an allocation fails, writes a line to
//! stderr (the launcher's log) with the size, the Windows commit and RAM in
//! use, and what to do. Some failures are recovered from (`try_reserve`,
//! `std::fs::read`), so the line says only what is true either way, and up
//! to [`LINES`] are written. Writing one allocates nothing, since the heap is
//! what ran out: the line is formatted on the stack and the numbers come
//! from system calls.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{
    AtomicBool, AtomicU32, Ordering::Acquire, Ordering::Relaxed, Ordering::Release,
};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
use windows_sys::Win32::System::JobObjects::{
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    QueryInformationJobObject,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetPerformanceInfo, K32GetProcessMemoryInfo, PERFORMANCE_INFORMATION,
    PROCESS_MEMORY_COUNTERS,
};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// The system's allocator, reporting allocations it cannot make (the caller
/// then gets the null it would have: std's handler aborts, a fallible
/// caller recovers).
pub struct Reporting;

// SAFETY: every call goes to the system's allocator unchanged; a null it
// returns is only reported.
unsafe impl GlobalAlloc for Reporting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let at = unsafe { System.alloc(layout) };
        if at.is_null() {
            report(layout.size());
        }
        at
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let at = unsafe { System.alloc_zeroed(layout) };
        if at.is_null() {
            report(layout.size());
        }
        at
    }

    unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(at, layout) };
    }

    unsafe fn realloc(&self, at: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let moved = unsafe { System.realloc(at, layout, size) };
        if moved.is_null() {
            report(size);
        }
        moved
    }
}

/// Lines written at most per process: failures that are recovered from do
/// not use up the one before an abort, and a process failing over and over
/// does not fill the log.
pub const LINES: u32 = 3;

/// How long a thread that fails while another writes its line waits for it.
const WAIT: Duration = Duration::from_millis(200);

/// One line at a time, at most [`LINES`].
struct Guard {
    busy: AtomicBool,
    lines: AtomicU32,
}

impl Guard {
    const fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
            lines: AtomicU32::new(0),
        }
    }

    /// Runs `write` unless [`LINES`] lines were written. A thread that comes
    /// while another writes writes nothing and waits for that line to be out
    /// (at most [`WAIT`], plus one timer tick), since its abort would end the
    /// process before it is.
    fn write(&self, write: impl FnOnce()) {
        if self.lines.load(Relaxed) >= LINES {
            return;
        }
        if self
            .busy
            .compare_exchange(false, true, Acquire, Relaxed)
            .is_err()
        {
            let start = Instant::now();
            while self.busy.load(Acquire) && start.elapsed() < WAIT {
                std::thread::sleep(Duration::from_millis(1));
            }
            return;
        }
        if self.lines.fetch_add(1, Relaxed) < LINES {
            write();
        }
        self.busy.store(false, Release);
    }
}

static GUARD: Guard = Guard::new();

/// Writes the out-of-memory line to stderr (see [`Guard::write`]).
#[cold]
#[inline(never)]
pub fn report(size: usize) {
    // SAFETY: reads the process's standard error handle.
    GUARD.write(|| report_to(unsafe { GetStdHandle(STD_ERROR_HANDLE) }, size));
}

/// Writes the line to `out` (a file or pipe; nothing if invalid).
fn report_to(out: HANDLE, size: usize) {
    let mut line = [0u8; 512];
    let len = format_line(&mut line, size, &query());
    let mut wrote = 0;
    // SAFETY: the buffer holds `len` bytes; a bad handle only fails the call.
    unsafe {
        WriteFile(
            out,
            line.as_ptr(),
            len as u32,
            &mut wrote,
            std::ptr::null_mut(),
        )
    };
}

/// What Windows had when the allocation failed, in bytes; what could not be
/// read is `None`.
#[derive(Clone, Copy, Default)]
struct Memory {
    /// System-wide committed memory and the commit limit (RAM plus the
    /// paging file).
    commit: Option<(u64, u64)>,
    /// This process's private memory and the limit a job object puts on it
    /// (the smaller of its per-process and whole-job limits; the system's
    /// commit numbers do not show it).
    ours: Option<u64>,
    job: Option<u64>,
    /// Free and total RAM.
    ram: Option<(u64, u64)>,
}

/// The memory limit of the job this process runs in, if it sets one.
fn job_limit() -> Option<u64> {
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    let len = size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
    // SAFETY: a null handle names the calling process's job; the buffer and
    // size describe a writable JOBOBJECT_EXTENDED_LIMIT_INFORMATION.
    let queried = unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut info).cast(),
            len,
            std::ptr::null_mut(),
        )
    } != 0;
    if !queried {
        return None;
    }
    let flags = info.BasicLimitInformation.LimitFlags;
    let process = (flags & JOB_OBJECT_LIMIT_PROCESS_MEMORY != 0).then_some(info.ProcessMemoryLimit);
    let job = (flags & JOB_OBJECT_LIMIT_JOB_MEMORY != 0).then_some(info.JobMemoryLimit);
    process.into_iter().chain(job).min().map(|n| n as u64)
}

fn query() -> Memory {
    let mut perf = PERFORMANCE_INFORMATION {
        cb: size_of::<PERFORMANCE_INFORMATION>() as u32,
        ..Default::default()
    };
    // SAFETY: the buffer and size describe a writable PERFORMANCE_INFORMATION.
    let commit = (unsafe { K32GetPerformanceInfo(&mut perf, perf.cb) } != 0).then(|| {
        let page = perf.PageSize as u64;
        (
            perf.CommitTotal as u64 * page,
            perf.CommitLimit as u64 * page,
        )
    });
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: the buffer and size describe a writable PROCESS_MEMORY_COUNTERS.
    let ours =
        (unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) } != 0)
            .then_some(counters.PagefileUsage as u64);
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: the buffer is a MEMORYSTATUSEX with its length set.
    let ram = (unsafe { GlobalMemoryStatusEx(&mut status) } != 0)
        .then_some((status.ullAvailPhys, status.ullTotalPhys));
    Memory {
        commit,
        ours,
        job: job_limit(),
        ram,
    }
}

/// Formats the line into `buf` (cut at its end, never allocating) and
/// returns its length.
fn format_line(buf: &mut [u8], size: usize, memory: &Memory) -> usize {
    use std::fmt::Write;

    /// Writes into a byte slice, dropping what does not fit.
    struct Cursor<'a>(&'a mut [u8], usize);
    impl Write for Cursor<'_> {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            let room = self.0.len() - self.1;
            let n = s.len().min(room);
            self.0[self.1..self.1 + n].copy_from_slice(&s.as_bytes()[..n]);
            self.1 += n;
            Ok(())
        }
    }
    const MIB: u64 = 1 << 20;
    let mut out = Cursor(buf, 0);
    let _ = write!(
        out,
        "ScarlettPort: an allocation of {size} bytes failed (out of memory)."
    );
    if let Some((used, limit)) = memory.commit {
        let _ = write!(
            out,
            " Windows commit: {} of {} MiB in use.",
            used / MIB,
            limit / MIB
        );
    }
    if let Some(ours) = memory.ours {
        let _ = write!(out, " ScarlettPort's commit: {} MiB", ours / MIB);
        if let Some(limit) = memory.job {
            let _ = write!(out, ", in a job object limited to {} MiB", limit / MIB);
        }
        let _ = write!(out, ".");
    }
    if let Some((free, total)) = memory.ram {
        let _ = write!(out, " RAM: {} of {} MiB free.", free / MIB, total / MIB);
    }
    let _ = writeln!(
        out,
        " ScarlettPort needs about 32 GB of RAM, or a larger Windows paging file; if it stops \
         here, close other applications or increase the paging file size, then retry."
    );
    out.1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::os::windows::io::AsRawHandle;

    /// The test binary's allocator: [`Reporting`], counting this thread's
    /// allocations.
    struct Counting;

    thread_local! {
        static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    }

    fn allocations() -> usize {
        ALLOCATIONS.try_with(Cell::get).unwrap_or(0)
    }

    fn count() {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
    }

    // SAFETY: every call goes to `Reporting` unchanged.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            count();
            // SAFETY: the caller's contract, passed on.
            unsafe { Reporting.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            count();
            // SAFETY: the caller's contract, passed on.
            unsafe { Reporting.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
            // SAFETY: the caller's contract, passed on.
            unsafe { Reporting.dealloc(at, layout) };
        }

        unsafe fn realloc(&self, at: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            count();
            // SAFETY: the caller's contract, passed on.
            unsafe { Reporting.realloc(at, layout, size) }
        }
    }

    #[global_allocator]
    static HEAP: Counting = Counting;

    const GIB: u64 = 1 << 30;

    fn text(buf: &[u8]) -> &str {
        std::str::from_utf8(buf).unwrap()
    }

    #[test]
    fn the_line_names_the_size_the_commit_and_the_ram() {
        let mut memory = Memory {
            commit: Some((63 * GIB, 78 * GIB)),
            ours: Some(20 * GIB - (3 << 20)),
            job: None,
            ram: Some((GIB / 2, 16 * GIB)),
        };
        let mut buf = [0u8; 512];
        let before = allocations();
        let len = format_line(&mut buf, 3822994, &memory);
        assert_eq!(allocations(), before, "formatting allocated");
        assert_eq!(
            text(&buf[..len]),
            "ScarlettPort: an allocation of 3822994 bytes failed (out of memory). Windows commit: \
             64512 of 79872 MiB in use. ScarlettPort's commit: 20477 MiB. RAM: 512 of 16384 MiB \
             free. ScarlettPort needs about 32 GB of RAM, or a larger Windows paging file; if it \
             stops here, close other applications or increase the paging file size, then retry.\n"
        );
        memory.job = Some(20 * GIB);
        let len = format_line(&mut buf, 3822994, &memory);
        assert!(text(&buf[..len]).contains(
            "ScarlettPort's commit: 20477 MiB, in a job object limited to 20480 MiB. RAM"
        ));
        let len = format_line(&mut buf, 64, &Memory::default());
        assert!(text(&buf[..len]).starts_with(
            "ScarlettPort: an allocation of 64 bytes failed (out of memory). ScarlettPort needs"
        ));
    }

    #[test]
    fn the_report_allocates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stderr.txt");
        let file = std::fs::File::create(&path).unwrap();
        let before = allocations();
        drop(std::hint::black_box(vec![0u8; 8]));
        assert_eq!(allocations(), before + 1, "the counter counts");
        let before = allocations();
        report_to(file.as_raw_handle() as HANDLE, 4096);
        assert_eq!(allocations(), before, "reporting allocated");
        drop(file);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with(
                "ScarlettPort: an allocation of 4096 bytes failed (out of memory). Windows commit: "
            ),
            "{text}"
        );
        assert!(text.contains(" ScarlettPort's commit: "), "{text}");
        assert!(text.ends_with("then retry.\n"), "{text}");
    }

    #[test]
    fn at_most_three_lines_are_written() {
        let guard = Guard::new();
        let written = AtomicU32::new(0);
        for _ in 0..5 {
            guard.write(|| {
                written.fetch_add(1, Relaxed);
            });
        }
        assert_eq!(written.load(Relaxed), LINES);
    }

    #[test]
    fn a_failure_during_a_line_waits_for_it_and_writes_none() {
        let guard = Guard::new();
        let written = AtomicU32::new(0);
        let (started, writing) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| {
                guard.write(|| {
                    started.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(50));
                    written.fetch_add(1, Relaxed);
                })
            });
            writing.recv().unwrap();
            guard.write(|| {
                written.fetch_add(10, Relaxed);
            });
            // Back only once the first line is out, and without one of its own.
            assert_eq!(written.load(Relaxed), 1);
        });
        // The next failure writes again.
        guard.write(|| {
            written.fetch_add(1, Relaxed);
        });
        assert_eq!(written.load(Relaxed), 2);
    }
}
