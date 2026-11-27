//! Every pipeline the title has ever created, kept on disk
//! (`SCARLETT_PIPELINE_BOOK=1`), and their host pipelines made ahead of a run
//! (`scarlett warm`), so that the driver's own cache on disk has them before
//! a draw asks for one.
//!
//! The title creates 84,000 pipelines while it loads and draws with some
//! 2,000 of them; which ones is known from earlier runs (`library`), and
//! those are made during loading. One met for the first time (a place never
//! seen, or everything after a change of the translator or of the driver) is
//! compiled when a draw wants it. Making all of them in the running title
//! (`SCARLETT_EARLY_ALL=1`) ended it inside the driver's compiler; made here,
//! a compiler that crashes or hangs ends a child process, costs that pipeline
//! alone and is named in `warm-bad.txt`.
//!
//! Two files beside the library, both only ever added to (a torn end is cut
//! off): `known-shaders.bin` (stage, hash, length, container) and
//! `known-pipelines.bin` (length, then the key, the shaders' stages and
//! hashes, and the graphics state). They hold the title's own code: they stay
//! in the cache folder.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::PCSTR;

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

pub(super) const SHADERS: &str = "known-shaders.bin";
pub(super) const PIPELINES: &str = "known-pipelines.bin";
/// Compiler crashes and hangs recorded by `scarlett warm` and the runtime guard.
pub const WARM_BAD_FILE: &str = "warm-bad.txt";
/// Stages by their number in the files.
pub(super) const STAGES: [&str; 8] =
    ["vertex", "pixel", "domain", "hull", "geometry", "compute", "amplification", "mesh"];

/// `SCARLETT_PIPELINE_BOOK=1`: the pipelines the title creates are kept.
fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SCARLETT_PIPELINE_BOOK").is_some_and(|v| v == "1"))
}

struct Book {
    dir: PathBuf,
    keys: HashSet<[u8; 16]>,
    shaders: HashSet<[u8; 32]>,
    /// Not yet written: shaders, then pipelines.
    new: [Vec<u8>; 2],
    added: usize,
}

static BOOK: OnceLock<Option<Mutex<Book>>> = OnceLock::new();

fn book() -> Option<&'static Mutex<Book>> {
    BOOK.get_or_init(|| {
        let dir = super::library::folder().filter(|_| on())?;
        std::fs::create_dir_all(&dir).ok()?;
        let mut keys = HashSet::new();
        let mut shaders = HashSet::new();
        let ends = [
            walk_shaders(&dir, |_, hash, _, _| drop(shaders.insert(hash))),
            walk_pipelines(&dir, |_, bytes| {
                keys.extend(bytes.first_chunk::<16>().copied());
            }),
        ];
        for (file, end) in [SHADERS, PIPELINES].iter().zip(ends) {
            let path = dir.join(file);
            if std::fs::metadata(&path).is_ok_and(|m| m.len() > end)
                && let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path)
            {
                let _ = f.set_len(end);
            }
        }
        tracing::info!(pipelines = keys.len(), shaders = shaders.len(), "title pipelines known from earlier runs");
        let spawned = std::thread::Builder::new().name("scarlett-known".into()).spawn(|| {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                flush();
            }
        });
        spawned.ok()?;
        Some(Mutex::new(Book { dir, keys, shaders, new: Default::default(), added: 0 }))
    })
    .as_ref()
}

/// The title created a pipeline.
pub(super) fn note(key: &str, shaders: &[Arc<Shader>], graphics: Option<&GraphicsState>) {
    let Some(book) = book() else { return };
    let mut id = [0u8; 16];
    for (at, b) in id.iter_mut().enumerate() {
        *b = key.get(2 * at..2 * at + 2).and_then(|x| u8::from_str_radix(x, 16).ok()).unwrap_or(0);
    }
    let mut book = book.lock().unwrap_or_else(|e| e.into_inner());
    if shaders.is_empty() || !book.keys.insert(id) {
        return;
    }
    let mut entry = id.to_vec();
    entry.push(shaders.len() as u8);
    for s in shaders {
        let hash = *blake3::hash(&s.container).as_bytes();
        let stage = STAGES.iter().position(|&x| x == s.stage).unwrap_or(0) as u8;
        entry.push(stage);
        entry.extend(hash);
        if book.shaders.insert(hash) {
            let new = &mut book.new[0];
            new.push(stage);
            new.extend(hash);
            new.extend((s.container.len() as u32).to_le_bytes());
            new.extend(&s.container);
        }
    }
    entry.push(u8::from(graphics.is_some()));
    if let Some(g) = graphics {
        g.put(&mut entry);
    }
    book.new[1].extend((entry.len() as u32).to_le_bytes());
    book.new[1].extend(entry);
    book.added += 1;
}

/// Writes what was noted.
fn flush() {
    let Some(Some(book)) = BOOK.get() else { return };
    let (dir, new, added) = {
        let mut book = book.lock().unwrap_or_else(|e| e.into_inner());
        if book.added == 0 {
            return;
        }
        (book.dir.clone(), std::mem::take(&mut book.new), std::mem::take(&mut book.added))
    };
    for (file, bytes) in [SHADERS, PIPELINES].iter().zip(&new) {
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(file))
            .and_then(|mut f| f.write_all(bytes));
        if let Err(e) = written {
            tracing::warn!(error = %e, file, "title pipelines not written down");
            return;
        }
    }
    tracing::info!(pipelines = added, "title pipelines written down");
}

/// Calls `f` with each container's stage, hash, place and length; where the
/// last whole one ends.
pub(super) fn walk_shaders(dir: &Path, mut f: impl FnMut(u8, [u8; 32], u64, u32)) -> u64 {
    let Ok(file) = std::fs::File::open(dir.join(SHADERS)) else { return 0 };
    let len = file.metadata().map_or(0, |m| m.len());
    let mut file = std::io::BufReader::new(file);
    let mut at = 0;
    let mut head = [0u8; 37];
    while file.read_exact(&mut head).is_ok() {
        let size = u32::from_le_bytes([head[33], head[34], head[35], head[36]]);
        if at + 37 + u64::from(size) > len || file.seek_relative(i64::from(size)).is_err() {
            break;
        }
        let mut hash = [0; 32];
        hash.copy_from_slice(&head[1..33]);
        f(head[0], hash, at + 37, size);
        at += 37 + u64::from(size);
    }
    at
}

/// Calls `f` with each pipeline's number and entry; where the last whole one
/// ends.
pub(super) fn walk_pipelines(dir: &Path, mut f: impl FnMut(usize, &[u8])) -> u64 {
    let bytes = std::fs::read(dir.join(PIPELINES)).unwrap_or_default();
    let (mut at, mut n) = (0, 0);
    while let Some(head) = bytes.get(at..at + 4) {
        let size = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let Some(entry) = bytes.get(at + 4..at + 4 + size) else { break };
        f(n, entry);
        (at, n) = (at + 4 + size, n + 1);
    }
    at as u64
}

/// How many pipelines are known in `dir`.
pub fn count(dir: &Path) -> usize {
    let mut n = 0;
    walk_pipelines(dir, |_, _| n += 1);
    n
}

/// The key of each pipeline known in `dir`, as the early list has it.
pub fn keys(dir: &Path) -> Vec<String> {
    let mut all = Vec::new();
    walk_pipelines(dir, |_, e| {
        all.push(e.get(..16).unwrap_or_default().iter().map(|b| format!("{b:02x}")).collect());
    });
    all
}

/// What a driver's cached pipelines depend on besides the title's: the
/// translator with its settings, and the driver.
pub fn fingerprint() -> String {
    let mut h = blake3::Hasher::new();
    h.update(&crate::shader_cache::translation());
    if let Some(host) = crate::host::host() {
        h.update(&host.driver().to_le_bytes());
    }
    h.finalize().to_hex()[..16].to_owned()
}

/// Whether `warm-bad.txt` is ignored (`SCARLETT_WARM_BAD=0`).
pub fn bad_list_disabled() -> bool {
    std::env::var_os("SCARLETT_WARM_BAD").is_some_and(|v| v == "0")
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Formats a `warm-bad.txt` entry with the pipeline key in column one (so
/// older binaries still quarantine the key) and a tagged fingerprint so
/// driver or translator updates retry it automatically.
pub fn format_bad_entry(key: &str, fingerprint: &str, reason: &str) -> String {
    if reason.is_empty() {
        format!("{key} fp={fingerprint}")
    } else {
        format!("{key} fp={fingerprint} {reason}")
    }
}

/// Pipeline keys from `warm-bad.txt` that apply to `fingerprint`.
///
/// Blank lines and `#` comments are ignored. Entries tagged with a
/// fingerprint (`<key> fp=<16-hex-fingerprint> ...`, or
/// `<16-hex-fingerprint> <32-hex-key> ...`) apply only when their
/// fingerprint matches `fingerprint`, so a driver or translator update
/// automatically retries them. Legacy or manually added bare-key lines
/// (`<key> ...`) continue to apply regardless of fingerprint.
pub fn parse_bad_keys(text: &str, fingerprint: &str) -> HashSet<String> {
    let mut keys = HashSet::new();
    for line in text.lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(first) = parts.next() else {
            continue;
        };
        if let Some(second) = parts.next() {
            if let Some(fp) = second.strip_prefix("fp=")
                && is_hex(fp, 16)
            {
                if fp == fingerprint {
                    keys.insert(first.to_owned());
                }
                continue;
            }
            if is_hex(first, 16) && is_hex(second, 32) {
                if first == fingerprint {
                    keys.insert(second.to_owned());
                }
                continue;
            }
        }
        keys.insert(first.to_owned());
    }
    keys
}

/// The key a `warm-bad.txt` line names, as [`parse_bad_keys`] reads it.
fn bad_line_key(line: &str) -> Option<&str> {
    let line = line.trim_start();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut parts = line.split_whitespace();
    let first = parts.next()?;
    match parts.next() {
        Some(second) if is_hex(first, 16) && is_hex(second, 32) => Some(second),
        _ => Some(first),
    }
}

/// `warm-bad.txt` without the lines that name one of `keys`.
pub fn without_keys(text: &str, keys: &HashSet<String>) -> String {
    let mut out = String::new();
    for line in text.lines() {
        if bad_line_key(line).is_some_and(|key| keys.contains(key)) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The translations of earlier runs are in `dir` (the pipeline library is
/// not used: what is made here is the driver's to keep).
pub fn use_translations(dir: PathBuf) {
    crate::shader_cache::use_shader_cache(dir);
}

/// Makes the host pipelines of those of `which` (numbers in the file) and
/// lets each go again: the driver's cache on disk keeps them. `each` is told
/// the number before, and what came of it after: milliseconds and whether
/// there is one.
pub fn make(dir: &Path, which: &[usize], mut each: impl FnMut(usize, Option<(f64, bool)>)) {
    let mut places = HashMap::new();
    walk_shaders(dir, |stage, hash, at, len| drop(places.insert(hash, (stage, at, len))));
    let Ok(mut file) = std::fs::File::open(dir.join(SHADERS)) else { return };
    let wanted: HashSet<usize> = which.iter().copied().collect();
    let mut entries = HashMap::new();
    walk_pipelines(dir, |n, bytes| {
        if wanted.contains(&n) {
            entries.insert(n, bytes.to_vec());
        }
    });
    let mut read = |hash: &[u8; 32]| {
        let &(_, at, len) = places.get(hash)?;
        let mut bytes = vec![0; len as usize];
        file.seek(SeekFrom::Start(at)).ok()?;
        file.read_exact(&mut bytes).ok()?;
        Some(bytes)
    };
    for n in which {
        let Some(entry) = entries.get(n) else { continue };
        each(*n, None);
        let began = std::time::Instant::now();
        let made = parse(entry, &mut read)
            .and_then(|(shaders, g)| HostPipeline::new(&shaders, g.as_deref()))
            .is_some();
        each(*n, Some((began.elapsed().as_secs_f64() * 1e3, made)));
    }
}

/// `scarlett prepare`: makes the host pipelines of those of `which` on
/// `jobs` threads into the pipeline library in `dir` (the one runs load
/// from), then saves it, so that a run loads them instead of compiling them
/// while it loads. One already in the library is only loaded. `each` is told
/// what came of each: milliseconds and whether there is one.
pub fn fill_library(dir: &Path, which: &[usize], jobs: usize, each: impl Fn(usize, f64, bool) + Sync) {
    super::library::use_pipeline_cache(dir.to_path_buf());
    let mut places = HashMap::new();
    walk_shaders(dir, |stage, hash, at, len| drop(places.insert(hash, (stage, at, len))));
    let wanted: HashSet<usize> = which.iter().copied().collect();
    let mut entries = HashMap::new();
    walk_pipelines(dir, |n, bytes| {
        if wanted.contains(&n) {
            entries.insert(n, bytes.to_vec());
        }
    });
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..jobs.max(1).min(which.len().max(1)) {
            s.spawn(|| {
                let Ok(mut file) = std::fs::File::open(dir.join(SHADERS)) else { return };
                let mut read = |hash: &[u8; 32]| {
                    let &(_, at, len) = places.get(hash)?;
                    let mut bytes = vec![0; len as usize];
                    file.seek(SeekFrom::Start(at)).ok()?;
                    file.read_exact(&mut bytes).ok()?;
                    Some(bytes)
                };
                while let Some(&n) = which.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                    let Some(entry) = entries.get(&n) else { continue };
                    let began = std::time::Instant::now();
                    let made = parse(entry, &mut read)
                        .and_then(|(shaders, g)| HostPipeline::new(&shaders, g.as_deref()))
                        .is_some();
                    each(n, began.elapsed().as_secs_f64() * 1e3, made);
                }
            });
        }
    });
    super::library::save(true);
}

#[allow(clippy::type_complexity)]
fn parse(
    entry: &[u8],
    read: &mut impl FnMut(&[u8; 32]) -> Option<Vec<u8>>,
) -> Option<(Vec<Arc<Shader>>, Option<Box<GraphicsState>>)> {
    let mut rest = entry.get(16..)?;
    let (&count, after) = rest.split_first()?;
    rest = after;
    let mut shaders = Vec::new();
    for _ in 0..count {
        let (head, after) = rest.split_first_chunk::<33>()?;
        rest = after;
        let mut hash = [0; 32];
        hash.copy_from_slice(&head[1..]);
        shaders.push(super::shader(STAGES.get(usize::from(head[0]))?, &read(&hash)?));
    }
    let (&graphics, rest) = rest.split_first()?;
    let g = match graphics {
        0 => None,
        _ => Some(Box::new(GraphicsState::get(rest)?)),
    };
    Some((shaders, g))
}

/// Calls `$f` with each number of a graphics state but its input elements',
/// as a place of 32 bits, in one order.
macro_rules! numbers {
    ($g:expr, $f:ident) => {{
        $f!($g.blend.AlphaToCoverageEnable.0);
        $f!($g.blend.IndependentBlendEnable.0);
        for t in &mut $g.blend.RenderTarget {
            $f!(t.BlendEnable.0);
            $f!(t.LogicOpEnable.0);
            $f!(t.SrcBlend.0);
            $f!(t.DestBlend.0);
            $f!(t.BlendOp.0);
            $f!(t.SrcBlendAlpha.0);
            $f!(t.DestBlendAlpha.0);
            $f!(t.BlendOpAlpha.0);
            $f!(t.LogicOp.0);
            $f!(t.RenderTargetWriteMask);
        }
        $f!($g.sample_mask);
        $f!($g.rasterizer.FillMode.0);
        $f!($g.rasterizer.CullMode.0);
        $f!($g.rasterizer.FrontCounterClockwise.0);
        $f!($g.rasterizer.DepthBias);
        $f!($g.rasterizer.DepthClipEnable.0);
        $f!($g.rasterizer.MultisampleEnable.0);
        $f!($g.rasterizer.AntialiasedLineEnable.0);
        $f!($g.rasterizer.ForcedSampleCount);
        $f!($g.rasterizer.ConservativeRaster.0);
        $f!($g.depth.DepthEnable.0);
        $f!($g.depth.DepthWriteMask.0);
        $f!($g.depth.DepthFunc.0);
        $f!($g.depth.StencilEnable.0);
        $f!($g.depth.StencilReadMask);
        $f!($g.depth.StencilWriteMask);
        for face in [&mut $g.depth.FrontFace, &mut $g.depth.BackFace] {
            $f!(face.StencilFailOp.0);
            $f!(face.StencilDepthFailOp.0);
            $f!(face.StencilPassOp.0);
            $f!(face.StencilFunc.0);
        }
        $f!($g.strip_cut.0);
        $f!($g.topology.0);
        $f!($g.targets.NumRenderTargets);
        for format in &mut $g.targets.RTFormats {
            $f!(format.0);
        }
        $f!($g.depth_format.0);
        $f!($g.samples.Count);
        $f!($g.samples.Quality);
    }};
}

impl GraphicsState {
    /// The state as bytes.
    pub(super) fn put(&self, out: &mut Vec<u8>) {
        out.push(u8::from(self.depth_bounds));
        for f in [self.rasterizer.DepthBiasClamp, self.rasterizer.SlopeScaledDepthBias] {
            out.extend(f.to_le_bytes());
        }
        let mut plain = GraphicsState {
            blend: self.blend,
            sample_mask: self.sample_mask,
            rasterizer: self.rasterizer,
            depth: self.depth,
            strip_cut: self.strip_cut,
            topology: self.topology,
            targets: self.targets,
            depth_format: self.depth_format,
            samples: self.samples,
            ..Default::default()
        };
        macro_rules! write {
            ($x:expr) => {
                out.extend(($x as u32).to_le_bytes())
            };
        }
        numbers!(plain, write);
        out.push(self.inputs.len() as u8);
        for (e, name) in self.inputs.iter().zip(&self.names) {
            let name = name.as_bytes();
            out.push(name.len() as u8);
            out.extend(name);
            for v in [
                e.SemanticIndex,
                e.Format.0 as u32,
                e.InputSlot,
                e.AlignedByteOffset,
                e.InputSlotClass.0 as u32,
                e.InstanceDataStepRate,
            ] {
                out.extend(v.to_le_bytes());
            }
        }
    }

    pub(super) fn get(bytes: &[u8]) -> Option<GraphicsState> {
        let mut g = GraphicsState::default();
        let (&bounds, mut rest) = bytes.split_first()?;
        g.depth_bounds = bounds != 0;
        let mut next = || {
            let (head, after) = rest.split_first_chunk::<4>()?;
            rest = after;
            Some(u32::from_le_bytes(*head))
        };
        g.rasterizer.DepthBiasClamp = f32::from_bits(next()?);
        g.rasterizer.SlopeScaledDepthBias = f32::from_bits(next()?);
        macro_rules! read {
            ($x:expr) => {
                $x = next()? as _
            };
        }
        numbers!(g, read);
        let (&count, mut rest) = rest.split_first()?;
        let mut elements = Vec::new();
        for _ in 0..count {
            let (&len, after) = rest.split_first()?;
            let (name, after) = after.split_at_checked(usize::from(len))?;
            let (numbers, after) = after.split_first_chunk::<24>()?;
            rest = after;
            let mut v = [0u32; 6];
            for (v, b) in v.iter_mut().zip(numbers.chunks_exact(4)) {
                *v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            }
            elements.push((CString::new(name).ok()?, v));
        }
        g.names = elements.iter().map(|e| e.0.clone()).collect();
        g.inputs = g
            .names
            .iter()
            .zip(&elements)
            .map(|(name, (_, v))| D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(name.as_ptr().cast()),
                SemanticIndex: v[0],
                Format: DXGI_FORMAT(v[1] as i32),
                InputSlot: v[2],
                AlignedByteOffset: v[3],
                InputSlotClass: D3D12_INPUT_CLASSIFICATION(v[4] as i32),
                InstanceDataStepRate: v[5],
            })
            .collect();
        Some(g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state with `count` targets (none past two) and blending on the
    /// first; the second has another description unless `same`, and those
    /// D3D12 does not read are the register image's copy of the first with
    /// a write mask of 0.
    fn blended(independent: bool, count: u32, same: bool) -> GraphicsState {
        let mut g = GraphicsState {
            depth_format: DXGI_FORMAT_D32_FLOAT,
            ..Default::default()
        };
        g.names = vec![CString::new("POSITION").unwrap()];
        g.inputs = vec![D3D12_INPUT_ELEMENT_DESC {
            SemanticName: PCSTR(g.names[0].as_ptr().cast()),
            Format: DXGI_FORMAT_R32G32B32_FLOAT,
            ..Default::default()
        }];
        g.topology = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
        g.targets.NumRenderTargets = count;
        let formats = [DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT];
        for (at, f) in formats.into_iter().enumerate().take(count as usize) {
            g.targets.RTFormats[at] = f;
        }
        g.blend.IndependentBlendEnable = independent.into();
        let first = &mut g.blend.RenderTarget[0];
        first.BlendEnable = true.into();
        first.SrcBlend = D3D12_BLEND_SRC_ALPHA;
        first.DestBlend = D3D12_BLEND_INV_SRC_ALPHA;
        let first = *first;
        for (i, t) in g.blend.RenderTarget.iter_mut().enumerate().skip(1) {
            *t = D3D12_RENDER_TARGET_BLEND_DESC {
                RenderTargetWriteMask: 0,
                ..first
            };
            if i == 1 && same && count > 1 {
                *t = first;
            } else if i == 1 && independent && count > 1 {
                *t = D3D12_RENDER_TARGET_BLEND_DESC {
                    RenderTargetWriteMask: 3,
                    ..GraphicsState::default().blend.RenderTarget[1]
                };
            }
        }
        g
    }

    /// `make()`'s state with its `k`th number (in `numbers!` order) changed,
    /// or `None` past the last.
    fn bumped(make: &dyn Fn() -> GraphicsState, k: usize) -> Option<GraphicsState> {
        let mut g = make();
        let mut at = 0;
        macro_rules! bump {
            ($x:expr) => {{
                if at == k {
                    $x = (($x as u32) ^ 1) as _;
                }
                at += 1;
            }};
        }
        numbers!(g, bump);
        (k < at).then_some(g)
    }

    /// Two states that differ only in the blend D3D12 does not read have one
    /// title pipeline key and library name, and the host is given one blend
    /// state for them; a change of any other number, or of what is not a
    /// number, gives another key. Each change still shows in `describe()`.
    #[test]
    fn the_key_leaves_out_only_the_blend_d3d12_does_not_read() {
        // Numbers 2 + 10 i to 11 + 10 i are target i's blend; 1 is
        // independent blending.
        let targets = |from: usize| move |k: usize| (2 + 10 * from..82).contains(&k);
        let and_independent = |from: usize| move |k: usize| k == 1 || targets(from)(k);
        type Case = (
            &'static str,
            Box<dyn Fn() -> GraphicsState>,
            Box<dyn Fn(usize) -> bool>,
        );
        let cases: [Case; 5] = [
            (
                "independent, two targets",
                Box::new(|| blended(true, 2, false)),
                Box::new(targets(2)),
            ),
            (
                "one description, two targets",
                Box::new(|| blended(false, 2, false)),
                Box::new(targets(1)),
            ),
            (
                "independent, both alike",
                Box::new(|| blended(true, 2, true)),
                Box::new(and_independent(2)),
            ),
            (
                "independent, no targets",
                Box::new(|| blended(true, 0, false)),
                Box::new(and_independent(0)),
            ),
            (
                "one description, no targets",
                Box::new(|| blended(false, 0, false)),
                Box::new(and_independent(0)),
            ),
        ];
        for (name, make, ignored) in &cases {
            let base = make();
            let mut k = 0;
            while let Some(g) = bumped(make.as_ref(), k) {
                assert_ne!(
                    g.describe(),
                    base.describe(),
                    "{name}: number {k} is not in describe()"
                );
                assert_eq!(
                    g.key_text() == base.key_text(),
                    ignored(k),
                    "{name}: number {k}"
                );
                if ignored(k) {
                    assert_eq!(g.made_blend(), base.made_blend(), "{name}: number {k}");
                }
                k += 1;
            }
            assert!(k > 100, "{name}: {k} numbers");
            // All that D3D12 does not read at D3D12's defaults at once.
            let mut plain = make();
            let default = GraphicsState::default().blend.RenderTarget[0];
            for (i, t) in plain.blend.RenderTarget.iter_mut().enumerate() {
                if ignored(2 + 10 * i) {
                    *t = default;
                }
            }
            if ignored(1) {
                plain.blend.IndependentBlendEnable = false.into();
            }
            assert_ne!(plain.describe(), base.describe(), "{name}");
            assert_eq!(plain.key_text(), base.key_text(), "{name}");
        }
        let changes: [fn(&mut GraphicsState); 6] = [
            |g| g.rasterizer.DepthBiasClamp = 0.5,
            |g| g.rasterizer.SlopeScaledDepthBias = 2.0,
            |g| g.depth_bounds = true,
            |g| {
                g.names[0] = CString::new("NORMAL").unwrap();
                g.inputs[0].SemanticName = PCSTR(g.names[0].as_ptr().cast());
            },
            |g| g.inputs[0].AlignedByteOffset = 12,
            |g| {
                g.inputs.clear();
                g.names.clear();
            },
        ];
        let base = blended(true, 2, false);
        for (n, change) in changes.iter().enumerate() {
            let mut g = blended(true, 2, false);
            change(&mut g);
            assert_ne!(g.key_text(), base.key_text(), "change {n}");
        }
    }

    #[test]
    fn a_state_comes_back_as_it_was() {
        let mut g = GraphicsState { sample_mask: 7, depth_bounds: true, ..Default::default() };
        g.rasterizer.DepthBias = -3;
        g.rasterizer.SlopeScaledDepthBias = 1.25;
        g.blend.RenderTarget[3].RenderTargetWriteMask = 5;
        g.depth.StencilReadMask = 9;
        g.targets.NumRenderTargets = 2;
        g.targets.RTFormats[1] = DXGI_FORMAT_R8_UNORM;
        g.depth_format = DXGI_FORMAT_D32_FLOAT;
        g.names = vec![CString::new("POSITION").unwrap(), CString::new("T").unwrap()];
        g.inputs = g
            .names
            .iter()
            .enumerate()
            .map(|(n, name)| D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(name.as_ptr().cast()),
                SemanticIndex: n as u32,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 2,
                AlignedByteOffset: D3D12_APPEND_ALIGNED_ELEMENT,
                InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA,
                InstanceDataStepRate: 4,
            })
            .collect();
        let mut bytes = Vec::new();
        g.put(&mut bytes);
        let back = GraphicsState::get(&bytes).expect("whole");
        assert_eq!(back.describe(), g.describe());
        assert!(GraphicsState::get(&bytes[..bytes.len() - 1]).is_none());
        assert!(GraphicsState::get(&bytes[..9]).is_none());
    }

    #[test]
    fn a_torn_end_is_not_read() {
        let dir = std::env::temp_dir().join(format!("scarlett-known-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut bytes = Vec::new();
        for n in 0..3u8 {
            bytes.extend(20u32.to_le_bytes());
            bytes.extend([n; 20]);
        }
        std::fs::write(dir.join(PIPELINES), &bytes[..bytes.len() - 5]).unwrap();
        assert_eq!(count(&dir), 2);
        assert_eq!(keys(&dir)[1], "01".repeat(16));
        assert_eq!(walk_pipelines(&dir, |_, _| ()), 48);
        let mut shader = vec![5u8];
        shader.extend([9; 32]);
        shader.extend(4u32.to_le_bytes());
        shader.extend([1, 2, 3, 4]);
        let one = shader.len() as u64;
        shader.extend_from_slice(&shader.clone()[..20]);
        std::fs::write(dir.join(SHADERS), &shader).unwrap();
        let mut seen = Vec::new();
        assert_eq!(walk_shaders(&dir, |stage, hash, at, len| seen.push((stage, hash[0], at, len))), one);
        assert_eq!(seen, [(5, 9, 37, 4)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_keys_expire_with_fingerprint_and_skip_comments() {
        let k_stale = "a".repeat(32);
        let k_warm = "b".repeat(32);
        let k_no_reason = "c".repeat(32);
        let k_tab = "d".repeat(32);
        let k_fp_first = "e".repeat(32);
        let k_legacy = "f".repeat(32);
        let k_bare = "1".repeat(32);
        let k_bad_tag = "2".repeat(32);

        let warm_line = format_bad_entry(&k_warm, "8899aabbccddeeff", "hung for 180 s");
        assert_eq!(warm_line.split(' ').next(), Some(k_warm.as_str()));
        let stale_line = format_bad_entry(&k_stale, "0011223344556677", "exit code: 0xc0000005");
        let no_reason_line = format_bad_entry(&k_no_reason, "8899aabbccddeeff", "");
        assert_eq!(no_reason_line, format!("{k_no_reason} fp=8899aabbccddeeff"));

        let text = format!(
            "\n   \n# comment\n   # indented comment\n\
             {stale_line}\n\
             {warm_line}\n\
             {no_reason_line}\n\
             \t{k_tab}\tfp=8899aabbccddeeff\tended the compiler\n\
             8899aabbccddeeff {k_fp_first} legacy fp-first\n\
             {k_legacy} legacy crash\n\
             {k_bare}\n\
             manual-skip-key reason\n\
             {k_bad_tag} fp=nothex reason\n"
        );

        assert_eq!(
            parse_bad_keys(&text, "8899aabbccddeeff"),
            HashSet::from([
                k_warm,
                k_no_reason,
                k_tab,
                k_fp_first,
                k_legacy.clone(),
                k_bare.clone(),
                "manual-skip-key".to_owned(),
                k_bad_tag.clone(),
            ])
        );
        assert_eq!(
            parse_bad_keys(&text, "0011223344556677"),
            HashSet::from([
                k_stale,
                k_legacy.clone(),
                k_bare.clone(),
                "manual-skip-key".to_owned(),
                k_bad_tag.clone(),
            ])
        );
        assert_eq!(
            parse_bad_keys(&text, "ffffffffffffffff"),
            HashSet::from([
                k_legacy,
                k_bare,
                "manual-skip-key".to_owned(),
                k_bad_tag,
            ])
        );
    }

    #[test]
    fn keys_made_alone_leave_the_bad_list() {
        let (made, kept, fp_first) = ("a".repeat(32), "b".repeat(32), "c".repeat(32));
        let text = format!(
            "# comment\n{}\n{}\n8899aabbccddeeff {fp_first} legacy\n{made} legacy crash\n",
            format_bad_entry(&made, "8899aabbccddeeff", "exit code: 0xc0000005"),
            format_bad_entry(&kept, "8899aabbccddeeff", "hung for 180 s"),
        );
        let gone = HashSet::from([made.clone(), fp_first.clone()]);
        let left = without_keys(&text, &gone);
        assert_eq!(parse_bad_keys(&left, "8899aabbccddeeff"), HashSet::from([kept]));
        assert!(left.starts_with("# comment\n"), "{left}");
    }
}
