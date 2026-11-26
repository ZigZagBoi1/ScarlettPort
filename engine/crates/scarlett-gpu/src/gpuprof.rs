//! The card's time by pipeline (`SCARLETT_GPU_PROFILE=1`; off otherwise).
//!
//! The card meter (`crate::cardmeter`) says how long the title's lists
//! took, not which shaders. Here every host list the title records gets a
//! block of timestamps: one where the list begins, one each time a draw or
//! dispatch binds another pipeline, one where the list ends, all resolved
//! by the list itself into one mapped read-back buffer. The span between
//! two stamps goes to the pipeline bound in it, with the draws and
//! dispatches it held. Nothing waits for the card: a block is read
//! [`AFTER`] its list closed, long after any flight of then has landed,
//! and zeroed so a list that never ran is not counted.
//!
//! Every [`EVERY`], the log gets the title lists' total and the top
//! [`TOP`] pipelines by card milliseconds a frame, named by their shaders'
//! hashes (the first 16 hex digits of the container's BLAKE3: the
//! translator cache's and shaders.rpf records' names).
//!
//! Spans are between the card's stamps at consecutive pipeline changes:
//! with draws overlapping across a change, some of a pipeline's work lands
//! in the next one's span. Good for ranking, not to the microsecond. The
//! indirect-clamp passes the layer adds inside a group count toward that
//! group's pipeline.

#[path = "gpuprof/agg.rs"]
mod agg;

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::Interface;

use agg::{Agg, ListStamps, OVERFLOW, PRELUDE};

/// Stamps a list may use.
const SLOTS: u32 = 1024;
/// Blocks of [`SLOTS`]: lists timed at once (recorded and not read yet).
const BLOCKS: u32 = 1024;
const AFTER: Duration = Duration::from_millis(750);
const EVERY: Duration = Duration::from_secs(5);
const TOP: usize = 25;

pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SCARLETT_GPU_PROFILE").is_some_and(|v| v != "0"))
}

struct Open {
    block: u32,
    /// Per span (stamps - 1 of them while open): pipeline key, draws.
    keys: Vec<u32>,
    draws: Vec<u32>,
}

struct Prof {
    heap: ID3D12QueryHeap,
    back: ID3D12Resource,
    mapped: usize,
    frequency: u64,
    free: Vec<u32>,
    open: HashMap<usize, Open>,
    pending: VecDeque<(Open, Instant)>,
    /// Host pipeline (kept alive so its address names it alone) to key.
    keys: HashMap<usize, (u32, ID3D12PipelineState)>,
    names: Vec<String>,
    agg: Agg,
    since: Instant,
    /// Lists not timed: no free block.
    untimed: u64,
}

// SAFETY: free-threaded host objects and a mapping, behind a lock.
unsafe impl Send for Prof {}

static PROF: Mutex<Option<Option<Prof>>> = Mutex::new(None);

fn make() -> Option<Prof> {
    let host = crate::host::host()?;
    let device = host.device();
    let count = SLOTS * BLOCKS;
    let mut heap: Option<ID3D12QueryHeap> = None;
    let mut back: Option<ID3D12Resource> = None;
    let readback = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: u64::from(count) * 8,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    // SAFETY: plain object creation; the buffer is mapped once and stays so.
    unsafe {
        device
            .CreateQueryHeap(&D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: count, NodeMask: 0 }, &mut heap)
            .ok()?;
        device
            .CreateCommittedResource(&readback, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut back)
            .ok()?;
        let (heap, back) = (heap?, back?);
        let mut mapped = std::ptr::null_mut();
        back.Map(0, None, Some(&mut mapped)).ok()?;
        let frequency = host.timestamp_frequency();
        if mapped.is_null() || frequency == 0 {
            return None;
        }
        std::ptr::write_bytes(mapped.cast::<u8>(), 0, count as usize * 8);
        tracing::info!(
            "SCARLETT_GPU_PROFILE: the card's time by pipeline, a timestamp at each pipeline change; top {TOP} every {}s",
            EVERY.as_secs()
        );
        Some(Prof {
            heap,
            back,
            mapped: mapped as usize,
            frequency,
            free: (0..BLOCKS).rev().collect(),
            open: HashMap::new(),
            pending: VecDeque::new(),
            keys: HashMap::new(),
            names: Vec::new(),
            agg: Agg::default(),
            since: Instant::now(),
            untimed: 0,
        })
    }
}

fn with<R>(f: impl FnOnce(&mut Prof) -> R) -> Option<R> {
    let mut g = PROF.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(make).as_mut().map(f)
}

impl Prof {
    fn stamp(&self, list: &ID3D12GraphicsCommandList, slot: u32) {
        // SAFETY: an open list and our heap.
        unsafe { list.EndQuery(&self.heap, D3D12_QUERY_TYPE_TIMESTAMP, slot) };
    }

    fn begin(&mut self, list: &ID3D12GraphicsCommandList) -> bool {
        let at = list.as_raw() as usize;
        if let Some(old) = self.open.remove(&at) {
            // Reset without a close: its stamps were never resolved.
            self.free.push(old.block);
        }
        let Some(block) = self.free.pop() else {
            self.untimed += 1;
            return false;
        };
        self.stamp(list, block * SLOTS);
        self.open.insert(at, Open { block, keys: vec![PRELUDE], draws: vec![0] });
        true
    }
}

/// A host list begins (or was reset to begin again).
pub(crate) fn begin(list: &ID3D12GraphicsCommandList) {
    if on() {
        with(|p| p.begin(list));
    }
}

/// A draw or dispatch with `pso` bound is recorded into `list`; `name`
/// names the pipeline by its shaders the first time it is seen.
pub(crate) fn draw(list: &ID3D12GraphicsCommandList, pso: &ID3D12PipelineState, name: impl FnOnce() -> String) {
    if !on() {
        return;
    }
    with(|p| {
        let at = list.as_raw() as usize;
        if !p.open.contains_key(&at) && !p.begin(list) {
            return;
        }
        let key = match p.keys.get(&(pso.as_raw() as usize)) {
            Some(&(k, _)) => k,
            None => {
                let k = p.names.len() as u32;
                p.names.push(name());
                p.keys.insert(pso.as_raw() as usize, (k, pso.clone()));
                k
            }
        };
        let o = p.open.get_mut(&at).expect("just opened");
        let last = o.keys.len() - 1;
        if o.keys[last] == key || o.keys[last] == OVERFLOW {
            o.draws[last] += 1;
            return;
        }
        // Stamps used so far: keys.len(). This change takes one, the end one more.
        if o.keys.len() as u32 + 2 > SLOTS {
            // Out of stamps: the rest of the list is one span.
            let slot = o.block * SLOTS + o.keys.len() as u32;
            o.keys.push(OVERFLOW);
            o.draws.push(1);
            p.stamp(list, slot);
            return;
        }
        let slot = o.block * SLOTS + o.keys.len() as u32;
        o.keys.push(key);
        o.draws.push(1);
        p.stamp(list, slot);
    });
}

/// The list closes: its last stamp, and all its stamps are resolved.
pub(crate) fn end(list: &ID3D12GraphicsCommandList) {
    if !on() {
        return;
    }
    with(|p| {
        let Some(o) = p.open.remove(&(list.as_raw() as usize)) else {
            return;
        };
        let n = o.keys.len() as u32 + 1;
        let base = o.block * SLOTS;
        p.stamp(list, base + n - 1);
        // SAFETY: an open list, our heap and read-back buffer, the block's own bytes.
        unsafe {
            list.ResolveQueryData(&p.heap, D3D12_QUERY_TYPE_TIMESTAMP, base, n, &p.back, u64::from(base) * 8);
        }
        p.pending.push_back((o, Instant::now()));
    });
}

/// Once a frame: blocks old enough are read, and the report is logged
/// every [`EVERY`].
pub(crate) fn frame() {
    if !on() {
        return;
    }
    with(|p| {
        p.agg.frame();
        let now = Instant::now();
        let cap = p.frequency;
        while p.pending.front().is_some_and(|(_, at)| now.duration_since(*at) >= AFTER) {
            let (o, _) = p.pending.pop_front().expect("front");
            let n = o.keys.len() + 1;
            // SAFETY: the block's stamps in the mapped read-back buffer;
            // the card wrote them long ago or never.
            let stamps: Vec<u64> = unsafe {
                let ptr = (p.mapped as *mut u64).add((o.block * SLOTS) as usize);
                let s = (0..n).map(|i| ptr.add(i).read_volatile()).collect();
                std::ptr::write_bytes(ptr, 0, n);
                s
            };
            p.agg.add_list(ListStamps { stamps: &stamps, keys: &o.keys, draws: &o.draws }, cap);
            p.free.push(o.block);
        }
        if now.duration_since(p.since) >= EVERY {
            let names = &p.names;
            let report = p.agg.report(TOP, p.frequency, &|k| names.get(k as usize).cloned().unwrap_or_default());
            tracing::info!(untimed_lists = p.untimed, "{report}");
            p.agg = Agg::default();
            p.since = now;
        }
    });
}

/// A host pipeline's name for the report: its stages and shader hashes.
pub(crate) fn name(p: &crate::pipeline::host::HostPipeline) -> String {
    let kind = if p.compute {
        "compute"
    } else if p.mesh {
        "mesh"
    } else {
        "graphics"
    };
    let stages: Vec<String> = p.stages.iter().map(|s| format!("{} {}", s.shader.stage, s.shader.hash)).collect();
    format!("{kind}: {}", stages.join(", "))
}
