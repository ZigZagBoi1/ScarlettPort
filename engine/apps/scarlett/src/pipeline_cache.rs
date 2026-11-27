//! `scarlett pipeline-cache`: the title's precompiled pipeline cache
//! (`v2.d3d12PipelineCache`, the last file of `shaders.rpf`) decoded
//! offline, each record named the way the runtime names the pipeline the
//! title creates from a stream instead (state text and shader hashes, and
//! the title pipeline key made of them). It makes a D3D12 device (but no
//! pipelines) to ask what formats it renders to, as the runtime does: keys
//! are per adapter. The file written holds the title's state and shader
//! hashes: it must stay outside the repository.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use scarlett_game::{pso_cache, pso_state, sgd5};
use scarlett_hle::precompiled_pipelines as pre;

/// A record's shaders in the order the key is made from: the geometry
/// stage, then pixel (what streams give; checked against the book).
const KEY_ORDER: [&str; 3] = ["vertex", "mesh", "pixel"];

/// What came of one record.
#[derive(Clone)]
struct Record {
    /// `g<n>` or `c<n>`.
    name: String,
    kind: &'static str,
    shaders: Vec<(&'static str, [u8; 32])>,
    state: Option<pre::StateText>,
    /// Which of how many choices of containers this is (where stages name
    /// digests that more than one container of the archive has).
    variant: Option<(usize, usize)>,
    /// Why it does not decode whole.
    failed: Vec<String>,
}

impl Record {
    fn whole(&self) -> bool {
        self.failed.is_empty()
    }

    fn key(&self) -> String {
        pre::title_key(
            &self.shaders,
            self.state.as_ref().map(|s| s.describe.as_str()),
        )
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The archive's containers by the digest of their `HASH` part: their
/// BLAKE3s (more than one if containers share a digest).
fn index(
    archive: &[u8],
) -> (
    HashMap<[u8; 16], Vec<[u8; 32]>>,
    BTreeMap<&'static str, usize>,
) {
    let records: Vec<sgd5::Record<'_>> = sgd5::records(archive).collect();
    let next = AtomicUsize::new(0);
    let map = Mutex::new(HashMap::<[u8; 16], Vec<[u8; 32]>>::new());
    let tally = Mutex::new(BTreeMap::<&'static str, usize>::new());
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let mut mine = Vec::new();
                let mut counts = BTreeMap::<&'static str, usize>::new();
                while let Some(r) = records.get(next.fetch_add(1, Ordering::Relaxed)) {
                    *counts.entry("records").or_default() += 1;
                    let Some(bytes) = r.unpack() else {
                        *counts.entry("records that do not unpack").or_default() += 1;
                        continue;
                    };
                    let Some(digest) = pso_cache::container_shader_hash(&bytes) else {
                        *counts.entry("containers without a HASH part").or_default() += 1;
                        continue;
                    };
                    mine.push((digest, *blake3::hash(&bytes).as_bytes()));
                }
                let mut map = lock(&map);
                for (d, h) in mine {
                    let all = map.entry(d).or_default();
                    if !all.contains(&h) {
                        all.push(h);
                    }
                }
                let mut tally = lock(&tally);
                for (k, v) in counts {
                    *tally.entry(k).or_default() += v;
                }
            });
        }
    });
    let map = map.into_inner().unwrap_or_else(|e| e.into_inner());
    let mut tally = tally.into_inner().unwrap_or_else(|e| e.into_inner());
    tally.insert("distinct digests", map.len());
    tally.insert(
        "digests naming more than one container",
        map.values().filter(|v| v.len() > 1).count(),
    );
    (map, tally)
}

/// A stage's containers: their BLAKE3s (more than one where containers of
/// the archive share the digest the stage names), or why there is none.
fn stage_container(
    cache: &pso_cache::Cache<'_>,
    containers: &HashMap<[u8; 16], Vec<[u8; 32]>>,
    code: u32,
    meta: u32,
) -> Result<Vec<[u8; 32]>, &'static str> {
    let code = cache.code(code).ok_or("stage code outside the file")?;
    let meta = cache.block(meta).unwrap_or_default();
    let digest = pso_cache::stage_shader_hash(code, meta).ok_or("stage names no shader")?;
    match containers.get(&digest) {
        Some(all) if !all.is_empty() => {
            let mut all = all.clone();
            all.sort();
            Ok(all)
        }
        _ => Err("stage's shader not in the archive"),
    }
}

/// At most this many variants of a record (one per choice of container).
const MAX_VARIANTS: usize = 64;

/// A record once per choice of its stages' containers.
fn variants(mut r: Record, stages: Vec<(&'static str, Vec<[u8; 32]>)>) -> Vec<Record> {
    let n: usize = stages.iter().map(|s| s.1.len()).product();
    if n > MAX_VARIANTS {
        r.failed
            .push(format!("more than {MAX_VARIANTS} choices of containers"));
    }
    if !r.failed.is_empty() || n <= 1 {
        r.shaders = stages
            .iter()
            .filter_map(|(s, h)| Some((*s, *h.first()?)))
            .collect();
        return vec![r];
    }
    (0..n)
        .map(|mut v| {
            let mut one = Record {
                variant: Some((v, n)),
                ..r.clone()
            };
            one.name = format!("{}v{v}", r.name);
            one.shaders = stages
                .iter()
                .map(|(s, h)| {
                    let pick = h[v % h.len()];
                    v /= h.len();
                    (*s, pick)
                })
                .collect();
            one
        })
        .collect()
}

fn graphics(
    cache: &pso_cache::Cache<'_>,
    containers: &HashMap<[u8; 16], Vec<[u8; 32]>>,
    i: usize,
) -> Vec<Record> {
    let mut r = Record {
        name: format!("g{i}"),
        kind: "graphics ?",
        shaders: Vec::new(),
        state: None,
        failed: Vec::new(),
        variant: None,
    };
    let Some(g) = cache.graphics(i) else {
        r.failed.push("record outside the file".into());
        return vec![r];
    };
    r.kind = match (g.stages[1].is_some(), g.stages[2].is_some()) {
        (true, false) => "graphics vertex",
        (false, true) => "graphics mesh",
        (true, true) => "graphics vertex+mesh",
        (false, false) => "graphics without vertex or mesh",
    };
    let mut stages = Vec::new();
    for (slot, stage) in g.stages.iter().enumerate() {
        let Some((code, meta)) = *stage else { continue };
        let Some(name) = pso_cache::slot_stage(slot) else {
            r.failed.push(format!("stage slot {slot} of unknown kind"));
            continue;
        };
        match stage_container(cache, containers, code, meta) {
            Ok(h) => stages.push((name, h)),
            Err(why) => r.failed.push(format!("{name}: {why}")),
        }
    }
    stages.sort_by_key(|(s, _)| KEY_ORDER.iter().position(|k| k == s));
    match (cache.block(g.state[0]), cache.block(g.state[1])) {
        (Some(a), Some(b)) => match pso_state::decode(a, b) {
            Ok(d) => {
                if !pso_state::consistent(a, &d) {
                    r.failed
                        .push("state: formats disagree with SX_PS_DOWNCONVERT".into());
                }
                r.state = Some(pre::state_text(&d));
            }
            Err(e) => r.failed.push(format!("state: {}", reason(&e))),
        },
        _ => r.failed.push("state block outside the file".into()),
    }
    variants(r, stages)
}

/// An error without the values that vary from record to record, but for
/// the unconfirmed ones (few, and worth seeing).
fn reason(e: &pso_state::Error) -> String {
    match e {
        pso_state::Error::Value(what, _) => format!("{what} has no D3D12 value"),
        other => other.to_string(),
    }
}

fn compute(
    cache: &pso_cache::Cache<'_>,
    containers: &HashMap<[u8; 16], Vec<[u8; 32]>>,
    i: usize,
) -> Vec<Record> {
    let mut r = Record {
        name: format!("c{i}"),
        kind: "compute",
        shaders: Vec::new(),
        state: None,
        failed: Vec::new(),
        variant: None,
    };
    let Some(c) = cache.compute(i) else {
        r.failed.push("record outside the file".into());
        return vec![r];
    };
    match stage_container(cache, containers, c.code, c.meta) {
        Ok(h) => variants(r, vec![("compute", h)]),
        Err(why) => {
            r.failed.push(format!("compute: {why}"));
            vec![r]
        }
    }
}

/// The shader archive: `path` itself, or `shaders.rpf` in it.
fn archive_path(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.join("shaders.rpf")
    } else {
        path.to_owned()
    }
}

/// What `pipeline-cache` does besides reporting.
#[derive(Default)]
pub struct Options {
    pub out: Option<PathBuf>,
}

pub fn run(game: PathBuf, o: Options) -> Result<()> {
    let Options { out } = o;
    let began = std::time::Instant::now();
    let path = archive_path(&game);
    let archive = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let file = pso_cache::find_in_archive(&archive)
        .context("no v2.d3d12PipelineCache (zlib chain with its magic) in the archive")?;
    let cache = pso_cache::Cache::parse(&file).map_err(|e| anyhow::anyhow!(e))?;
    println!(
        "pipeline cache: {} bytes, build {:?}, driver {:?}, {} graphics and {} compute records ({:.1} s)",
        file.len(),
        cache.build,
        cache.driver,
        cache.graphics_count(),
        cache.compute_count(),
        began.elapsed().as_secs_f64()
    );
    let (containers, tally) = index(&archive);
    println!("shader archive ({:.1} s):", began.elapsed().as_secs_f64());
    for (k, v) in &tally {
        println!("  {v:>7}  {k}");
    }

    let mut records: Vec<Record> = (0..cache.graphics_count())
        .flat_map(|i| graphics(&cache, &containers, i))
        .collect();
    records.extend((0..cache.compute_count()).flat_map(|i| compute(&cache, &containers, i)));

    let mut kinds = BTreeMap::<&str, [usize; 2]>::new();
    let mut failures = BTreeMap::<String, usize>::new();
    let mut slots = BTreeMap::<String, usize>::new();
    let mut ambiguous = BTreeMap::<&str, [usize; 2]>::new();
    for r in &records {
        if let Some((v, _)) = r.variant {
            let a = ambiguous.entry(r.kind).or_default();
            a[1] += 1;
            if v > 0 {
                continue;
            }
            a[0] += 1;
        }
        let k = kinds.entry(r.kind).or_default();
        k[0] += 1;
        k[1] += usize::from(r.whole());
        for f in &r.failed {
            *failures.entry(f.clone()).or_default() += 1;
        }
        let stages: Vec<&str> = r.shaders.iter().map(|s| s.0).collect();
        *slots.entry(stages.join("+")).or_default() += 1;
    }
    println!("records by kind (all, decoded whole):");
    for (k, [all, whole]) in &kinds {
        println!("  {all:>7} {whole:>7}  {k}");
    }
    println!(
        "records whose stages name a digest more than one container has (records, lines written, one per choice):"
    );
    for (k, [n, lines]) in &ambiguous {
        println!("  {n:>7} {lines:>7}  {k}");
    }
    println!("failures by reason (a record can have more than one):");
    let mut failures: Vec<_> = failures.into_iter().collect();
    failures.sort_by_key(|f| std::cmp::Reverse(f.1));
    for (why, n) in &failures {
        println!("  {n:>7}  {why}");
    }
    println!("stages named (of shaders found):");
    for (s, n) in &slots {
        println!("  {n:>7}  {}", if s.is_empty() { "(none)" } else { s });
    }
    let distinct: HashSet<String> = records
        .iter()
        .filter(|r| r.whole())
        .map(Record::key)
        .collect();
    // (Choices of containers count each.)
    println!(
        "  {:>7}  distinct title keys among records decoded whole",
        distinct.len()
    );

    let cfg = scarlett_core::Config::load()?;
    let out = match out {
        Some(o) => o,
        None => cfg.cache_subdir("pipelines")?.join("precompiled-keys.txt"),
    };
    write_keys(&out, &records)?;
    println!("keys written to {}", out.display());
    Ok(())
}

fn create(path: &Path) -> Result<std::io::BufWriter<std::fs::File>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    scarlett_core::ensure_outside_repo(path)?;
    Ok(std::io::BufWriter::new(
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?,
    ))
}

/// One line per record: name, kind, `ok` or the failures, the title key,
/// the shaders (stage=BLAKE3), then after ` | ` the state text exactly as
/// `describe()` gives it.
fn write_keys(path: &Path, records: &[Record]) -> Result<()> {
    let mut f = create(path)?;
    writeln!(f, "# record kind status key shaders | describe()")?;
    for r in records {
        let status = if r.whole() {
            "ok".to_owned()
        } else {
            r.failed.join("; ").replace(' ', "_")
        };
        let shaders: Vec<String> = r
            .shaders
            .iter()
            .map(|(s, h)| format!("{s}={}", hex(h)))
            .collect();
        let describe = r.state.as_ref().map_or("-", |s| s.describe.as_str());
        writeln!(
            f,
            "{} {} {status} key={} {} | {describe}",
            r.name,
            r.kind.replace(' ', "-"),
            r.key(),
            shaders.join(" ")
        )?;
    }
    f.flush()?;
    Ok(())
}
