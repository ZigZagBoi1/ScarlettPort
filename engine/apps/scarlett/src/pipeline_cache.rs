//! `scarlett pipeline-cache`: the title's precompiled pipeline cache
//! (`v2.d3d12PipelineCache`, the last file of `shaders.rpf`) decoded
//! offline, each record named the way the runtime names the pipeline the
//! title creates from a stream instead (state text and shader hashes, and
//! the title pipeline key made of them), optionally set beside the
//! pipelines a run wrote down (`known-pipelines.bin`). It makes a D3D12
//! device (no pipelines, but with --fill) to ask what formats it renders
//! to, as the runtime does: keys are per adapter. The files written hold
//! the title's state and shader hashes: they must stay outside the
//! repository.

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
    /// The decoded state, for seeding.
    desc: Option<pso_state::GraphicsDesc>,
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

    /// Its shaders as a set (stage and hash, sorted).
    fn set(&self) -> Vec<(&'static str, [u8; 32])> {
        let mut s = self.shaders.clone();
        s.sort();
        s
    }

    fn key(&self) -> String {
        pre::title_key(&self.shaders, self.state.as_ref().map(|s| s.key.as_str()))
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
        desc: None,
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
                r.desc = Some(d);
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
        desc: None,
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
    pub book: Option<PathBuf>,
    /// Write the records decoded whole into the book of the cache's
    /// `pipelines` folder, for `scarlett warm` / `prepare` to make.
    pub seed: bool,
    /// Then make their host pipelines into the pipeline library runs load
    /// from (as `scarlett prepare` does for the pipelines runs drew with).
    pub fill: bool,
    pub jobs: Option<usize>,
}

/// The containers of the records decoded whole, by BLAKE3.
fn containers_of(archive: &[u8], records: &[Record]) -> HashMap<[u8; 32], Vec<u8>> {
    let wanted: HashSet<[u8; 32]> = records
        .iter()
        .filter(|r| r.whole())
        .flat_map(|r| r.shaders.iter().map(|s| s.1))
        .collect();
    let all: Vec<sgd5::Record<'_>> = sgd5::records(archive).collect();
    let next = AtomicUsize::new(0);
    let found = Mutex::new(HashMap::new());
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                while let Some(r) = all.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let Some(bytes) = r.unpack() else { continue };
                    let hash = *blake3::hash(&bytes).as_bytes();
                    if wanted.contains(&hash) {
                        lock(&found).insert(hash, bytes);
                    }
                }
            });
        }
    });
    found.into_inner().unwrap_or_else(|e| e.into_inner())
}

/// Writes the records decoded whole into the book in `dir`.
fn seed(archive: &[u8], records: &[Record], dir: &Path) -> Result<pre::Seeded> {
    let began = std::time::Instant::now();
    let bytes = containers_of(archive, records);
    let seeds: Vec<pre::Seed<'_>> = records
        .iter()
        .filter(|r| r.whole())
        .filter_map(|r| {
            let shaders = r
                .shaders
                .iter()
                .map(|(s, h)| Some((*s, bytes.get(h)?.as_slice())))
                .collect::<Option<Vec<_>>>()?;
            Some(pre::Seed {
                shaders,
                state: r.desc.as_ref(),
            })
        })
        .collect();
    scarlett_core::ensure_outside_repo(dir)?;
    let seeded =
        pre::seed(dir, &seeds).with_context(|| format!("writing the book in {}", dir.display()))?;
    println!(
        "seed: {} records decoded whole, {} written into the book in {} ({} with a key the book had already or an earlier record gave), {} containers added, {:.1} s",
        seeds.len(),
        seeded.added,
        dir.display(),
        seeded.known,
        seeded.shaders,
        began.elapsed().as_secs_f64()
    );
    Ok(seeded)
}

/// Makes the host pipelines of the book entries with `keys` into the
/// pipeline library in `dir`, as `scarlett prepare` does for the ones runs
/// drew with.
fn fill(dir: &Path, keys: &HashSet<String>, jobs: Option<usize>) {
    let began = std::time::Instant::now();
    scarlett_hle::known_pipelines::use_translations(dir.with_file_name("shaders"));
    let all = scarlett_hle::known_pipelines::keys(dir);
    let which: Vec<usize> = (0..all.len()).filter(|&n| keys.contains(&all[n])).collect();
    let jobs = jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()))
        .max(1);
    let total = which.len();
    println!("fill: {total} pipelines into the library, {jobs} threads");
    let (count, failed) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let times = Mutex::new(Vec::with_capacity(total));
    scarlett_hle::known_pipelines::fill_library(dir, &which, jobs, |_, ms, made| {
        failed.fetch_add(usize::from(!made), Ordering::Relaxed);
        lock(&times).push(ms);
        let n = count.fetch_add(1, Ordering::Relaxed) + 1;
        if n % 1000 == 0 || n == total {
            println!(
                "fill: {n} of {total}, {:.0} s",
                began.elapsed().as_secs_f64()
            );
        }
    });
    let mut times = times.into_inner().unwrap_or_else(|e| e.into_inner());
    times.sort_by(f64::total_cmp);
    let at = |q: f64| {
        times
            .get((times.len() as f64 * q) as usize)
            .copied()
            .unwrap_or(0.0)
    };
    let size = ["host-pipelines.next", "host-pipelines.bin"]
        .iter()
        .find_map(|f| std::fs::metadata(dir.join(f)).ok())
        .map_or(0, |m| m.len());
    println!(
        "fill: {} made, {} without a host pipeline, {:.0} s; ms each: median {:.1}, p90 {:.1}, p99 {:.1}, most {:.0}; library {} MB",
        count.load(Ordering::Relaxed),
        failed.load(Ordering::Relaxed),
        began.elapsed().as_secs_f64(),
        at(0.5),
        at(0.9),
        at(0.99),
        times.last().copied().unwrap_or(0.0),
        size >> 20
    );
}

pub fn run(game: PathBuf, o: Options) -> Result<()> {
    let Options {
        out,
        book,
        seed: seeding,
        fill: filling,
        jobs,
    } = o;
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

    if let Some(book) = book {
        compare(&book, &records, &out)?;
    }
    if seeding || filling {
        let dir = cfg.cache_subdir("pipelines")?;
        let seeded = seed(&archive, &records, &dir)?;
        if filling {
            fill(&dir, &seeded.keys.into_iter().collect(), jobs);
        }
    }
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

/// The book's pipelines against the records.
fn compare(book: &Path, records: &[Record], out: &Path) -> Result<()> {
    let dir = if book.is_dir() {
        book.to_owned()
    } else {
        book.parent().map(Path::to_owned).unwrap_or_default()
    };
    let pipelines = pre::book(&dir);
    anyhow::ensure!(!pipelines.is_empty(), "no pipelines in {}", dir.display());
    let mut by_set: HashMap<Vec<(&'static str, [u8; 32])>, Vec<usize>> = HashMap::new();
    let mut record_shaders: HashSet<[u8; 32]> = HashSet::new();
    for (i, r) in records.iter().enumerate() {
        record_shaders.extend(r.shaders.iter().map(|s| s.1));
        if r.whole() {
            by_set.entry(r.set()).or_default().push(i);
        }
    }
    let mut counts = BTreeMap::<String, usize>::new();
    let mut fields = BTreeMap::<String, usize>::new();
    let mut patterns = BTreeMap::<String, usize>::new();
    let mut orders = BTreeMap::<String, usize>::new();
    let mut matched_records = HashSet::new();
    let mut other_fields = BTreeMap::<String, usize>::new();
    let mut other_patterns = BTreeMap::<String, usize>::new();
    let mut same_records = HashSet::new();
    // The pipelines runs drew with (`early-pipelines.txt` beside the book).
    let used: HashSet<String> = std::fs::read_to_string(dir.join("early-pipelines.txt"))
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
        .collect();
    let in_book: HashSet<&str> = pipelines.iter().map(|p| p.key.as_str()).collect();
    let used_missing = used
        .iter()
        .filter(|k| !in_book.contains(k.as_str()))
        .count();
    let mut detail = create(&out.with_extension("book.txt"))?;
    writeln!(detail, "# book-key status record differing-fields")?;
    let mut count = |what: &str| *counts.entry(what.to_owned()).or_default() += 1;
    for p in &pipelines {
        if p.torn {
            count("book entries that do not read whole");
            continue;
        }
        let describe = p.state.as_ref().map(|s| s.describe.as_str());
        let key_text = p.state.as_ref().map(|s| s.key.as_str());
        if pre::title_key(&p.shaders, key_text) == p.key {
            count("book keys the key function gives again (self-check)");
        }
        let stages: Vec<&str> = p.shaders.iter().map(|s| s.0).collect();
        *orders.entry(stages.join(",")).or_default() += 1;
        let kind = match (p.state.is_some(), stages.as_slice()) {
            (false, ["compute"]) => "compute",
            (false, _) => "graphics without stream state",
            (true, _) if stages.contains(&"mesh") => "graphics mesh",
            (true, _) => "graphics vertex",
        };
        count(&format!("{kind}: in the book"));
        let mut set = p.shaders.clone();
        set.sort();
        let candidates = by_set.get(&set).map(Vec::as_slice).unwrap_or_default();
        let (status, record, diff) = if candidates.is_empty() {
            let why = if p
                .shaders
                .iter()
                .any(|s| !matches!(s.0, "vertex" | "pixel" | "mesh" | "compute"))
            {
                "no record: a stage records do not have"
            } else if p.shaders.iter().any(|s| !record_shaders.contains(&s.1)) {
                "no record: a shader no record names"
            } else {
                "no record: shaders of records, not together"
            };
            (why, String::from("-"), Vec::new())
        } else if kind == "compute" || kind == "graphics without stream state" {
            matched_records.extend(candidates.iter().map(|&c| base(&records[c])));
            (
                "same shader (no state to compare)",
                records[candidates[0]].name.clone(),
                Vec::new(),
            )
        } else {
            let theirs = p.state.as_ref().expect("graphics");
            let state = |c: usize| {
                records[c]
                    .state
                    .as_ref()
                    .expect("whole graphics records have a state")
            };
            let value = |s: &pre::StateText, f: &str| {
                s.fields
                    .iter()
                    .find(|(k, _)| k == f)
                    .map_or("-".to_owned(), |(_, v)| v.replace(' ', ""))
            };
            let tally = |into: &mut BTreeMap<String, usize>,
                         sets: &mut BTreeMap<String, usize>,
                         c: usize| {
                let d = pre::differing(state(c), theirs);
                let mut pattern: Vec<String> = d.iter().map(|f| normalise(f)).collect();
                pattern.sort();
                pattern.dedup();
                for f in &pattern {
                    *into.entry(f.clone()).or_default() += 1;
                }
                *sets.entry(pattern.join(" ")).or_default() += 1;
                d.iter()
                    .map(|f| format!("{f}={}/{}", value(state(c), f), value(theirs, f)))
                    .collect::<Vec<_>>()
            };
            if let Some(&c) = candidates.iter().find(|&&c| state(c).key == theirs.key) {
                matched_records.insert(base(&records[c]));
                if records[c].key() == p.key {
                    count(&format!("{kind}: identical, record key equals book key"));
                }
                // The library names both by the key text: those whose
                // describe() differs were named apart before it did.
                if state(c).describe != theirs.describe {
                    let early = if used.contains(&p.key) {
                        "early list, "
                    } else {
                        ""
                    };
                    count(&format!("{early}{kind}: identical, other describe()"));
                    if state(c).made == theirs.made {
                        count(&format!(
                            "{early}{kind}: identical, other describe(), same host blend"
                        ));
                    }
                }
                ("identical", records[c].name.clone(), Vec::new())
            } else if let Some(&c) = candidates
                .iter()
                .find(|&&c| state(c).semantic == theirs.semantic)
            {
                same_records.insert(base(&records[c]));
                let shown = tally(&mut fields, &mut patterns, c);
                (
                    "same pipeline, other key text",
                    records[c].name.clone(),
                    shown,
                )
            } else {
                let c = *candidates
                    .iter()
                    .min_by_key(|&&c| pre::differing(state(c), theirs).len())
                    .expect("a candidate");
                let shown = tally(&mut other_fields, &mut other_patterns, c);
                (
                    "same shaders, another pipeline",
                    records[c].name.clone(),
                    shown,
                )
            }
        };
        count(&format!("{kind}: {status}"));
        if used.contains(&p.key) {
            count(&format!("early list, {kind}: {status}"));
        }
        let shaders: Vec<String> = p
            .shaders
            .iter()
            .map(|(s, h)| format!("{s}={}", hex(&h[..8])))
            .collect();
        writeln!(
            detail,
            "{} {} {record} {} {} | {}",
            p.key,
            status.replace(' ', "_"),
            if diff.is_empty() {
                "-".to_owned()
            } else {
                diff.join(",")
            },
            shaders.join(","),
            describe.unwrap_or("-")
        )?;
    }
    detail.flush()?;
    println!("book {} ({} pipelines):", dir.display(), pipelines.len());
    for (k, v) in &counts {
        println!("  {v:>7}  {k}");
    }
    println!(
        "  {:>7}  records identical to a book pipeline (in one of their choices of containers)",
        matched_records.len()
    );
    println!(
        "  {:>7}  graphics records equal to a book pipeline in what they do only",
        same_records.difference(&matched_records).count()
    );
    println!(
        "  {:>7}  pipelines in early-pipelines.txt (requested by draws or dispatches in any run on this cache, made or not), {used_missing} of them not in the book",
        used.len()
    );
    println!("stage orders in the book:");
    for (k, v) in &orders {
        println!("  {v:>7}  {k}");
    }
    let top = |m: BTreeMap<String, usize>, title: &str, n: usize| {
        println!("{title}");
        let mut v: Vec<_> = m.into_iter().collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.1));
        for (k, c) in v.into_iter().take(n) {
            println!("  {c:>7}  {k}");
        }
    };
    top(
        fields,
        "same pipeline, other key text: fields that differ (N as *):",
        30,
    );
    top(patterns, "same pipeline, other key text: field sets:", 15);
    top(
        other_fields,
        "same shaders, another pipeline: fields that differ from the nearest record:",
        20,
    );
    top(
        other_patterns,
        "same shaders, another pipeline: field sets:",
        10,
    );
    println!(
        "per book pipeline: {}",
        out.with_extension("book.txt").display()
    );
    Ok(())
}

/// A record's name without its choice of containers.
fn base(r: &Record) -> &str {
    r.name.split('v').next().unwrap_or(&r.name)
}

/// A field name with its target or element number taken out.
fn normalise(field: &str) -> String {
    let mut out = String::new();
    let mut digits = false;
    for c in field.chars() {
        if c.is_ascii_digit() {
            if !digits {
                out.push('*');
            }
            digits = true;
        } else {
            digits = false;
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_numbers_are_taken_out() {
        assert_eq!(normalise("blend.rt3.SrcBlend"), "blend.rt*.SrcBlend");
        assert_eq!(normalise("targets.RTFormats12"), "targets.RTFormats*");
        assert_eq!(normalise("sample_mask"), "sample_mask");
    }
}
