//! First-boot prewarm (`victory prewarm`): what a warm cache's runs translated and compiled,
//! named by hashes only, made again from the user's own game files before the first run.
//!
//! The manifest (`victory prewarm manifest`) lists
//! - the title's shader containers by the 16 hex digits the logs name them by (the BLAKE3 of
//!   the container), with their stage;
//! - the title pipeline keys the warm cache's runs asked for (its early list,
//!   `early-pipelines.txt`), each marked by how it can be made offline: `c` a compute
//!   pipeline (its one shader is enough), `g` a graphics record of the title's own pipeline
//!   cache (in its `shaders.rpf`), `r` only the run knows its state.
//!
//! Hashes only: no code, no shader bytes, no driver blobs, so it can ship with the source.
//! `victory prewarm run` then translates the listed containers into the shader cache, makes
//! the `c` and `g` pipelines into the pipeline library, and puts every listed key on the early
//! list (an `r` pipeline then starts compiling when the title creates it, during loading,
//! instead of at its first draw). With `--check` it changes nothing and says how much of the
//! manifest the caches hold: what the next boot would find.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use super::{Shader, library, make, precompiled};

/// Creates the host GPU and probes what the translation depends on (its widest wave, whether
/// it caps view heaps), as a run does before its first translation, so the cache keys are the
/// run's. Whether there is a host GPU.
pub fn prepare_host() -> bool {
    let ok = crate::host::host().is_some();
    crate::descriptor::host::probe_view_limit();
    ok
}

/// The calling thread at the lowest priority, in background mode (lowered I/O and memory
/// priority too), so the work can go on beside a game.
pub fn background() {
    precompiled::background();
}

fn shader(stage: &'static str, container: &[u8]) -> Arc<Shader> {
    // Not through the table, which keeps every container it is given.
    Arc::new(Shader {
        stage,
        hash: blake3::hash(container).to_hex()[..16].to_owned(),
        container: container.to_vec(),
        translated: OnceLock::new(),
        walks: OnceLock::new(),
        thread_limited: false,
        limited_twin: OnceLock::new(),
        group: OnceLock::new(),
        bakes: OnceLock::new(),
    })
}

/// Looks the shader cache up with fx-cand's key (only its eight named switches), to read a
/// cache that build warmed. Before any lookup.
pub fn cand_key() {
    crate::shader_cache::key_by_first_switches();
}

/// Whether the translation of `container` is in the shader cache (nothing is translated).
pub fn shader_cached(container: &[u8]) -> bool {
    crate::shader_cache::is_cached(container)
}

/// Whether a run translated `container` into the shader cache: at this build's key, or at the
/// key of a run whose first lookup came before the view heap probe (no view limit, no heap
/// map). Such runs keyed so whenever an early pipeline was looked up before any translation,
/// which is where their compute shaders are: what the manifest lists (`run` then translates
/// at this build's key, which probes first).
pub fn shader_cached_by_runs(container: &[u8]) -> bool {
    crate::shader_cache::is_cached_by_runs(container)
}

/// The title pipeline key of the compute pipeline of `container` (a compute shader).
pub fn compute_key(container: &[u8]) -> String {
    make::Pending::new(vec![shader("compute", container)], None).key().to_owned()
}

/// What [`warm`] did with one container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Warmed {
    /// It translates (now or from the cache).
    pub translated: bool,
    /// It was translated now (not in the cache before).
    pub new: bool,
    /// Its compute pipeline, when asked for: made or loaded (`Some(true)`), refused, or
    /// `None` (not asked for, left out, or does not translate).
    pub pipeline: Option<bool>,
}

/// Translates `container` into the shader cache unless it is there, and with `compute` makes
/// its compute pipeline into the library unless it loads from there.
pub fn warm(stage: &'static str, container: &[u8], compute: bool) -> Warmed {
    let new = !shader_cached(container);
    let s = shader(stage, container);
    let translated = s.translated().is_some();
    let pipeline = (translated && compute)
        .then(|| make::warm_pending(&make::Pending::new(vec![s], None)))
        .flatten();
    Warmed { translated, new, pipeline }
}

/// The title pipeline keys of the graphics records of the title's pipeline cache (in its
/// shader archive). Needs no host GPU.
pub fn graphics_keys(archive: &std::path::Path, index: Option<&std::path::Path>, jobs: usize) -> Result<HashSet<String>, String> {
    precompiled::precompile(archive, index, jobs, None, 0, false, precompiled::Pick::KeysOnly, &|_, _| {}).map(|(_, k)| k)
}

/// Makes the host pipelines of the graphics records whose key is in `wanted` (loaded from the
/// library when they are there): see [`super::precompile_graphics`].
pub fn graphics(
    archive: &std::path::Path,
    index: Option<&std::path::Path>,
    jobs: usize,
    wanted: &HashSet<String>,
    progress: &(dyn Fn(usize, &super::PrecompileReport) + Sync),
) -> Result<super::PrecompileReport, String> {
    precompiled::precompile(archive, index, jobs, None, 0, false, precompiled::Pick::Listed(wanted), progress).map(|(r, _)| r)
}

/// Pipelines only loaded from the library from now on, never made (`victory prewarm run
/// --check`): a pipeline the library does not hold counts as refused.
pub fn load_only() {
    library::set_load_only(true);
}

/// Whether `key` is on the early list (the pipelines a run starts at their creation).
pub fn early_listed(key: &str) -> bool {
    library::wanted_early(key)
}

/// Puts `keys` on the early list (written with the library by [`save`]).
pub fn list_early<'a>(keys: impl IntoIterator<Item = &'a str>) {
    for key in keys {
        library::note_used(key);
    }
}

/// Saves the pipeline library and the early list: `now`, or if 20 s passed since the last save
/// (so a prewarm that is stopped keeps most of what it made).
pub fn save(now: bool) {
    library::save(now);
}

/// Host pipelines made (compiled by the driver) and loaded from the library since the start.
pub fn pipeline_counts() -> (u64, u64) {
    super::pipeline_counts()
}
