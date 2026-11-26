//! The translated-shader cache as fx-final (26f8afd) has it, kept as the default of fx-cand:
//! keyed by the container's content and by the translator that made it. The two-level cache
//! that keys by settings and rule stamps (`super`) is opt-in with `VICTORY_SHADER_CACHE_KEY=output`.
//!
//! One difference from 26f8afd: the translator named in the key is [`FINAL_SOURCE_HASH`], the
//! source hash of the last `victory-shader` whose default output changed, not this build's
//! `victory_shader::SOURCE_HASH`. Sources change far more often than the default output (new
//! switches, rule notes, tests), so the build's own hash would throw away every translation the
//! cache holds. A change to what the translator makes by default must change that constant.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use victory_pipeline::{Cache, Converter};
use victory_shader::emit::{HostShader, StaticSampler, UserData};
use victory_shader::resource::RootIndexedView;
use victory_shader::{HwStage, RootType};

/// `victory_shader::SOURCE_HASH` of the last translator whose default output changed (see the
/// module notes): the one that reads only constant buffer slots as constant buffers.
const FINAL_SOURCE_HASH: &str = "366a1edf33210d9aa302060578ab9fafdba0127caba70ac5109fe6443042cf5a";

static CACHE: OnceLock<Cache> = OnceLock::new();
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

/// Keeps translated shaders in `dir` from now on.
pub fn use_shader_cache(dir: PathBuf) {
    match Cache::open(dir) {
        Ok(cache) => {
            let _ = CACHE.set(cache);
        }
        Err(e) => tracing::warn!(error = %e, "no shader cache"),
    }
}

/// Shaders found in the cache and translated since the start.
pub(crate) fn counts() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// What identifies a translation besides the container: the translator that made it and the
/// settings it ran with. The translator is named by the BLAKE3 of the sources of
/// `victory-shader` (`victory_shader::SOURCE_HASH`, made by its `build.rs`, from upstream PR
/// Marvisak/VIctory#254 by wbkepler) and by [`CACHE_SCHEMA_VERSION`] (bumped when this file's
/// encoding changes), not by the executable: a rebuild that leaves the translator alone
/// (another crate changed) serves the translations an earlier build cached, and one that
/// changes it translates again. Run with `VICTORY_SHADER_CACHE_KEY=exe` for the BLAKE3 of the
/// executable instead (any new executable then starts with an empty cache; a build patched
/// outside the translator's sources that changes what it makes should).
fn build() -> &'static [u8; 32] {
    static BUILD: OnceLock<[u8; 32]> = OnceLock::new();
    BUILD.get_or_init(|| {
        // The view heap is in the key, so it is probed before the key is made, whatever looks
        // the cache up first (fx-prewarm). A run's first lookup used to be `is_cached`, from the
        // early pipelines' scheduling (`make::translations_ready`), before any translation had
        // probed: such a run keyed with no view limit and no heap map although it translated
        // with them, and a run without early pipelines (a first boot) keyed with them.
        #[cfg(windows)]
        crate::descriptor::host::probe_view_limit();
        with_host_settings(*base(), &HostFacts::now())
    })
}

/// [`build`] as runs before fx-prewarm made it when their first lookup came before the view
/// heap probe (see [`build`]): no view limit and no heap map. Where the compute shaders of a
/// cache those runs filled are: `victory prewarm manifest` looks there too.
fn build_unprobed() -> &'static [u8; 32] {
    static BUILD: OnceLock<[u8; 32]> = OnceLock::new();
    BUILD.get_or_init(|| with_host_settings(*base(), &HostFacts::now().unprobed()))
}

/// [`key`] of this build and settings, before the host facts are mixed in.
fn base() -> &'static [u8; 32] {
    static BASE: OnceLock<[u8; 32]> = OnceLock::new();
    BASE.get_or_init(|| {
        let limit = victory_shader::emit::loop_limit();
        if limit != victory_shader::emit::DEFAULT_LOOP_LIMIT {
            tracing::warn!(
                limit,
                "VICTORY_SHADER_LOOP_LIMIT: translated loops stop sooner"
            );
        }
        let translator = if std::env::var_os("VICTORY_SHADER_CACHE_KEY").is_some_and(|v| v == "exe")
        {
            std::env::current_exe().and_then(std::fs::read)
        } else {
            Ok([&CACHE_SCHEMA_VERSION.to_le_bytes()[..], FINAL_SOURCE_HASH.as_bytes()].concat())
        };
        match translator {
            Ok(translator) => {
                let switches = if FIRST_SWITCHES_ONLY.load(Ordering::Relaxed) {
                    SWITCHES.into_iter().map(|name| (name, std::env::var_os(name))).collect()
                } else {
                    all_switches()
                };
                key(
                    &translator,
                    limit,
                    victory_shader::emit::group_sync(),
                    &switches,
                    victory_shader::emit::shader_opt(),
                )
            }
            Err(e) => {
                // Without it no entry can be trusted: a fresh key per run.
                tracing::warn!(error = %e, "cannot read our executable; shader cache unused");
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                *blake3::hash(&now.as_nanos().to_le_bytes()).as_bytes()
            }
        }
    })
}

/// [`key`] mixed with the settings of this run that change translations
/// beyond the translator's own switches: whether ray-traced passes the
/// translator cannot follow run with every ray missing (`VICTORY_RT_MISS`,
/// VIcky #6), and the view heap the host keeps (`probe_view_limit`, which
/// runs before the first translation): the clamp of view indices and the
/// heap map bindless views are looked up in (`VICTORY_HEAP_MAP`,
/// `VICTORY_HEAP_WINDOW`).
fn with_host_settings(base: [u8; 32], facts: &HostFacts) -> [u8; 32] {
    let mut bytes = base.to_vec();
    bytes.push(u8::from(facts.rt_miss));
    bytes.extend_from_slice(&facts.view_limit.to_le_bytes());
    // (victory-amd) the host's widest wave, which picks how wave64 programs are made.
    bytes.extend_from_slice(&facts.max_wave.to_le_bytes());
    super::append_view_heap_layout(&mut bytes, facts.view_map, facts.table_snapshots);
    // (`VICTORY_F3_VIEW_FIX=1` changes the translation of typed buffer accesses; named only
    // when on, so the key of a run without it is the one of fx-final)
    if facts.f3_view_fix {
        bytes.extend_from_slice(b"VICTORY_F3_VIEW_FIX=1");
    }
    *blake3::hash(&bytes).as_bytes()
}

/// The host facts and settings [`with_host_settings`] mixes into the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HostFacts {
    rt_miss: bool,
    view_limit: u32,
    max_wave: u32,
    view_map: Option<(u32, u32)>,
    /// Copied tables read their own descriptor words (with ray tracing).
    table_snapshots: bool,
    f3_view_fix: bool,
}

impl HostFacts {
    fn now() -> HostFacts {
        HostFacts {
            rt_miss: victory_shader::resource::rt_miss(),
            view_limit: victory_shader::emit::view_heap_limit(),
            max_wave: victory_shader::emit::host_max_wave(),
            view_map: victory_shader::emit::view_heap_map(),
            table_snapshots: victory_shader::emit::table_snapshots(),
            f3_view_fix: victory_shader::emit::words::f3_view_fix(),
        }
    }

    /// As a key made before the view heap probe saw them (see [`build`]).
    fn unprobed(self) -> HostFacts {
        HostFacts { view_limit: 0, view_map: None, table_snapshots: false, ..self }
    }
}

/// The environment switches that change what the translator makes, part of the key.
const SWITCHES: [&str; 8] = [
    "VICTORY_WAVE_WIDE",
    "VICTORY_WAVE_DEEP_SKIPS",
    "VICTORY_WAVE_OWN_BITS",
    // (victory-amd: these three change the translation too)
    "VICTORY_WAVE_LOCAL",
    "VICTORY_STRICT_JOINS",
    "VICTORY_MESH_OUTPUT_STRICT",
    "VICTORY_F16_NEAREST",
    // (VIcky #6: the loop limit changes the translation too)
    "VICTORY_SHADER_LOOP_LIMIT",
];

/// Serialization schema version of [`encode`] / [`decode`]: bump it whenever [`HostShader`]'s
/// on-disk format changes (the sources hash covers the translator, not this file).
const CACHE_SCHEMA_VERSION: u32 = 1;

/// Key by [`SWITCHES`] alone, as fx-cand (before 53f1675b named every switch of the
/// translator): `victory prewarm manifest --cand-key` reads a cache warmed by that build.
/// Set before the first lookup (the key is made once).
static FIRST_SWITCHES_ONLY: AtomicBool = AtomicBool::new(false);

pub(crate) fn key_by_first_switches() {
    FIRST_SWITCHES_ONLY.store(true, Ordering::Relaxed);
}

/// [`SWITCHES`] and every other `VICTORY_*` switch the translator's source
/// names (`victory_shader::SWITCHES`, fxceless 8dd30ab), with their values.
fn all_switches() -> Vec<(&'static str, Option<std::ffi::OsString>)> {
    let extra = victory_shader::SWITCHES.split(',').filter(|n| !n.is_empty() && !SWITCHES.contains(n));
    SWITCHES.into_iter().chain(extra).map(|name| (name, std::env::var_os(name))).filter(super::keyed).collect()
}

/// The key of a translation's context: the translator's identity and the settings that
/// change what it makes (the loop limit, which group barriers wait, the switches that pick
/// the form of a wave64 program, the clean-up passes that ran over the translated code).
fn key(
    translator: &[u8],
    limit: u32,
    group_sync: u8,
    switches: &[(&str, Option<std::ffi::OsString>)],
    shader_opt: u32,
) -> [u8; 32] {
    let mut bytes = translator.to_vec();
    bytes.extend_from_slice(&limit.to_le_bytes());
    bytes.push(group_sync);
    for (name, value) in switches {
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(b'=');
        if let Some(value) = value {
            bytes.extend_from_slice(value.as_encoded_bytes());
        }
        bytes.push(0);
    }
    bytes.extend_from_slice(&shader_opt.to_le_bytes());
    *blake3::hash(&bytes).as_bytes()
}

/// The offline warm-up (`victory translate --warm`, fxceless 8dd30ab):
/// `container` is translated into the cache unless it is there already.
/// `Ok(true)`: it was translated now. Needs [`use_shader_cache`] and the
/// same host facts the run will have. Keys as [`host_shader`] does.
pub(super) fn warm(container: &[u8]) -> Result<bool, String> {
    if CACHE.get().is_none() {
        return Err("no shader cache".into());
    }
    let misses = MISSES.load(Ordering::Relaxed);
    host_shader(container).map(|_| MISSES.load(Ordering::Relaxed) != misses)
}

/// The translation of a console shader container, from the cache when
/// there is one.
pub(crate) fn host_shader(container: &[u8]) -> Result<HostShader, String> {
    use victory_shader::emit;
    if let Some(s) = stand_in(container) {
        return Ok(s);
    }
    // The hash the log names the shader by (`pipeline.rs`), which the
    // per-program switches take.
    let hash = blake3::hash(container).to_hex()[..16].to_owned();
    let translate = |c: &[u8]| emit::for_program(&hash, || emit::translate(c)).map_err(|e| e.to_string());
    let r = match CACHE.get() {
        Some(cache) => {
            cached(cache, &super::translation_key(build(), container, &hash), container, translate)
        }
        None => translate(container),
    };
    match r {
        Err(e) => exposure_stub(container).ok_or(e),
        ok => ok,
    }
}

// A partial group has a different root layout and must have its own cache key.
pub(crate) fn host_shader_thread_limited(container: &[u8]) -> Result<HostShader, String> {
    use victory_shader::emit;
    let hash = blake3::hash(container).to_hex()[..16].to_owned();
    let translate = |c: &[u8]| emit::for_program(&hash, || emit::translate_thread_limited(c)).map_err(|e| e.to_string());
    let Some(cache) = CACHE.get().or_else(|| super::CACHE.get()) else { return translate(container) };
    let mut key = blake3::Hasher::new();
    key.update(&super::translation_key(build(), container, &hash));
    key.update(b"partial thread group v1");
    key.update(victory_shader::SOURCE_HASH.as_bytes());
    cached(cache, key.finalize().as_bytes(), container, translate)
}

/// The stand-ins of `victory_shader::emit::stand_in`, each opt-in:
/// `VICTORY_AMBIENT_STUB=1` (the diffuse illumination's last pass writes
/// `VICTORY_AMBIENT_STUB_VALUE`, 0.1 by default) and
/// `VICTORY_SHADOWMASK_STUB=1` (the ray-traced distant shadows write
/// `VICTORY_SHADOWMASK_STUB_VALUE`, 1 = lit; each output in its own encoding,
/// by slot: `VICTORY_AMBIENT_STUB_SLOTS="152=rgb9e5,208=mask,*=f"`), for the containers in
/// `VICTORY_AMBIENT_STUB_HASHES` / `VICTORY_SHADOWMASK_STUB_HASHES` (the
/// title's by default). They replace those passes whenever switched on: the
/// passes translate, but what they read comes from ray tracing, which does
/// not run on the host yet. Not cached (the cache keys by container).
fn stand_in(container: &[u8]) -> Option<HostShader> {
    use victory_shader::emit::stand_in as s;
    let hash = blake3::hash(container).to_hex()[..16].to_owned();
    let kinds = [
        ("VICTORY_AMBIENT_STUB", s::AMBIENT_HASHES, s::AMBIENT_VALUE, s::ambient_slots(&hash)),
        ("VICTORY_SHADOWMASK_STUB", s::SHADOWMASK_HASHES, s::SHADOWMASK_VALUE, s::SHADOWMASK_SLOTS),
    ];
    for (name, hashes, value, defaults) in kinds {
        if std::env::var(name).ok().as_deref().map(str::trim) != Some("1") {
            continue;
        }
        let listed = match std::env::var(format!("{name}_HASHES")) {
            Ok(v) => v
                .split(|c: char| c == ',' || c.is_whitespace())
                .any(|h| h.eq_ignore_ascii_case(&hash)),
            Err(_) => hashes.contains(&hash.as_str()),
        };
        if !listed {
            continue;
        }
        let value = std::env::var(format!("{name}_VALUE"))
            .ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .unwrap_or(value);
        let slots = std::env::var(format!("{name}_SLOTS")).unwrap_or_default();
        let plan = s::Plan::new(value)
            .with_overrides(defaults)
            .with_overrides(&slots);
        match s::storing_plan(container, plan) {
            Ok((shader, plan)) => {
                tracing::warn!(
                    %hash,
                    stand_in = name,
                    value,
                    outputs = ?plan.seen,
                    "pass replaced by a stand-in writing each output in its own encoding"
                );
                return Some(shader);
            }
            Err(e) => {
                tracing::warn!(%hash, stand_in = name, error = %e, "stand-in does not translate");
            }
        }
    }
    None
}

/// `VICTORY_EXPOSURE_STUB=1`: the auto-exposure average, when it does not
/// translate, replaced by a stand-in writing a constant luminance
/// (`victory_shader::emit::exposure`; `VICTORY_EXPOSURE_STUB_VALUE`, 0.18 by
/// default) for the containers in `VICTORY_EXPOSURE_STUB_HASHES` (16-digit
/// hashes as the pipeline logs them, comma-separated; the title's by
/// default).
fn exposure_stub(container: &[u8]) -> Option<HostShader> {
    use victory_shader::emit::exposure;
    if std::env::var("VICTORY_EXPOSURE_STUB").ok()?.trim() != "1" {
        return None;
    }
    let hash = blake3::hash(container).to_hex()[..16].to_owned();
    let listed = match std::env::var("VICTORY_EXPOSURE_STUB_HASHES") {
        Ok(v) => v
            .split(|c: char| c == ',' || c.is_whitespace())
            .any(|h| h.eq_ignore_ascii_case(&hash)),
        Err(_) => exposure::HASHES.contains(&hash.as_str()),
    };
    if !listed {
        return None;
    }
    let value = std::env::var("VICTORY_EXPOSURE_STUB_VALUE")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .unwrap_or(exposure::VALUE);
    match exposure::stub(container, value) {
        Ok(s) => {
            tracing::warn!(%hash, value, "exposure shader replaced by a constant stand-in");
            Some(s)
        }
        Err(e) => {
            tracing::warn!(%hash, error = %e, "exposure stand-in does not translate either");
            None
        }
    }
}

#[cfg(test)]
use super::program_key;

/// Whether the translation of `container` is in the cache already (nothing is read or
/// translated: the entry's file is looked for).
pub(crate) fn is_cached(container: &[u8]) -> bool {
    // At the key `host_shader` stores under: a wave64 program kept out of the deep-skips
    // form has its own.
    CACHE.get().is_some_and(|cache| has_entry_in(cache, &[*build()], container))
}

/// Whether the cache holds a translation of `container` that a run made: at this build's key
/// ([`is_cached`]) or at the key of a run that looked the cache up before the view heap probe
/// ([`build_unprobed`]). For choosing what to prewarm (`victory prewarm manifest`); what the
/// next run finds is [`is_cached`].
pub(crate) fn is_cached_by_runs(container: &[u8]) -> bool {
    is_cached(container) || CACHE.get().is_some_and(|cache| has_entry_in(cache, &[*build_unprobed()], container))
}

/// Whether `cache` holds the translation of `container` made in one of `contexts`, each at the
/// key [`host_shader`] stores under (the program's own when it is kept out of deep skips).
fn has_entry_in(cache: &Cache, contexts: &[[u8; 32]], container: &[u8]) -> bool {
    let hash = blake3::hash(container).to_hex();
    contexts.iter().any(|c| has_entry(cache, &super::translation_key(c, container, &hash[..16]), container))
}

/// Whether `cache` holds the translation of `container` made in context `build`.
fn has_entry(cache: &Cache, build: &[u8; 32], container: &[u8]) -> bool {
    let converter = Translate {
        translate: |_: &[u8]| Err::<HostShader, String>(String::new()),
        ran: AtomicBool::new(false),
    };
    let input = cache_input(build, container);
    cache
        .path_for(&victory_pipeline::CacheKey::new(&converter, &input))
        .exists()
}

/// The cache entry's converter: the build and the container in, the
/// encoded translation out. `ran` tells a miss from a hit.
struct Translate<F> {
    translate: F,
    ran: AtomicBool,
}

impl<F: Fn(&[u8]) -> Result<HostShader, String> + Send + Sync> Converter for Translate<F> {
    fn id(&self) -> &'static str {
        "shader.dxil"
    }
    fn version(&self) -> u32 {
        // The build fingerprint in the input does the versioning.
        1
    }
    fn convert(&self, input: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.ran.store(true, Ordering::Relaxed);
        let shader = (self.translate)(&input[32..]).map_err(anyhow::Error::msg)?;
        Ok(encode(&shader))
    }
}

/// What a cache entry is made from: the context's key and the container (the entry's name
/// is a hash of it, so [`is_cached`] finds what [`cached`] stored).
fn cache_input(build: &[u8; 32], container: &[u8]) -> Vec<u8> {
    [&build[..], container].concat()
}

fn cached(
    cache: &Cache,
    build: &[u8; 32],
    container: &[u8],
    translate: impl Fn(&[u8]) -> Result<HostShader, String> + Send + Sync,
) -> Result<HostShader, String> {
    let input = cache_input(build, container);
    let converter = Translate {
        translate,
        ran: AtomicBool::new(false),
    };
    for _ in 0..2 {
        let path = cache
            .get_or_convert(&converter, &input)
            .map_err(|e| format!("{e:#}"))?;
        if let Some(shader) = std::fs::read(&path).ok().and_then(|b| decode(&b)) {
            let (counter, what) = if converter.ran.load(Ordering::Relaxed) {
                (&MISSES, "translated and cached")
            } else {
                (&HITS, "from the cache")
            };
            counter.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(path = %path.display(), "shader {what}");
            return Ok(shader);
        }
        tracing::debug!(path = %path.display(), "unreadable shader cache entry; translating again");
        let _ = std::fs::remove_file(&path);
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    (converter.translate)(container)
}

const MAGIC: &[u8; 4] = b"VSH1";
const STAGES: [HwStage; 8] = [
    HwStage::Ls,
    HwStage::Hs,
    HwStage::Es,
    HwStage::Gs,
    HwStage::Vs,
    HwStage::Ps,
    HwStage::Ms,
    HwStage::Cs,
];

fn root_code(r: RootType) -> u32 {
    match r {
        RootType::Constants => 0,
        RootType::Descriptor => 1,
        RootType::Table => 2,
        RootType::ImmediateSampler => 3,
        RootType::ResourceHeap => 4,
        RootType::SamplerHeap => 5,
        RootType::Other(x) => 6 + x,
    }
}

fn root_type(code: u32) -> RootType {
    match code {
        0 => RootType::Constants,
        1 => RootType::Descriptor,
        2 => RootType::Table,
        3 => RootType::ImmediateSampler,
        4 => RootType::ResourceHeap,
        5 => RootType::SamplerHeap,
        x => RootType::Other(x - 6),
    }
}

/// Little-endian words and length-prefixed lists.
fn encode(s: &HostShader) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    let word = |out: &mut Vec<u8>, v: u32| out.extend(v.to_le_bytes());
    let stage = STAGES.iter().position(|&x| x == s.stage).unwrap_or(0) as u32;
    word(&mut out, stage);
    word(&mut out, s.dxil.len() as u32);
    out.extend(&s.dxil);
    word(&mut out, s.user_data.len() as u32);
    for u in &s.user_data {
        for v in [u.index, u32::from(u.slot), root_code(u.root), u.dword] {
            word(&mut out, v);
        }
    }
    word(&mut out, s.samplers.len() as u32);
    for sampler in &s.samplers {
        for &v in sampler {
            word(&mut out, v);
        }
    }
    word(&mut out, s.static_samplers.len() as u32);
    for t in &s.static_samplers {
        let [u, v, w] = t.address;
        let words = [
            t.filter,
            u,
            v,
            w,
            t.mip_lod_bias.to_bits(),
            t.max_anisotropy,
            t.comparison,
            t.border,
            t.min_lod.to_bits(),
            t.max_lod.to_bits(),
        ];
        for v in words {
            word(&mut out, v);
        }
    }
    for slots in [&s.root_cbvs, &s.root_srvs, &s.root_uavs] {
        word(&mut out, slots.len() as u32);
        out.extend(slots);
    }
    word(&mut out, s.root_indexed_views.len() as u32);
    for view in &s.root_indexed_views {
        for value in [
            u32::from(view.table_slot),
            u32::from(view.constant_slot),
            view.constant_dword,
            view.byte_offset,
            u32::from(view.shift),
        ] {
            word(&mut out, value);
        }
    }
    out
}

fn decode(bytes: &[u8]) -> Option<HostShader> {
    struct Reader<'a>(&'a [u8]);
    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let (head, rest) = self.0.split_at_checked(n)?;
            self.0 = rest;
            Some(head)
        }
        fn word(&mut self) -> Option<u32> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
        }
        fn list(&mut self) -> Option<Vec<u8>> {
            let n = self.word()? as usize;
            Some(self.take(n)?.to_vec())
        }
    }
    let mut r = Reader(bytes);
    if r.take(4)? != MAGIC {
        return None;
    }
    let stage = *STAGES.get(r.word()? as usize)?;
    let dxil = r.list()?;
    let n = r.word()? as usize;
    let mut user_data = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        user_data.push(UserData {
            index: r.word()?,
            slot: u8::try_from(r.word()?).ok()?,
            root: root_type(r.word()?),
            dword: r.word()?,
        });
    }
    let n = r.word()? as usize;
    let mut samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        samplers.push([r.word()?, r.word()?, r.word()?, r.word()?]);
    }
    let n = r.word()? as usize;
    let mut static_samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        static_samplers.push(StaticSampler {
            filter: r.word()?,
            address: [r.word()?, r.word()?, r.word()?],
            mip_lod_bias: f32::from_bits(r.word()?),
            max_anisotropy: r.word()?,
            comparison: r.word()?,
            border: r.word()?,
            min_lod: f32::from_bits(r.word()?),
            max_lod: f32::from_bits(r.word()?),
        });
    }
    let root_cbvs = r.list()?;
    let root_srvs = r.list()?;
    let root_uavs = r.list()?;
    let n = r.word()? as usize;
    let mut root_indexed_views = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        root_indexed_views.push(RootIndexedView {
            table_slot: u8::try_from(r.word()?).ok()?,
            constant_slot: u8::try_from(r.word()?).ok()?,
            constant_dword: r.word()?,
            byte_offset: r.word()?,
            shift: u8::try_from(r.word()?).ok().filter(|&s| s < 32)?,
        });
    }
    r.0.is_empty().then_some(HostShader {
        stage,
        dxil,
        user_data,
        samplers,
        static_samplers,
        root_cbvs,
        root_srvs,
        root_uavs,
        root_indexed_views,
        // Not stored (the entry format is fx-final's): a translation made with
        // `VICTORY_D16_UINT=1` comes back without the table places of its
        // 16-bit views, which this title's own unsigned views do not need.
        half_views: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fx-prewarm: a cache filled by runs whose first lookup came before the view heap probe
    /// holds their translations (their compute shaders among them) at the unprobed key. The
    /// probed key misses them; the lookup the prewarm manifest makes finds them.
    #[test]
    fn translations_keyed_before_the_view_heap_probe_are_found_for_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().to_owned()).unwrap();
        let base = [7u8; 32];
        // A capped host (NVIDIA): the probe sets the view limit and the heap map.
        let probed = HostFacts {
            rt_miss: false,
            view_limit: 999_999,
            max_wave: 32,
            view_map: Some((1 << 17, 1_000_000 - (1 << 18) - 2)),
            table_snapshots: false,
            f3_view_fix: false,
        };
        let unprobed = probed.unprobed();
        assert_eq!((unprobed.view_limit, unprobed.view_map, unprobed.max_wave), (0, None, 32));
        let (now, before) = (with_host_settings(base, &probed), with_host_settings(base, &unprobed));
        assert_ne!(now, before, "the probe's facts are in the key");

        // A run stored a translation at the unprobed key.
        let container = b"not a container: no wave64 program, so the context's own key".to_vec();
        let converter = Translate {
            translate: |_: &[u8]| Ok(shader()),
            ran: AtomicBool::new(false),
        };
        cache.get_or_convert(&converter, &cache_input(&before, &container)).unwrap();

        assert!(!has_entry_in(&cache, &[now], &container), "the next run's key misses it");
        assert!(has_entry_in(&cache, &[now, before], &container), "the manifest's lookup finds it");
        assert!(!has_entry_in(&cache, &[now, before], b"another container"));
    }

    fn shader() -> HostShader {
        HostShader {
            stage: HwStage::Ps,
            dxil: b"DXBC and the rest".to_vec(),
            user_data: vec![UserData {
                index: 8,
                slot: 1,
                root: RootType::Table,
                dword: 0,
            }],
            samplers: vec![[0x400, 0xc0_0000, 0x18f0_0000, 0]],
            static_samplers: vec![StaticSampler::from_words([
                0x400,
                0xc0_0000,
                0x18f0_0000,
                0,
            ])],
            root_cbvs: vec![2],
            root_srvs: vec![0, 3],
            root_uavs: vec![1],
            root_indexed_views: vec![RootIndexedView {
                table_slot: 1,
                constant_slot: 2,
                constant_dword: 3,
                byte_offset: 32,
                shift: 5,
            }],
            half_views: Vec::new(),
        }
    }

    #[test]
    fn translations_come_back_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [7u8; 32];
        let container = b"a console container".as_slice();
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 1, "the second came from disk");

        // Another build translates again.
        assert_eq!(cached(&cache, &[8; 32], container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 2);

        // A damaged entry is a miss, translated and stored again.
        for e in walk(dir.path()) {
            std::fs::write(e, b"VSH1 broken").unwrap();
        }
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);

        // Failures are not stored.
        let fail = |_: &[u8]| Err::<HostShader, _>("unsupported".to_owned());
        assert!(cached(&cache, &build, b"other", fail).is_err());
    }

    #[test]
    fn an_entry_is_found_for_the_translator_that_stored_it_only() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let (container, mine, other) = (b"a container".as_slice(), [1; 32], [2; 32]);
        assert!(!has_entry(&cache, &mine, container));
        let translate = |_: &[u8]| Ok(shader());
        cached(&cache, &mine, container, translate).unwrap();
        assert!(has_entry(&cache, &mine, container));
        // another translator's key, or another container, finds nothing
        assert!(!has_entry(&cache, &other, container));
        assert!(!has_entry(&cache, &mine, b"another container"));
    }

    /// The digest `victory-shader`'s `build.rs` makes, from its sources as they are now.
    fn digest_of_translator_sources() -> String {
        fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, PathBuf)>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace(std::path::MAIN_SEPARATOR, "/");
                    out.push((rel, path));
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../victory-shader/src");
        let mut files = Vec::new();
        walk(&root, &root, &mut files);
        files.sort_by(|a, b| a.0.cmp(&b.0));
        let mut hasher = blake3::Hasher::new();
        for (rel, path) in files {
            let contents = std::fs::read(path).unwrap();
            hasher.update(rel.as_bytes());
            hasher.update(&[0]);
            hasher.update(&(contents.len() as u64).to_le_bytes());
            hasher.update(&contents);
        }
        hasher.finalize().to_hex().to_string()
    }

    #[test]
    fn the_source_hash_is_the_translators_sources_digest() {
        // so a changed translator source changes the key (the build script reruns when a file
        // under `victory-shader/src` changes) and nothing else does
        assert_eq!(victory_shader::SOURCE_HASH, digest_of_translator_sources());
    }

    #[test]
    fn the_key_follows_the_translator_and_its_settings_and_nothing_else() {
        let k = |translator: &[u8], limit: u32, wide: Option<&str>| {
            key(
                translator,
                limit,
                1,
                &[("VICTORY_WAVE_WIDE", wide.map(Into::into))],
                3,
            )
        };
        let base = k(b"fingerprint A", 4096, None);
        assert_eq!(
            base,
            k(b"fingerprint A", 4096, None),
            "the same inputs, the same key"
        );
        // another translator: another key
        assert_ne!(base, k(b"fingerprint B", 4096, None));
        // its settings: the loop limit, a switch that picks the wave64 form
        assert_ne!(base, k(b"fingerprint A", 16, None));
        assert_ne!(base, k(b"fingerprint A", 4096, Some("1")));
        // (a switch set to nothing reads as unset in the translator, so it keys alike)
        assert_eq!(k(b"x", 1, None), k(b"x", 1, Some("")));
    }

    #[test]
    fn the_translator_of_this_build_is_named_by_a_fingerprint_not_the_executable() {
        // 64 hex digits of the BLAKE3 of the translator's sources (victory-shader's build.rs)
        assert_eq!(victory_shader::SOURCE_HASH.len(), 64);
        assert!(victory_shader::SOURCE_HASH.bytes().all(|b| b.is_ascii_hexdigit()));
        // and, unless the executable is asked for, the key is made of it
        if std::env::var_os("VICTORY_SHADER_CACHE_KEY").is_none() {
            let actual = build(); // Probes the host heap before reading its settings.
            let expected = with_host_settings(key(
                &[&CACHE_SCHEMA_VERSION.to_le_bytes()[..], FINAL_SOURCE_HASH.as_bytes()].concat(),
                victory_shader::emit::loop_limit(),
                victory_shader::emit::group_sync(),
                &all_switches(),
                victory_shader::emit::shader_opt(),
            ), &HostFacts::now());
            assert_eq!(actual, &expected);
        }
    }

    fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            if e.path().is_dir() {
                files.extend(walk(&e.path()));
            } else {
                files.push(e.path());
            }
        }
        files
    }

    #[test]
    fn a_program_kept_out_of_the_deep_skips_form_has_a_key_of_its_own() {
        let build = [7u8; 32];
        assert_eq!(program_key(&build, false), build, "allowed: the build's key, as without lists");
        let off = program_key(&build, true);
        assert_ne!(off, build);
        assert_eq!(program_key(&build, true), off, "the same decision, the same key");
        assert_ne!(program_key(&[8; 32], true), off, "and still the build's");

        // So only the program whose decision changed is translated again.
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        let [a, b] = [b"program a".as_slice(), b"program b"];
        for c in [a, b] {
            cached(&cache, &program_key(&build, false), c, translate).unwrap();
        }
        assert_eq!(runs.load(Ordering::Relaxed), 2);
        // a listed: a translated again, b from the cache
        cached(&cache, &program_key(&build, true), a, translate).unwrap();
        cached(&cache, &program_key(&build, false), b, translate).unwrap();
        assert_eq!(runs.load(Ordering::Relaxed), 3);
        // the list emptied again: both from the cache
        for c in [a, b] {
            cached(&cache, &program_key(&build, false), c, translate).unwrap();
        }
        assert_eq!(runs.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn entries_round_trip_and_reject_damage() {
        let bytes = encode(&shader());
        assert_eq!(decode(&bytes), Some(shader()));
        assert_eq!(decode(&bytes[..bytes.len() - 1]), None);
        assert_eq!(decode(&[bytes.as_slice(), &[0]].concat()), None);
        assert_eq!(decode(b"VSH0"), None);
    }
}
