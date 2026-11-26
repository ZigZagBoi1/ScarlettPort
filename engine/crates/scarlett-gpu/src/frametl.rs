//! With `SCARLETT_GPU_PROFILE=1`: where the wall-clock time of a frame goes
//! on the threads that gate it (those that submit and present), every five
//! seconds with the frame line, per frame (fx-gaps, `gaps.md`).
//!
//! Each such thread is always in exactly one state, so its states' times
//! sum to the time between the lines: the title's own code between runtime
//! calls (whatever no scope below covers), list calls, preparing a
//! submission, its refresh copies, copy-back done on the thread, the
//! present call, the submission lock, other queue calls, and waiting, by
//! what was waited for. For each state three clocks are kept: wall time,
//! the thread's processor time (wall minus that is time blocked), and how
//! much of it the card had nothing of ours queued (the card's clock,
//! `crate::host`'s `CardClock`). A wait with the card idle is a wait for
//! what follows the card: the landing, its copy-back, fetches.
//!
//! From those, the line states how much of the frame the thread and the
//! card were both busy, and what the frame would be, to first order, with
//! copy-back free and with a card that takes no time.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// The title's code between runtime calls (and runtime calls no scope
/// covers).
pub(crate) const TITLE: usize = 0;
/// List calls (translation), from `crate::copystat::recorded`.
pub(crate) const TRANSLATE: usize = 1;
/// In a submission, outside the phases below (its end, after the send).
pub(crate) const PREP: usize = 2;
/// Refresh copies of host copies.
pub(crate) const REFRESH: usize = 3;
/// Copy-back done on this thread (a submission that is waited for).
pub(crate) const BACK: usize = 4;
/// The present call (pacing and swap-chain waits are inside).
pub(crate) const PRESENT: usize = 5;
/// Waiting for the submission lock.
pub(crate) const LOCK: usize = 6;
/// Other queue calls: execute outside the submission proper, signal, wait.
pub(crate) const CALL: usize = 7;
/// Phases of preparing a submission (`Host::submit`), refresh copies and
/// waits apart: the checks before (what flights write that it reads, heap
/// sync, sweep, waiting slots); the write-watch asking and the look at
/// stale copies; the lists' deferred steps; the last flush's texture
/// refreshes and the snapshots; the card copies' sends and compare; the
/// send (served copies, the queue call, the flight's bookkeeping).
pub(crate) const PREP_CHECK: usize = 8;
pub(crate) const PREP_ASK: usize = 9;
pub(crate) const PREP_STEPS: usize = 10;
pub(crate) const PREP_SNAP: usize = 11;
pub(crate) const PREP_CARD: usize = 12;
pub(crate) const PREP_SEND: usize = 13;
/// Waiting, plus the `WHY_` place of `crate::host` (0: the compare's
/// result area half taken).
pub(crate) const WAIT: usize = 14;
const WHYS: usize = 12;
const CATS: usize = WAIT + WHYS;
const THREADS: usize = 4;

const NAMES: [&str; CATS] = [
    "title code between runtime calls",
    "list calls",
    "submission, after the send",
    "refresh copies",
    "copy-back on this thread",
    "present call",
    "submission lock",
    "other queue calls",
    "preparing: checks before (reads of flights' writes, heap sync)",
    "preparing: write-watch asking, look at stale copies",
    "preparing: deferred steps (not their refresh copies)",
    "preparing: texture refreshes of the last flush, snapshots",
    "preparing: card copies (sends, hash and compare lists)",
    "preparing: the send (served copies, queue call, bookkeeping)",
    "wait: the compare's result area half taken",
    "wait (1)",
    "wait: a synchronous submission or present",
    "wait: the card had run them",
    "wait: a memory value (title read)",
    "wait: title memory changed under shader writes (next refresh)",
    "wait: title memory read (buffer copy, fill, texture, refresh)",
    "wait: a step of ours",
    "wait: lists in the air",
    "wait: bottom-of-pipe writes",
    "wait: a blocking fence wait (title)",
    "wait (11)",
];

static THREAD: [AtomicU64; THREADS] = [const { AtomicU64::new(0) }; THREADS];
/// Presents and submissions by thread.
static COUNTS: [AtomicU64; THREADS * 2] = [const { AtomicU64::new(0) }; THREADS * 2];
static WALL: [AtomicU64; THREADS * CATS] = [const { AtomicU64::new(0) }; THREADS * CATS];
static CPU: [AtomicU64; THREADS * CATS] = [const { AtomicU64::new(0) }; THREADS * CATS];
static IDLE: [AtomicU64; THREADS * CATS] = [const { AtomicU64::new(0) }; THREADS * CATS];

/// The card's idle time so far (microseconds), and since when it is idle
/// now (`now_us` plus one; 0: it is not).
static CARD_IDLE: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The card reached the last of our signals with nothing behind it.
pub(crate) fn card_idle_begins() {
    CARD_IDLE[1].store(crate::host::now_us() + 1, Relaxed);
}

/// Lists of ours went to the queue.
pub(crate) fn card_idle_ends() {
    let since = CARD_IDLE[1].swap(0, Relaxed);
    if since != 0 {
        CARD_IDLE[0].fetch_add((crate::host::now_us() + 1).saturating_sub(since), Relaxed);
    }
}

fn card_idle(now: u64) -> u64 {
    let since = CARD_IDLE[1].load(Relaxed);
    CARD_IDLE[0].load(Relaxed) + if since != 0 { (now + 1).saturating_sub(since) } else { 0 }
}

#[derive(Clone, Copy)]
struct Me {
    slot: usize,
    cat: usize,
    at: u64,
    cpu: u64,
    idle: u64,
}

thread_local! {
    static ME: Cell<Option<Me>> = const { Cell::new(None) };
}

/// The calling thread's processor time so far (user, kernel), in
/// microseconds.
fn thread_times() -> (u64, u64) {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let (mut created, mut exited, mut kernel, mut user) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: our own thread and four valid out pointers.
    let _ = unsafe { GetThreadTimes(GetCurrentThread(), &mut created, &mut exited, &mut kernel, &mut user) };
    let us = |t: FILETIME| ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) / 10;
    (us(user), us(kernel))
}

fn clocks() -> (u64, u64, u64) {
    let now = crate::host::now_us();
    let (user, kernel) = thread_times();
    (now, user + kernel, card_idle(now))
}

fn switch(mut me: Me, cat: usize) -> Me {
    let (now, cpu, idle) = clocks();
    let i = me.slot * CATS + me.cat;
    WALL[i].fetch_add(now.saturating_sub(me.at), Relaxed);
    CPU[i].fetch_add(cpu.saturating_sub(me.cpu), Relaxed);
    IDLE[i].fetch_add(idle.saturating_sub(me.idle), Relaxed);
    me.cat = cat;
    (me.at, me.cpu, me.idle) = (now, cpu, idle);
    me
}

/// A state of the calling thread until it is dropped; then the state from
/// before.
pub(crate) struct Scope(Option<usize>);

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some(before) = self.0 {
            ME.with(|m| {
                if let Some(me) = m.get() {
                    m.set(Some(switch(me, before)));
                }
            });
        }
    }
}

fn enter(cat: usize, register: bool) -> Scope {
    if !crate::gpuprof::on() {
        return Scope(None);
    }
    ME.with(|m| {
        let me = match m.get() {
            Some(me) => me,
            None if register => {
                let id = u64::from(crate::host::thread_id()).max(1);
                let Some(slot) = (0..THREADS).find(|&s| THREAD[s].compare_exchange(0, id, Relaxed, Relaxed).is_ok()) else {
                    return Scope(None);
                };
                let (now, cpu, idle) = clocks();
                Me { slot, cat: TITLE, at: now, cpu, idle }
            }
            None => return Scope(None),
        };
        let before = me.cat;
        // A wait inside a wait keeps the outer one's reason.
        if cat >= WAIT && before >= WAIT {
            return Scope(None);
        }
        m.set(Some(switch(me, cat.min(CATS - 1))));
        Scope(Some(before))
    })
}

/// State `cat` for a thread already followed (one that submitted or
/// presented); nothing on others.
pub(crate) fn scope(cat: usize) -> Scope {
    enter(cat, false)
}

/// State `cat`, and the thread is followed from here on (a submission, a
/// present, a queue call).
pub(crate) fn root(cat: usize) -> Scope {
    enter(cat, true)
}

/// The calling thread, if followed, is in state `cat` from here on (a
/// phase inside a scope: the scope's end still puts back what was before
/// the scope).
pub(crate) fn set(cat: usize) {
    if !crate::gpuprof::on() {
        return;
    }
    ME.with(|m| {
        if let Some(me) = m.get() {
            m.set(Some(switch(me, cat.min(CATS - 1))));
        }
    });
}

/// Deferred steps by kind: nanoseconds and count.
static STEPS: [AtomicU64; 14] = [const { AtomicU64::new(0) }; 14];
const STEP_NAMES: [&str; 7] = [
    "title copies",
    "fills of upload memory",
    "refreshes (asked; the copying is at the flushes)",
    "steps that write title memory",
    "host steps",
    "ranges given to shaders (with the flush of their copy)",
    "textures",
];

/// A deferred step of kind `kind` took `ns` (with what it set off: the
/// flushes of refreshes are in the step that caused them).
pub(crate) fn step(kind: usize, ns: u64) {
    let k = kind.min(6);
    STEPS[k * 2].fetch_add(ns, Relaxed);
    STEPS[k * 2 + 1].fetch_add(1, Relaxed);
}

/// A present (0) or a submission (1) by this thread.
pub(crate) fn count(what: usize) {
    if let Some(me) = ME.with(Cell::get) {
        COUNTS[me.slot * 2 + what.min(1)].fetch_add(1, Relaxed);
    }
}

/// The last `ns` of this thread were `cat` (a list call, timed by its
/// caller): taken out of the state it is in.
pub(crate) fn retro(cat: usize, ns: u64) {
    if !crate::gpuprof::on() {
        return;
    }
    ME.with(|m| {
        if let Some(mut me) = m.get() {
            let us = ns / 1000;
            let i = me.slot * CATS + cat.min(CATS - 1);
            WALL[i].fetch_add(us, Relaxed);
            CPU[i].fetch_add(us, Relaxed);
            me.at += us;
            me.cpu += us;
            m.set(Some(me));
        }
    });
}

/// With the frame line: `frames` frames of `ms` each since the last one.
pub(crate) fn line(frames: f64, ms: f64) {
    if !crate::gpuprof::on() {
        return;
    }
    let per = |a: &AtomicU64| a.swap(0, Relaxed) as f64 / 1000.0 / frames;
    let steps: Vec<String> = (0..7)
        .map(|k| {
            let ns = STEPS[k * 2].swap(0, Relaxed) as f64 / frames;
            let n = STEPS[k * 2 + 1].swap(0, Relaxed) as f64 / frames;
            format!("{} {:.0} in {:.1} ms", STEP_NAMES[k], n, ns / 1e6)
        })
        .collect();
    tracing::info!(
        "  timeline, deferred steps per frame by kind (wall, refresh flushes inside the step that set them off): {}",
        steps.join("; ")
    );
    for slot in 0..THREADS {
        let thread = THREAD[slot].load(Relaxed);
        if thread == 0 {
            continue;
        }
        let rows: Vec<(f64, f64, f64)> =
            (0..CATS).map(|c| (per(&WALL[slot * CATS + c]), per(&CPU[slot * CATS + c]), per(&IDLE[slot * CATS + c]))).collect();
        let presents = COUNTS[slot * 2].swap(0, Relaxed) as f64 / frames;
        let submits = COUNTS[slot * 2 + 1].swap(0, Relaxed) as f64 / frames;
        let total: f64 = rows.iter().map(|r| r.0).sum();
        if total < 0.02 * ms {
            continue;
        }
        tracing::info!(
            "frame timeline, thread {thread}: {presents:.1} presents and {submits:.1} submissions a frame; its states sum to \
             {total:.1} ms of the frame's {ms:.1} ms; each state: wall ms (on the processor / blocked; of the wall, the card \
             had nothing of ours queued)"
        );
        for (c, &(wall, cpu, idle)) in rows.iter().enumerate() {
            if wall < 0.05 {
                continue;
            }
            let idle = idle.min(wall);
            let cpu = cpu.min(wall);
            tracing::info!(
                "  timeline {:<62} {wall:>7.1} ms ({cpu:.1} on the processor / {:.1} blocked; card idle {idle:.1})",
                NAMES[c],
                wall - cpu
            );
        }
        // Waits: the scoped ones. Work: everything else but the title's
        // own state, which is split by its processor time.
        let wait: f64 = rows[WAIT..].iter().map(|r| r.0).sum();
        let wait_idle: f64 = rows[WAIT..].iter().map(|r| r.2.min(r.0)).sum();
        let work: f64 = rows[TRANSLATE..WAIT].iter().map(|r| r.0).sum();
        let work_idle: f64 = rows[TRANSLATE..WAIT].iter().map(|r| r.2.min(r.0)).sum();
        let work_cpu: f64 = rows[TRANSLATE..WAIT].iter().map(|r| r.1.min(r.0)).sum();
        let (title, title_cpu, title_idle) = (rows[TITLE].0, rows[TITLE].1.min(rows[TITLE].0), rows[TITLE].2.min(rows[TITLE].0));
        let title_blocked = title - title_cpu;
        // The title's state: its idle share spread evenly over running and
        // blocked (nothing finer is known).
        let share = if title > 0.0 { title_idle / title } else { 0.0 };
        let both = (work - work_idle) + title_cpu * (1.0 - share);
        let cpu_only = work_idle + title_cpu * share;
        let card_only = wait - wait_idle;
        let neither = wait_idle;
        let back = rows[BACK].0;
        tracing::info!(
            "  timeline summary, thread {thread}, ms per frame: thread working and card busy (overlap) {both:.1}; thread \
             working, card idle {cpu_only:.1}; thread waiting, card busy {card_only:.1}; thread waiting, card idle (the \
             landing: copy-back, fetches) {neither:.1}; title's own code blocked {title_blocked:.1} (card idle {:.1} of it, \
             by its share); card idle in all {:.1}. To first order the frame would be {:.1} ms with copy-back free \
             (waits behind the landing and copy-back on this thread gone), {:.1} ms with a card that takes no time (waits \
             with the card busy gone), {:.1} ms with both (all scoped waits gone); the title's own blocked time is in none \
             of the three. The thread's own work is {work:.1} ms ({work_cpu:.1} on the processor): the frame's floor on \
             this thread however the card does",
            title_blocked * share,
            work_idle + wait_idle + title_idle,
            total - neither - back,
            total - card_only,
            total - wait - back,
        );
    }
}
