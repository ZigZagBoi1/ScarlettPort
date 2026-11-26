//! Title memory by address, for translated shaders that read and write
//! through raw 40-bit addresses (`victory_shader::emit::SLOT_MEMORY`: the
//! console's ray-tracing structure builder and the programs that walk its
//! structures; the structures themselves lie in plain title memory, in no
//! buffer object).
//!
//! The copies are the ones every other access uses ([`crate::shadow`]: a
//! title buffer's copy, or the 4 MiB window of memory outside buffers), so
//! a root view and a raw address reach the same host bytes. This module
//! keeps what a shader needs to find them:
//!
//! - a directory per host view heap, indexed by 64 KiB title page. Each entry
//!   gives the raw UAV slot relative to the null slot, and its byte offset.
//!   New mappings take free slots as the title streams its structures;
//! - the **faults**: the page numbers shaders found empty, read back here
//!   once a frame, so the copies exist from the next frame on. A pointer a
//!   shader takes out of memory cannot be known before it runs; the
//!   addresses a dispatch is given as root views are made ready before it.
//!
//! **A page that is not there when a shader touches it is a lost access**:
//! the read gives 0, the store is dropped, and the fault note only helps
//! the next frame. For the structure builder that is the same as a dropped
//! stage, so before a dispatch the host makes ready everything it can see
//! the dispatch name: the whole copy around each root view, around each
//! pair of root constants that is a title address and around each root
//! constant that is one in 256-byte units (the builder passes structures
//! all three ways), and the 4 MiB after each (a structure may cross a
//! window's end).
//! InitHeaders records are filled after recording and resolved at submission.
//! Other pointer chains can still fault the first time they are traversed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use victory_shader::emit::{MEMORY_FAULTS, MEMORY_PAGES};
use windows::Win32::Graphics::Direct3D12::*;

use crate::descriptor::host::HostHeap;
use crate::host::{HostList, host};
use crate::shadow::Shadow;

/// A window of memory outside buffers (`shadow.rs`).
const CELL: u64 = 4 << 20;
/// A directory page.
const PAGE: u64 = 1 << 16;

struct Memory {
    /// Zero means unmapped. Other entries identify a copy by its index plus one.
    page_copy: Vec<u32>,
    /// Mappings in publication order, for heaps that have not seen them yet.
    pages: Vec<Page>,
    faults: (ID3D12Resource, *mut u32, u64),
    /// Copy `n` is `copies[n - 1]`.
    copies: Vec<Arc<Shadow>>,
    heaps: HashMap<usize, HeapDirectory>,
    /// The frame the faults were last read in.
    frame: u64,
    /// Pages asked for this frame that are not the title's memory.
    refused: std::collections::HashSet<u64>,
}

// SAFETY: the mappings are of buffers the struct owns; all access is under
// the mutex.
unsafe impl Send for Memory {}

struct Page {
    index: u32,
    copy: usize,
    offset: u32,
}

struct HeapDirectory {
    buffer: (ID3D12Resource, *mut u32, u64),
    null: u32,
    slots: Vec<u32>,
    published: usize,
}

fn mapped_words(dwords: u64, name: &str) -> Option<(ID3D12Resource, *mut u32, u64)> {
    let (buffer, data) = host()?.shared_buffer(dwords * 4)?;
    crate::host::name(&buffer, || format!("title memory by address: {name}"));
    // SAFETY: the buffer owns the whole mapping.
    let gpu = unsafe {
        std::ptr::write_bytes(data, 0, (dwords * 4) as usize);
        buffer.GetGPUVirtualAddress()
    };
    Some((buffer, data.cast(), gpu))
}

impl HeapDirectory {
    fn new(heap: &HostHeap) -> Option<Self> {
        let buffer = mapped_words(u64::from(MEMORY_PAGES) * 2, "directory")?;
        let null = heap.dynamic(1)?;
        heap.null_raw_uav_at(null);
        Some(Self { buffer, null, slots: Vec::new(), published: 0 })
    }

    fn publish(&mut self, heap: &HostHeap, copies: &[Arc<Shadow>], pages: &[Page]) -> Option<()> {
        for shadow in &copies[self.slots.len()..] {
            let slot = heap.dynamic(1)?;
            heap.raw_uav_at(slot, &shadow.buffer, 0, shadow.span().1);
            self.slots.push(slot);
        }
        for page in &pages[self.published..] {
            let relative = self.slots[page.copy].wrapping_sub(self.null);
            // SAFETY: the last row is reserved. Publish the slot after its offset.
            unsafe {
                let entry = self.buffer.1.add(page.index as usize * 2);
                entry.add(1).write_volatile(page.offset);
                std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
                entry.write_volatile(relative);
            }
        }
        self.published = pages.len();
        Some(())
    }
}

static MEMORY: Mutex<Option<Memory>> = Mutex::new(None);

fn contiguous_copy(page_copy: &[u32], address: u64, reach: u64) -> Option<usize> {
    let end = address.checked_add(reach.checked_sub(1)?)?;
    let pages = page_copy.get((address / PAGE) as usize..=(end / PAGE) as usize)?;
    let (&first, remaining) = pages.split_first()?;
    let copy = first.checked_sub(1)?;
    remaining.iter().all(|&page| page == first).then_some(copy as usize)
}

/// A builder can address the same bytes through a root UAV and through a
/// node pointer. Overlapping title buffers may have different host copies.
/// Bind the directory's copy when it covers the whole shader access.
pub(crate) fn builder_root(address: u64, reach: u64, original: &Arc<Shadow>) -> Option<Arc<Shadow>> {
    if reach == 0 || address / PAGE >= u64::from(MEMORY_PAGES) - 1 {
        return None;
    }
    let mut memory = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if memory.is_none() {
        *memory = Memory::new();
    }
    let memory = memory.as_mut()?;
    // DecompressHeader can write a structure using only root UAVs, before
    // any shader requests a raw view. Remember the backing it writes now.
    memory.map_with_root(address, Some(original));
    memory.builder_root(address, reach, original)
}

#[derive(Clone)]
pub(crate) struct BuilderUpload {
    pub(crate) heap: Arc<HostHeap>,
    pub(crate) inputs: BuilderInputs,
}

#[derive(Clone, Copy)]
pub(crate) enum BuilderInputs {
    Headers { table: u64, count: u32 },
    Leaves { table: u64, source: u64, destination: u64, offset: u32, count: u32 },
}

impl BuilderUpload {
    pub(crate) fn prepare(&self, list: &HostList) {
        let mut addresses = Vec::new();
        match self.inputs {
            BuilderInputs::Headers { table, count } => header_destinations(table, count, &mut addresses),
            BuilderInputs::Leaves { table, source, destination, offset, count } => {
                leaf_inputs(table, source, destination, offset, count, &mut addresses);
                addresses.sort_unstable();
                addresses.dedup();
            }
        }
        if !addresses.is_empty() {
            ready(list, &self.heap, &addresses, &[], false, true);
        }
    }
}

/// The raw-address directory maps whole pages. Include the last one when
/// the shader's stream ends partway through it.
fn stream_pages(address: u64, length: u64, pages: &mut Vec<u64>) {
    if address == 0 || length == 0 { return; }
    let Some(end) = address.checked_add(length - 1) else { return };
    if end / PAGE >= u64::from(MEMORY_PAGES) - 1 { return; }
    pages.extend((address / PAGE..=end / PAGE).map(|page| page * PAGE));
}

fn leaf_inputs(table: u64, source: u64, destination: u64, offset: u32, count: u32, pages: &mut Vec<u64>) {
    let length = u64::from(count) * 64;
    let Some(start) = source.checked_add(u64::from(offset)) else { return };
    let Some(end) = start.checked_add(length) else { return };
    if source == 0 || table == 0 || count == 0 || end > (u64::from(MEMORY_PAGES) - 1) * PAGE { return; }
    stream_pages(start, length, pages);
    if destination != 0 && let Some(dest) = destination.checked_add(u64::from(offset)) {
        stream_pages(dest, length, pages);
    }
    for id in leaf_geometries(start, count) {
        let mut record = [0; 48];
        let Some(address) = table.checked_add(u64::from(id) * 48) else { continue };
        if crate::host::read_into(address as *const u8, &mut record) {
            geometry_streams(&record, pages);
        }
    }
}

/// Each group starts with one little-endian 24-bit geometry index per
/// leaf, then padding and other fields up to the next 128 KiB boundary
/// relative to the first group. Read only the indices of live leaves.
fn leaf_geometries(start: u64, count: u32) -> std::collections::BTreeSet<u32> {
    const GROUP: u32 = 2048;
    let mut ids = std::collections::BTreeSet::new();
    let mut packed = [0; GROUP as usize * 3];
    for group in 0..count.div_ceil(GROUP) {
        let address = start + u64::from(group) * u64::from(GROUP) * 64;
        let leaves = (count - group * GROUP).min(GROUP) as usize;
        let packed = &mut packed[..3 * leaves];
        if !crate::host::read_into(address as *const u8, packed) { continue; }
        for id in packed.chunks_exact(3) {
            ids.insert(u32::from_le_bytes([id[0], id[1], id[2], 0]));
        }
    }
    ids
}

/// The builder reads console D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC
/// records. Flag bits above the low 15 stride bits describe packed geometry.
fn geometry_streams(record: &[u8; 48], pages: &mut Vec<u64>) {
    let word = |i: usize| u32::from_le_bytes(record[i..i+4].try_into().unwrap());
    let address = |i: usize| u64::from_le_bytes(record[i..i+8].try_into().unwrap());
    let index = address(24);
    let index_count = u64::from(word(16));
    let length = match word(8) {
        42 => index_count * 4,
        57 => index_count * 2,
        53 if index != 0 => {
            let mut header = [0; 12];
            if crate::host::read_into(index as *const u8, &mut header) {
                packed_index_bytes(&header, index_count)
            } else {
                12
            }
        }
        _ => 0,
    };
    stream_pages(index, length, pages);
    stream_pages(address(32), u64::from(word(20)) * (address(40) & 0x7fff), pages);
    stream_pages(address(0), 48, pages);
}

/// The packed index layout has a 16-byte record per meshlet, a dword per
/// triangle and a 16-bit vertex remap. LeafRehydration loads the remap in
/// aligned dwords. An incomplete upload may still have an old header.
fn packed_index_bytes(header: &[u8; 12], index_count: u64) -> u64 {
    let word = |i: usize| u64::from(u32::from_le_bytes(header[i..i+4].try_into().unwrap()));
    let meshlets = word(0) & 0x1ffff;
    let triangles = word(4);
    let remap = word(8);
    if triangles * 3 != index_count || meshlets > triangles || remap > index_count {
        return 12;
    }
    (12 + meshlets * 16 + triangles * 4 + remap * 2).next_multiple_of(4)
}

/// Each 40-byte InitHeaders record names a 64-byte header and, optionally,
/// another 24 bytes for the bounding box. Read the upload in small chunks.
fn header_destinations(mut table: u64, mut count: u32, addresses: &mut Vec<u64>) {
    let mut chunk = [0u8; 40 * 32];
    while count != 0 {
        let n = count.min(32);
        let bytes = &mut chunk[..40 * n as usize];
        let Some(next) = table.checked_add(bytes.len() as u64) else { break };
        if !crate::host::read_into(table as *const u8, bytes) {
            break;
        }
        for record in bytes.chunks_exact(40) {
            for (word, len) in [(0, 64), (8, 24)] {
                let address = u64::from_le_bytes(record[word..word + 8].try_into().unwrap());
                if address != 0 && address < (u64::from(MEMORY_PAGES) - 1) * PAGE {
                    addresses.push(address);
                    if address % PAGE + len > PAGE {
                        addresses.push(address + len - 1);
                    }
                }
            }
        }
        count -= n;
        table = next;
    }
}

/// What a draw binds.
pub(crate) struct Ready {
    pub(crate) directory: u64,
    pub(crate) faults: u64,
    /// The view heap slot of the null view; copy `n`'s view is `n` further.
    pub(crate) base: u32,
}

impl Memory {
    fn builder_root(&self, address: u64, reach: u64, original: &Shadow) -> Option<Arc<Shadow>> {
        let index = self.copy_at(address)?;
        let copy = self.copies.get(index)?;
        if std::ptr::eq(copy.as_ref(), original)
            || copy.offset_in(address).is_none()
            || copy.left(address) < reach
            || (copy.gpu(address) & 15) != (original.gpu(address) & 15)
            || contiguous_copy(&self.page_copy, address, reach) != Some(index)
        {
            return None;
        }
        Some(copy.clone())
    }

    fn new() -> Option<Memory> {
        tracing::info!("title memory by address: a shader reads or writes through raw addresses");
        Some(Memory {
            page_copy: vec![0; MEMORY_PAGES as usize],
            pages: Vec::new(),
            faults: mapped_words(u64::from(MEMORY_FAULTS), "faults")?,
            copies: Vec::new(),
            heaps: HashMap::new(),
            frame: u64::MAX,
            refused: Default::default(),
        })
    }

    /// Gives the page of `address` its copy, if it has none and the memory
    /// is the title's.
    fn map(&mut self, address: u64) {
        self.map_with_root(address, None);
    }

    fn map_with_root(&mut self, address: u64, root: Option<&Arc<Shadow>>) {
        let page = address / PAGE;
        // The last entry stays empty: addresses past the directory read it.
        if page >= u64::from(MEMORY_PAGES) - 1 {
            return;
        }
        if self.page_copy[page as usize] != 0 {
            return;
        }
        let start = page * PAGE;
        // A shader's pointer, or a constant taken for an address, may be
        // anything: only memory the title has.
        if self.refused.contains(&page) {
            return;
        }
        if !crate::texture::committed(start as *const u8, PAGE as usize) {
            self.refused.insert(page);
            return;
        }
        // The copy a root view at that address would get; a buffer that
        // ends inside the page is passed over for what holds all of it.
        let usable = |s: &Arc<Shadow>| {
            if s.offset_in(start).is_none() || s.left(start) < PAGE {
                return false;
            }
            // SAFETY: the copy owns its live host buffer.
            unsafe { s.buffer.GetDesc() }.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
        };
        let Some(shadow) = root.filter(|s| usable(s)).cloned()
            .or_else(|| crate::shadow::find(start, 1).filter(usable))
            .or_else(|| crate::shadow::find(start, PAGE).filter(usable))
        else {
            return;
        };
        let n = match self.copies.iter().position(|c| Arc::ptr_eq(c, &shadow)) {
            Some(n) => n,
            None => {
                let n = self.copies.len();
                self.copies.push(shadow.clone());
                let (first, len) = shadow.span();
                let mut at = first.next_multiple_of(PAGE);
                while at + PAGE <= first + len && at / PAGE < u64::from(MEMORY_PAGES) - 1 {
                    self.add_page(at / PAGE, n, at - first);
                    at += PAGE;
                }
                if (n + 1).is_power_of_two() && n >= 1023 {
                    tracing::info!(copies = n + 1, pages = self.pages.len(), "mapped title memory for raw-address shaders");
                }
                n
            }
        };
        self.add_page(page, n, shadow.offset_in(start).unwrap_or(0));
    }

    fn add_page(&mut self, index: u64, copy: usize, offset: u64) {
        let id = &mut self.page_copy[index as usize];
        if *id == 0 {
            *id = copy as u32 + 1;
            self.pages.push(Page { index: index as u32, copy, offset: offset as u32 });
        }
    }

    /// The pages shaders found empty since the last frame.
    fn take_faults(&mut self) {
        let mut pages = Vec::new();
        for i in 1..MEMORY_FAULTS as usize {
            // SAFETY: inside the faults' mapping; the GPU may be writing,
            // a number missed now is written again next frame.
            let at = unsafe { self.faults.1.add(i) };
            // SAFETY: as above.
            let page = unsafe { at.read_volatile() };
            if page != 0 {
                pages.push(page);
                // SAFETY: as above.
                unsafe { at.write_volatile(0) };
            }
        }
        if !pages.is_empty() {
            // Each is an access that read 0 or was dropped.
            static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 50 {
                tracing::warn!(
                    pages = pages.len(),
                    first = format_args!("{:#x}", u64::from(pages[0]) * PAGE),
                    "title memory by address: shaders touched pages that had no copy (those accesses read 0 or were dropped); the copies are made now"
                );
            }
        }
        for page in pages {
            self.map(u64::from(page) * PAGE);
        }
    }

    /// What a dispatch names: the copy around `address` and the 4 MiB
    /// after it. Returns their numbers less one (`copies` places), for
    /// `VICTORY_NARROW_BVH=2`.
    fn name(&mut self, address: u64) -> [Option<usize>; 2] {
        let next = (address & !(CELL - 1)) + CELL;
        self.map(address);
        self.map(next);
        [self.copy_at(address), self.copy_at(next)]
    }

    /// The `copies` place of the copy holding `address`'s page, if it has one.
    fn copy_at(&self, address: u64) -> Option<usize> {
        let page = address / PAGE;
        if page >= u64::from(MEMORY_PAGES) - 1 {
            return None;
        }
        let n = self.page_copy[page as usize];
        (n != 0).then(|| n as usize - 1)
    }
}

/// Before a draw whose shader reads title memory by address: what its
/// root arguments name gets its copies (`addresses`: its root views;
/// `constants`: its root constants, see the module), every numbered copy
/// has its view in `heap`'s run and is brought up to date when `list` is
/// submitted (and, with `writes`, copied back after). None if the host
/// cannot hold it; the draw is then dropped.
///
/// `VICTORY_NARROW_BVH` ([`crate::narrow`]): at 1, with `by_address` false
/// (the program has no store or atomic by raw address) no copy is declared
/// written; at 2, a program that does store by address declares only the
/// copies its root views and constants name.
pub(crate) fn ready(
    list: &HostList,
    heap: &HostHeap,
    addresses: &[u64],
    constants: &[u32],
    writes: bool,
    by_address: bool,
) -> Option<Ready> {
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Memory::new();
    }
    let m = guard.as_mut()?;
    let frame = crate::frame::current();
    if m.frame != frame {
        m.frame = frame;
        m.refused.clear();
        m.take_faults();
    }
    let mut named: Vec<usize> = Vec::new();
    for &address in addresses {
        named.extend(m.name(address).into_iter().flatten());
    }
    // A title address in two constants (low dword, then bits 32 to 39), or
    // in one in 256-byte units. A constant that only looks like one names
    // memory nothing reads; one that is not the title's memory is refused.
    for (i, &c) in constants.iter().enumerate() {
        if let Some(&high) = constants.get(i + 1)
            && (1..=0xFF).contains(&high)
            && c >= 0x1_0000
        {
            named.extend(m.name(u64::from(c) | u64::from(high) << 32).into_iter().flatten());
        }
        if c >= 0x0100_0000 {
            named.extend(m.name(u64::from(c) << 8).into_iter().flatten());
        }
    }
    let key = std::ptr::from_ref(heap) as usize;
    let directory = match m.heaps.entry(key) {
        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        std::collections::hash_map::Entry::Vacant(e) => e.insert(HeapDirectory::new(heap)?),
    };
    directory.publish(heap, &m.copies, &m.pages)?;
    let gpu = directory.buffer.2;
    let base = directory.null;
    if list.first_use(directory.buffer.1 as usize) {
        list.keep(&directory.buffer.0);
    }
    // Once per list, and for copies numbered since: the builder alone
    // dispatches hundreds of times a frame.
    const READ: usize = usize::MAX - 0x6d;
    // (`VICTORY_NARROW_BVH`: the copies left out are counted once per list
    // too; with 2, each named copy is declared once per list.)
    const NAMED: usize = usize::MAX - 0x1_0000;
    let level = crate::narrow::bvh();
    let declare = plan(level, writes, by_address);
    let first_read = list.first_use(READ);
    let [read, written, omitted] = list.raw_copy_counts(m.copies.len(), declare == Declare::All, writes && declare != Declare::All);
    let first = match declare {
        Declare::All => read.min(written),
        Declare::Named if !named.is_empty() => 0,
        _ if writes => read.min(omitted),
        _ => read,
    };
    for (n, shadow) in m.copies.iter().enumerate().skip(first) {
        let (start, len) = shadow.span();
        if n >= read {
            shadow.refresh(list, start, len);
        }
        let declared = match declare {
            Declare::All => n >= written,
            Declare::Named => named.contains(&n) && list.first_use(NAMED - n),
            Declare::None => false,
        };
        if declared {
            shadow.gpu_writes(list, start, len);
        } else if declare != Declare::All
            && writes && n >= omitted
            && !(declare == Declare::Named && named.contains(&n))
            && !shadow.in_place()
        {
            list.note_narrowed(shadow, start as usize, len as usize, crate::narrow::Cut::Bvh);
        }
    }
    if first_read {
        list.keep(&m.faults.0);
    }
    Some(Ready {
        directory: gpu,
        faults: m.faults.2,
        base,
    })
}

/// Which numbered copies a dispatch declares written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Declare {
    /// All of them, once per list (and those numbered since): as before.
    All,
    /// Those its root views and constants name (`VICTORY_NARROW_BVH=2`).
    Named,
    None,
}

/// [`Declare`] for a dispatch at `VICTORY_NARROW_BVH` `level` whose shader
/// stores (`writes`) and may store by raw address (`by_address`).
fn plan(level: u8, writes: bool, by_address: bool) -> Declare {
    match (writes, level, by_address) {
        (false, ..) => Declare::None,
        (true, 0, _) => Declare::All,
        // The program reaches the copies only to read them.
        (true, _, false) => Declare::None,
        (true, 1, true) => Declare::All,
        (true, _, true) => Declare::Named,
    }
}

#[cfg(test)]
mod plan_tests {
    use super::{Declare, plan};

    #[test]
    fn only_programs_that_store_by_address_declare_the_copies() {
        // Off: as before, whatever the program does.
        assert_eq!(plan(0, true, false), Declare::All);
        assert_eq!(plan(0, true, true), Declare::All);
        assert_eq!(plan(0, false, true), Declare::None);
        // 1 (exact): a program without stores by address declares none.
        assert_eq!(plan(1, true, false), Declare::None);
        assert_eq!(plan(1, true, true), Declare::All);
        assert_eq!(plan(1, false, true), Declare::None);
        // 2: only what the dispatch names, for one that stores by address.
        assert_eq!(plan(2, true, true), Declare::Named);
        assert_eq!(plan(2, true, false), Declare::None);
        assert_eq!(plan(2, false, false), Declare::None);
    }
}

/// Evicts cached descriptor-run state for `heap` when its owning `Heap` is
/// dropped so a later `Heap` allocated at the same address cannot reuse stale
/// `(base, made)` slot offsets.
pub(crate) fn forget_heap(heap: &HostHeap) {
    let key = std::ptr::from_ref(heap) as usize;
    if let Some(m) = MEMORY.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        m.heaps.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{HeapDesc, create_heap, heap};
    use crate::object::{S_OK, This, guid};

    #[test]
    fn a_builder_root_maps_the_backing_that_received_the_header() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        let Some(mut memory) = Memory::new() else { return };
        // The console's raw pointers are 40 bits. A normal host allocation
        // can land above that range on Windows.
        let allocation = (1usize..256).find_map(|i| {
            let hint = (i << 32) as *const std::ffi::c_void;
            let ptr = unsafe { VirtualAlloc(hint, 2 * PAGE as usize, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
            (!ptr.is_null()).then_some(ptr)
        }).expect("two free pages below the console address limit");
        let start = allocation as u64;
        let page = start + PAGE;
        crate::shadow::register(start as usize, start, 2 * PAGE);
        let writer = crate::shadow::find(start, 2 * PAGE).unwrap();
        crate::shadow::register(page as usize, page, PAGE);
        let inner = crate::shadow::find(page, PAGE).unwrap();
        assert!(!Arc::ptr_eq(&writer, &inner));

        memory.map_with_root(page + 256, Some(&writer));
        memory.map_with_root(page, Some(&inner));
        let copy = memory.copy_at(page).unwrap();
        assert!(Arc::ptr_eq(&writer, &memory.copies[copy]));
        let entry = memory.pages.iter().find(|p| u64::from(p.index) == page / PAGE).unwrap();
        assert_eq!(entry.offset, PAGE as u32);

        crate::shadow::unregister(page as usize, page);
        crate::shadow::unregister(start as usize, start);
        unsafe { VirtualFree(allocation, 0, MEM_RELEASE) };
    }

    #[test]
    fn compressed_leaf_groups_skip_padding_and_keep_24_bit_geometry_ids() {
        let pitch = 2048 * 64;
        let mut input = vec![0xffu8; pitch + 3];
        input[..2048 * 3].fill(0);
        input[pitch..].copy_from_slice(&[0x26, 0x13, 0x02]);
        let ids = leaf_geometries(input.as_ptr() as u64, 2049);
        assert_eq!(ids.into_iter().collect::<Vec<_>>(), [0, 0x021326]);
    }

    #[test]
    fn leaf_geometry_streams_include_the_last_vertex_and_index_pages() {
        let mut record = [0u8; 48];
        record[8..12].copy_from_slice(&57u32.to_le_bytes());
        record[16..20].copy_from_slice(&6u32.to_le_bytes());
        record[20..24].copy_from_slice(&3u32.to_le_bytes());
        record[24..32].copy_from_slice(&(2 * PAGE - 4).to_le_bytes());
        record[32..40].copy_from_slice(&(5 * PAGE - 16).to_le_bytes());
        record[40..48].copy_from_slice(&0x8008u64.to_le_bytes());
        let mut pages = Vec::new();
        geometry_streams(&record, &mut pages);
        assert_eq!(pages, [PAGE, 2 * PAGE, 4 * PAGE, 5 * PAGE]);
    }

    #[test]
    fn packed_indices_cover_meshlets_triangles_and_vertex_remaps() {
        let mut header = [0u8; 12];
        header[..4].copy_from_slice(&0x8000_0003u32.to_le_bytes());
        header[4..8].copy_from_slice(&210u32.to_le_bytes());
        header[8..].copy_from_slice(&335u32.to_le_bytes());
        assert_eq!(packed_index_bytes(&header, 630), 1572);
        assert_eq!(packed_index_bytes(&header, 18), 12);
    }

    #[test]
    fn a_builder_root_must_reach_one_copy_across_every_page() {
        let pages = [0, 5, 5, 2];
        assert_eq!(contiguous_copy(&pages, 2 * PAGE - 8, 16), Some(4));
        assert_eq!(contiguous_copy(&pages, 3 * PAGE - 8, 16), None);
        assert_eq!(contiguous_copy(&pages, PAGE - 8, 16), None);
        assert_eq!(contiguous_copy(&pages, PAGE, 0), None);
        assert_eq!(contiguous_copy(&pages, u64::MAX - 7, 16), None);
    }

    #[test]
    fn a_heap_directory_can_add_the_1024th_copy_after_other_tables() {
        if host().is_none() { return; }
        let heap = HostHeap::new(false, 64).unwrap();
        let mut directory = HeapDirectory::new(&heap).unwrap();
        let data = [0u8; 64];
        let shadow = crate::shadow::find(data.as_ptr() as u64, data.len() as u64).unwrap();
        let mut copies = vec![shadow.clone(); 1023];
        let mut pages = vec![Page { index: 1, copy: 0, offset: 64 }];
        directory.publish(&heap, &copies, &pages).unwrap();
        let occupied = heap.dynamic(7).unwrap();
        copies.push(shadow);
        pages.push(Page { index: 2, copy: 1023, offset: 128 });
        directory.publish(&heap, &copies, &pages).unwrap();
        let slot = directory.slots[1023];
        assert!(!(occupied..occupied + 7).contains(&slot));
        // SAFETY: rows one and two were published above. No GPU work is in flight.
        let rows = unsafe { std::slice::from_raw_parts(directory.buffer.1.add(2), 4) };
        assert_eq!(directory.null.wrapping_add(rows[0]), directory.slots[0]);
        assert_eq!(directory.null.wrapping_add(rows[2]), slot);
        assert_eq!((rows[1], rows[3]), (64, 128));
    }

    #[test]
    fn header_upload_maps_both_ends_of_cross_page_writes() {
        let mut records = [[0u64; 5]; 2];
        records[0][0] = 2 * PAGE - 16;
        records[1][0] = 3 * PAGE;
        records[1][1] = 5 * PAGE - 8;
        let mut pages = Vec::new();
        header_destinations(records.as_ptr() as u64, 2, &mut pages);
        assert_eq!(pages, [2 * PAGE - 16, 2 * PAGE + 47, 3 * PAGE, 5 * PAGE - 8, 5 * PAGE + 15]);
    }

    #[test]
    fn dropping_a_view_heap_evicts_its_memory_heaps_entry() {
        let Some(host) = crate::host::host() else {
            return;
        };
        let iid = guid(
            0x8efb471d,
            0x616c,
            0x4f49,
            [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
        );
        let desc = HeapDesc {
            kind: 0,
            count: 64,
            flags: 1,
            nodes: 1,
        };
        let mut h: This = std::ptr::null();
        assert_eq!(create_heap(std::ptr::null(), &desc, &iid, &mut h), S_OK);
        let list = host.new_list().unwrap();
        let host_heap = heap(h).host().expect("host view heap");
        let key = std::ptr::from_ref(host_heap) as usize;

        assert!(ready(&list, host_heap, &[], &[], false, true).is_some());
        {
            let guard = MEMORY.lock().unwrap();
            assert!(guard.as_ref().unwrap().heaps.contains_key(&key));
        }

        // Release the title heap (COM vtable slot 2): `Heap::drop` must evict `key`.
        // SAFETY: `h` is a live COM object from `create_heap`.
        let release: extern "system" fn(This) -> u32 =
            unsafe { std::mem::transmute(*(*(h as *const *const *const ())).add(2)) };
        assert_eq!(release(h), 0);

        let guard = MEMORY.lock().unwrap();
        assert!(
            !guard.as_ref().unwrap().heaps.contains_key(&key),
            "MEMORY.heaps retained dropped HostHeap key {key:#x}"
        );
    }
}
