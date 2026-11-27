//! Every submission of ours timed on the card (`SCARLETT_GPU_PROFILE=1`):
//! the title's sends with the layer's own lists in them (preludes, card
//! steps, checks), and the layer's submissions of its own (card copies,
//! fetches of what shaders wrote, texture uploads ahead).
//!
//! A list made ahead, one timestamp and its way into memory the PC reads,
//! is sent before the submission and another after it on the same queue.
//! The PC's counter at the send is kept, so a gap of the card between two
//! submissions can be told apart: the next one was sent only after the
//! card ran dry (the PC was late: it waited for something, or prepared),
//! or it was in the queue already and the card still stood (a wait on the
//! card: none of our queues waits on a fence, so this should be ~0).

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::core::Interface;

/// What was sent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// A title submission: its lists and the layer's lists among them.
    Title = 0,
    /// The layer's card copies brought up to date before a submission.
    CardCopies = 1,
    /// A fetch of what shaders wrote (copy-back), on the main queue.
    Fetch = 2,
    /// Texture uploads ahead, on their own queue.
    TextureAhead = 3,
    /// Anything else of ours.
    #[allow(dead_code)]
    Other = 4,
    /// Inside a title submission: the layer's card steps list ahead of the title's.
    OwnSteps = 5,
    /// Inside a title submission: the compare list behind the title's.
    Check = 6,
}

const KINDS: usize = 7;
const NAMES: [&str; KINDS] = ["title submissions", "card copies", "fetches of shader writes", "texture uploads ahead", "other own", "(inside) card steps", "(inside) compares"];

/// Stamp lists per queue type (each used once until read).
const RING: usize = 16384;
const AFTER: Duration = Duration::from_millis(1500);

struct Ring {
    lists: Vec<ID3D12CommandList>,
    busy: Vec<bool>,
    next: usize,
    back: usize,
    _kept: (ID3D12CommandAllocator, ID3D12QueryHeap, ID3D12Resource),
}

// SAFETY: free-threaded objects and a mapping, behind the profiler's lock.
unsafe impl Send for Ring {}

/// A submission begun: its stamp ahead.
pub(crate) struct Sending {
    ring: usize,
    queue: usize,
    kind: Kind,
    slot: usize,
    pc: i64,
}

struct Done {
    ring: usize,
    queue: usize,
    kind: Kind,
    ahead: usize,
    behind: usize,
    pc: i64,
    at: Instant,
}

/// One submission as read: on the PC's counter.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Span {
    pub(crate) queue: usize,
    pub(crate) kind: usize,
    pub(crate) sent: i64,
    pub(crate) from: i64,
    pub(crate) to: i64,
}

#[derive(Default)]
pub(crate) struct Sends {
    rings: [Option<Option<Ring>>; 2],
    /// Queues seen, by address (kept alive).
    queues: Vec<(usize, ID3D12CommandQueue)>,
    pending: VecDeque<Done>,
    pub(crate) spans: Vec<Span>,
    pub(crate) untimed: u64,
}

// SAFETY: as `Ring`.
unsafe impl Send for Sends {}

fn pc() -> i64 {
    let mut t = 0;
    // SAFETY: plain query.
    let _ = unsafe { QueryPerformanceCounter(&mut t) };
    t
}

fn make(device: &ID3D12Device, kind: D3D12_COMMAND_LIST_TYPE) -> windows::core::Result<Ring> {
    let readback = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: RING as u64 * 8,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    // SAFETY: plain object creation and recording of lists of our own.
    unsafe {
        let mut heap: Option<ID3D12QueryHeap> = None;
        device.CreateQueryHeap(&D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: RING as u32, NodeMask: 0 }, &mut heap)?;
        let heap = heap.ok_or_else(windows::core::Error::empty)?;
        let mut back: Option<ID3D12Resource> = None;
        device.CreateCommittedResource(&readback, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut back)?;
        let back = back.ok_or_else(windows::core::Error::empty)?;
        let mut mapped = std::ptr::null_mut();
        back.Map(0, None, Some(&mut mapped))?;
        std::ptr::write_bytes(mapped.cast::<u8>(), 0, RING * 8);
        let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(kind)?;
        let mut lists = Vec::with_capacity(RING);
        for i in 0..RING as u32 {
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, kind, &allocator, None)?;
            list.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, i);
            list.ResolveQueryData(&heap, D3D12_QUERY_TYPE_TIMESTAMP, i, 1, &back, u64::from(i) * 8);
            list.Close()?;
            lists.push(list.cast()?);
        }
        Ok(Ring { lists, busy: vec![false; RING], next: 0, back: mapped as usize, _kept: (allocator, heap, back) })
    }
}

impl Sends {
    fn ring_of(&mut self, queue: &ID3D12CommandQueue) -> Option<usize> {
        // SAFETY: plain query.
        let t = unsafe { queue.GetDesc() }.Type;
        let r = match t {
            D3D12_COMMAND_LIST_TYPE_DIRECT => 0,
            D3D12_COMMAND_LIST_TYPE_COMPUTE => 1,
            _ => return None,
        };
        if self.rings[r].is_none() {
            let device: Option<ID3D12Device> = {
                let mut d: Option<ID3D12Device> = None;
                // SAFETY: plain query.
                unsafe { queue.GetDevice(&mut d) }.ok().and(d)
            };
            self.rings[r] = Some(device.and_then(|d| make(&d, t).inspect_err(|e| tracing::warn!(error = %e, "no stamps for submissions")).ok()));
        }
        self.rings[r].as_ref()?.as_ref()?;
        Some(r)
    }

    fn queue_index(&mut self, queue: &ID3D12CommandQueue) -> usize {
        let at = queue.as_raw() as usize;
        if let Some(i) = self.queues.iter().position(|q| q.0 == at) {
            return i;
        }
        self.queues.push((at, queue.clone()));
        self.queues.len() - 1
    }

    fn take(&mut self, r: usize) -> Option<(usize, ID3D12CommandList)> {
        let ring = self.rings[r].as_mut()?.as_mut()?;
        let slot = ring.next;
        if ring.busy[slot] {
            return None;
        }
        ring.busy[slot] = true;
        ring.next = (slot + 1) % RING;
        Some((slot, ring.lists[slot].clone()))
    }

    /// A stamp list for kind to put among a submission's lists, and the
    /// token for its end ([Sends::inline_end]).
    pub(crate) fn inline(&mut self, queue: &ID3D12CommandQueue, kind: Kind) -> Option<(ID3D12CommandList, Sending)> {
        let r = self.ring_of(queue)?;
        let q = self.queue_index(queue);
        let Some((slot, list)) = self.take(r) else {
            self.untimed += 1;
            return None;
        };
        Some((list, Sending { ring: r, queue: q, kind, slot, pc: pc() }))
    }

    pub(crate) fn inline_end(&mut self, s: Sending) -> Option<ID3D12CommandList> {
        let taken = self.take(s.ring);
        let behind = taken.as_ref().map_or(usize::MAX, |t| t.0);
        self.pending.push_back(Done { ring: s.ring, queue: s.queue, kind: s.kind, ahead: s.slot, behind, pc: s.pc, at: Instant::now() });
        taken.map(|t| t.1)
    }

    fn stamp(&mut self, queue: &ID3D12CommandQueue, r: usize) -> Option<usize> {
        let (slot, list) = self.take(r)?;
        // SAFETY: a closed list of ours, not in flight (its stamp was read).
        unsafe { queue.ExecuteCommandLists(&[Some(list)]) };
        Some(slot)
    }

    pub(crate) fn sending(&mut self, queue: &ID3D12CommandQueue, kind: Kind) -> Option<Sending> {
        let Some(r) = self.ring_of(queue) else {
            self.untimed += 1;
            return None;
        };
        let q = self.queue_index(queue);
        let pc = pc();
        let Some(slot) = self.stamp(queue, r) else {
            self.untimed += 1;
            return None;
        };
        Some(Sending { ring: r, queue: q, kind, slot, pc })
    }

    pub(crate) fn sent(&mut self, queue: &ID3D12CommandQueue, s: Sending) {
        match self.stamp(queue, s.ring) {
            Some(behind) => {
                self.pending.push_back(Done { ring: s.ring, queue: s.queue, kind: s.kind, ahead: s.slot, behind, pc: s.pc, at: Instant::now() })
            }
            None => {
                // The stamp ahead was sent; it is read and freed with an empty end.
                self.untimed += 1;
                self.pending.push_back(Done { ring: s.ring, queue: s.queue, kind: s.kind, ahead: s.slot, behind: usize::MAX, pc: s.pc, at: Instant::now() });
            }
        }
    }

    /// Reads what is old enough, onto the PC's counter.
    pub(crate) fn read(&mut self) {
        let now = Instant::now();
        let mut qpf = 0;
        // SAFETY: plain query.
        let _ = unsafe { QueryPerformanceFrequency(&mut qpf) };
        // Per queue: (card ticks, pc) now, and the card's frequency.
        let cal: Vec<Option<(u64, u64, u64)>> = self
            .queues
            .iter()
            .map(|(_, q)| {
                let (mut card, mut at) = (0, 0);
                // SAFETY: plain queries.
                unsafe {
                    q.GetClockCalibration(&mut card, &mut at).ok()?;
                    let f = q.GetTimestampFrequency().ok()?;
                    (f != 0).then_some((card, at, f))
                }
            })
            .collect();
        while self.pending.front().is_some_and(|d| now.duration_since(d.at) >= AFTER) {
            let d = self.pending.pop_front().expect("front");
            let Some(Some(ring)) = self.rings[d.ring].as_mut() else { continue };
            // SAFETY: the ring's mapping, RING tick counts.
            let back = unsafe { std::slice::from_raw_parts_mut(ring.back as *mut u64, RING) };
            let take = |slot: usize, back: &mut [u64], busy: &mut [bool]| {
                if slot >= RING {
                    return 0;
                }
                let v = back[slot];
                back[slot] = 0;
                busy[slot] = false;
                v
            };
            let a = take(d.ahead, back, &mut ring.busy);
            let b = take(d.behind, back, &mut ring.busy);
            let Some(Some((card0, pc0, f))) = cal.get(d.queue).copied() else { continue };
            if a == 0 || b < a {
                continue;
            }
            let place = |t: u64| pc0 as i64 - ((card0 as i128 - t as i128) * qpf as i128 / f as i128) as i64;
            self.spans.push(Span { queue: d.queue, kind: d.kind as usize, sent: d.pc, from: place(a), to: place(b) });
        }
    }
}

/// The report over `frames` frames: per kind, and the main queue's gaps.
pub(crate) fn report(spans: &[Span], frames: u64, title_list_ms: f64, untimed: u64) -> String {
    let mut qpf = 0;
    // SAFETY: plain query.
    let _ = unsafe { QueryPerformanceFrequency(&mut qpf) };
    let ms = |t: i64| t as f64 * 1e3 / qpf.max(1) as f64 / frames.max(1) as f64;
    let mut out = String::new();
    let mut by = [(0u64, 0i64); KINDS];
    for s in spans {
        by[s.kind].0 += 1;
        by[s.kind].1 += s.to - s.from;
    }
    let _ = write!(out, "  GPU submissions ({untimed} untimed):");
    for k in 0..KINDS {
        if by[k].0 > 0 {
            let _ = write!(out, " {} {:.1}/frame {:.2} ms/frame;", NAMES[k], by[k].0 as f64 / frames.max(1) as f64, ms(by[k].1));
        }
    }
    let _ = writeln!(out, " title submissions less their title lists {:.2} ms/frame (the layer's lists inside them)", ms(by[0].1) - title_list_ms);
    // The main queue: the one title submissions went to.
    let Some(main) = spans.iter().find(|s| s.kind == 0).map(|s| s.queue) else {
        return out;
    };
    let mut v: Vec<Span> = spans.iter().filter(|s| s.queue == main).copied().collect();
    v.sort_by_key(|s| s.from);
    let others: Vec<(i64, i64)> = spans.iter().filter(|s| s.queue != main).map(|s| (s.from, s.to)).collect();
    let long = qpf / 1000;
    let (mut busy, mut extent) = (0i64, 0i64);
    let (mut n, mut gap_t, mut late_t, mut queued_t, mut other_t) = (0u64, 0i64, 0i64, 0i64, 0i64);
    let mut after = [(0u64, 0i64); KINDS];
    let mut before = [(0u64, 0i64); KINDS];
    if let Some(first) = v.first() {
        let (mut b, mut e, mut last_kind) = (first.from, first.to, first.kind);
        for s in &v[1..] {
            if s.from <= e {
                if s.to > e {
                    (e, last_kind) = (s.to, s.kind);
                }
                continue;
            }
            busy += e - b;
            let gap = s.from - e;
            if gap >= long {
                n += 1;
                gap_t += gap;
                // The PC sent the next one only after the card ran dry: late.
                let late = (s.sent - e).clamp(0, gap);
                late_t += late;
                queued_t += gap - late;
                other_t += others.iter().map(|&(a, z)| (z.min(s.from) - a.max(e)).max(0)).sum::<i64>().min(gap);
                after[s.kind].0 += 1;
                after[s.kind].1 += gap;
                before[last_kind].0 += 1;
                before[last_kind].1 += gap;
            }
            (b, e, last_kind) = (s.from, s.to, s.kind);
        }
        busy += e - b;
        extent = e - first.from;
    }
    let f = frames.max(1) as f64;
    let _ = writeln!(
        out,
        "  GPU main queue, all our submissions: busy {:.2} ms/frame of {:.2}; {:.1} gaps/frame over 1 ms = {:.2} ms/frame: the next submission sent after the card ran dry (PC late) {:.2} ms, already queued {:.2} ms; other queues busy in the gaps {:.2} ms",
        ms(busy),
        ms(extent),
        n as f64 / f,
        ms(gap_t),
        ms(late_t),
        ms(queued_t),
        ms(other_t)
    );
    let _ = write!(out, "  GPU gaps over 1 ms by what came before / after:");
    for k in 0..KINDS {
        if before[k].0 + after[k].0 > 0 {
            let _ = write!(out, " {}: before {:.2} ms, after {:.2} ms;", NAMES[k], ms(before[k].1), ms(after[k].1));
        }
    }
    let _ = writeln!(out);
    out
}
