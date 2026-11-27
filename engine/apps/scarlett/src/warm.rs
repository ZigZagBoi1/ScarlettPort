//! `scarlett warm`: host pipelines made ahead of a run (`known_pipelines`), in
//! processes of their own: a compiler that crashes or hangs costs one pipeline.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use scarlett_hle::known_pipelines as known;

/// Pipelines a process makes before another takes its place: the compiler's
/// and the translations' memory goes with it.
const AT_ONCE: usize = 40;
/// A pipeline that takes longer is taken for one the compiler hangs on.
const LONGEST: Duration = Duration::from_secs(180);

/// As the runs count one that takes long.
const SLOW_MS: f64 = 1000.0;

fn folder() -> Result<PathBuf> {
    let cfg = scarlett_core::Config::load()?;
    Ok(cfg.cache_subdir("shaders")?.with_file_name("pipelines"))
}

fn select_pipelines(
    keys: &[String],
    done: &HashSet<&str>,
    bad: &HashSet<String>,
    early: &HashSet<&str>,
    again: bool,
    used: bool,
    limit: Option<usize>,
) -> Vec<usize> {
    // Those earlier runs drew with first, the others in no order (by key).
    let mut all: Vec<usize> = (0..keys.len())
        .filter(|&n| {
            let key = keys[n].as_str();
            (again || !done.contains(key))
                && (again || !bad.contains(key))
                && (!used || early.contains(key))
        })
        .collect();
    all.sort_by_key(|&n| (!early.contains(keys[n].as_str()), &keys[n]));
    all.truncate(limit.unwrap_or(usize::MAX));
    all
}

pub fn run(jobs: Option<usize>, limit: Option<usize>, again: bool, used: bool, these: Option<String>) -> Result<()> {
    let dir = folder()?;
    if let Some(these) = these {
        return make(&dir, &these);
    }
    let began = Instant::now();
    scarlett_hle::known_pipelines::use_translations(dir.with_file_name("shaders"));
    let fingerprint = known::fingerprint();
    let done_file = dir.join("warm-done.txt");
    let bad_file = dir.join(known::WARM_BAD_FILE);
    let keys = known::keys(&dir);
    let listed = std::fs::read_to_string(&done_file).unwrap_or_default();
    let mut done: HashSet<&str> = HashSet::new();
    for line in listed.lines() {
        if let Some((of, key)) = line.split_once(' ')
            && of == fingerprint
        {
            done.insert(key);
        }
    }
    let mut bad: HashSet<String> = if known::bad_list_disabled() {
        HashSet::new()
    } else {
        let text = std::fs::read_to_string(&bad_file).unwrap_or_default();
        known::parse_bad_keys(&text, &fingerprint)
    };
    // Nothing stays listed for good: each listed pipeline is made once more,
    // alone in a process of its own, and one that is made leaves the list.
    // A batch's crash names pipelines that are made fine alone (issue #92).
    if !bad.is_empty() && !again && retry_alone() {
        let listed: Vec<usize> = (0..keys.len()).filter(|&n| bad.contains(&keys[n])).collect();
        let mut made = HashSet::new();
        for &n in &listed {
            if let (1, None) = child(&[n], |_, _, _| {}) {
                made.insert(keys[n].clone());
            }
        }
        if !made.is_empty() {
            let text = std::fs::read_to_string(&bad_file).unwrap_or_default();
            std::fs::write(&bad_file, known::without_keys(&text, &made))?;
            bad.retain(|key| !made.contains(key));
        }
        println!(
            "{} listed in {} made again alone: {} made and taken off the list, {} still end their process",
            listed.len(),
            known::WARM_BAD_FILE,
            made.len(),
            listed.len() - made.len()
        );
    }
    let early = std::fs::read_to_string(dir.join("early-pipelines.txt")).unwrap_or_default();
    let early: HashSet<&str> = early.lines().collect();
    let all = select_pipelines(&keys, &done, &bad, &early, again, used, limit);
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let jobs = jobs.unwrap_or(cores * 2 / 3).max(1);
    println!(
        "{} pipelines known, {} made before ({fingerprint}), {} bad; {} to make, {jobs} processes",
        keys.len(),
        done.len(),
        bad.len(),
        all.len()
    );
    // (No run recorded pipelines yet: the folder is not there.)
    std::fs::create_dir_all(&dir)?;
    let chunks: Vec<&[usize]> = all.chunks(AT_ONCE).collect();
    let next = AtomicUsize::new(0);
    let state = Mutex::new((Vec::<f64>::new(), 0usize, 0usize, 0usize));
    let done_out = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(&done_file)?);
    // (Read back by the runs: these are made beside the others, not before.)
    let slow_out = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(dir.join("slow-pipelines.txt"))?);
    let bad_out = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(&bad_file)?);
    std::thread::scope(|s| {
        for _ in 0..jobs.min(chunks.len()) {
            s.spawn(|| {
                let mut noted = |n: usize, ms: f64, there: bool| {
                    if ms >= SLOW_MS {
                        let _ = writeln!(lock(&slow_out), "\n{} {ms:.0}", keys[n]);
                    }
                    let mut state = lock(&state);
                    state.0.push(ms);
                    state.1 += usize::from(!there);
                    let count = state.0.len();
                    if count % 1000 == 0 {
                        println!("{count} of {}, {:.0} s", all.len(), began.elapsed().as_secs_f64());
                    }
                    drop(state);
                    let _ = writeln!(lock(&done_out), "{fingerprint} {}", keys[n]);
                };
                while let Some(&chunk) = chunks.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let mut rest = chunk;
                    while !rest.is_empty() {
                        let (made, ended) = child(rest, &mut noted);
                        rest = &rest[made.min(rest.len())..];
                        match (ended, rest.split_first()) {
                            (Some(why), Some((&n, after))) => {
                                // The one it was at, made again in a process of
                                // its own ([`retry_alone`]): named only if that
                                // ends too.
                                let alone = retry_alone().then(|| child(&[n], &mut noted));
                                if let Some((1, None)) = alone {
                                    println!("pipeline {n} {}: {why} in a batch, made alone", keys[n]);
                                    lock(&state).3 += 1;
                                } else {
                                    println!("pipeline {n} {}: {why}", keys[n]);
                                    let _ = writeln!(lock(&bad_out), "{}", known::format_bad_entry(&keys[n], &fingerprint, &why));
                                    lock(&state).2 += 1;
                                }
                                rest = after;
                            }
                            _ => break,
                        }
                    }
                }
            });
        }
    });
    let (mut times, refused, crashed, alone) = state.into_inner().unwrap_or_else(|e| e.into_inner());
    times.sort_by(f64::total_cmp);
    let at = |q: f64| times.get((times.len() as f64 * q) as usize).copied().unwrap_or(0.0);
    println!(
        "{} made in {:.0} s ({refused} have no host pipeline, {crashed} ended their process, {alone} more ended one in a batch and were made alone); ms each: median {:.1}, p90 {:.1}, p99 {:.1}, most {:.0}, sum {:.0} s",
        times.len(),
        began.elapsed().as_secs_f64(),
        at(0.5),
        at(0.9),
        at(0.99),
        times.last().copied().unwrap_or(0.0),
        times.iter().sum::<f64>() / 1e3
    );
    Ok(())
}

/// `scarlett prepare`: everything that can be built ahead of a run, with no
/// game window, on every core. Machine-readable progress on stdout, one line
/// each: `prepare step <n>/<steps> <name>`, `prepare progress <name> <done>
/// <total>` (total 0 when unknown) and at the end `prepare done <seconds>`.
/// 1. shaders: every shader container in the game folder translated into
///    the shader cache (as `scarlett shaders`);
/// 2. library: the host pipelines of every pipeline earlier runs drew with
///    (`early-pipelines.txt`) made into the pipeline library runs load from;
/// 3. (`--all`) driver: every other pipeline the title ever created, made
///    for the driver's cache on disk (as `scarlett warm`).
pub fn prepare(jobs: Option<usize>, all: bool) -> Result<()> {
    let began = Instant::now();
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let jobs = jobs.unwrap_or(cores).max(1);
    let steps = if all { 3 } else { 2 };
    println!("prepare step 1/{steps} shaders");
    crate::translate::precompile(Vec::new(), Some(jobs))?;
    println!("prepare step 2/{steps} library");
    let dir = folder()?;
    scarlett_hle::known_pipelines::use_translations(dir.with_file_name("shaders"));
    let keys = known::keys(&dir);
    let early = std::fs::read_to_string(dir.join("early-pipelines.txt")).unwrap_or_default();
    let early: HashSet<&str> = early.lines().collect();
    let used: Vec<usize> = (0..keys.len()).filter(|&n| early.contains(keys[n].as_str())).collect();
    let total = used.len();
    if keys.is_empty() {
        println!("prepare note no pipeline records yet: runs write them with SCARLETT_PIPELINE_BOOK=1 (SCARLETT_PIPELINE_BOOK); run prepare again after one");
    }
    let count = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    println!("prepare progress library 0 {total}");
    known::fill_library(&dir, &used, jobs, |_, _, made| {
        failed.fetch_add(usize::from(!made), Ordering::Relaxed);
        let n = count.fetch_add(1, Ordering::Relaxed) + 1;
        if n % 50 == 0 || n == total {
            println!("prepare progress library {n} {total}");
        }
    });
    println!(
        "library      {total} pipelines in the library ({} have no host pipeline), {:.0} s",
        failed.load(Ordering::Relaxed),
        began.elapsed().as_secs_f64()
    );
    if all {
        println!("prepare step 3/{steps} driver");
        run(Some(jobs), None, false, false, None)?;
    }
    println!("prepare done {:.0}", began.elapsed().as_secs_f64());
    Ok(())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Has a process make `these`; how many it made, and how it ended if not well.
/// Whether a pipeline that ended a process of a batch is made once more in a
/// process of its own before it is named in `warm-bad.txt`
/// (`SCARLETT_WARM_RETRY=0`: named at once, as before).
///
/// On an RX 9060 XT (amdxc64 32.0.32029.2010) every one of the pipelines that
/// ended a batch's process in two full warms (6 of 83,577 each time, 12 keys,
/// among them the skin pipeline whose draws went black, issue #92) was made
/// alone, twice each: the compiler ends a process some way into a batch of
/// [`AT_ONCE`], not on one pipeline. Named, each stayed unmade in every run.
fn retry_alone() -> bool {
    std::env::var_os("SCARLETT_WARM_RETRY").is_none_or(|v| v != "0")
}

fn child(these: &[usize], mut each: impl FnMut(usize, f64, bool)) -> (usize, Option<String>) {
    let list: Vec<String> = these.iter().map(usize::to_string).collect();
    let spawned = std::env::current_exe().and_then(|exe| {
        use std::os::windows::process::CommandExt;
        const IDLE_PRIORITY_CLASS: u32 = 0x40;
        Command::new(exe)
            .args(["warm", "--these", &list.join(",")])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(IDLE_PRIORITY_CLASS)
            .spawn()
    });
    let Ok(mut process) = spawned else { return (these.len(), None) };
    let Some(out) = process.stdout.take() else { return (these.len(), None) };
    let process = Mutex::new(process);
    let (heard, over) = (Mutex::new(Instant::now()), AtomicBool::new(false));
    let hung = AtomicBool::new(false);
    let mut made = 0;
    std::thread::scope(|s| {
        s.spawn(|| {
            while !over.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                if lock(&heard).elapsed() > LONGEST {
                    hung.store(true, Ordering::Relaxed);
                    let _ = lock(&process).kill();
                    return;
                }
            }
        });
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            *lock(&heard) = Instant::now();
            let mut words = line.split(' ');
            if words.next() == Some("made")
                && let (Some(n), Some(ms), Some(there)) = (words.next(), words.next(), words.next())
                && let (Ok(n), Ok(ms)) = (n.parse(), ms.parse())
            {
                made += 1;
                each(n, ms, there == "1");
            }
        }
        over.store(true, Ordering::Relaxed);
    });
    let status = lock(&process).wait();
    let well = status.as_ref().is_ok_and(|s| s.success());
    if well {
        return (these.len(), None);
    }
    let why = if hung.load(Ordering::Relaxed) {
        format!("hung for {} s", LONGEST.as_secs())
    } else {
        status.map_or_else(|e| e.to_string(), |s| format!("{s}"))
    };
    (made, Some(why))
}

/// What such a process does.
fn make(dir: &Path, these: &str) -> Result<()> {
    let these: Vec<usize> = these.split(',').map(|n| n.parse().context("--these")).collect::<Result<_>>()?;
    let cfg = scarlett_core::Config::load()?;
    scarlett_hle::known_pipelines::use_translations(cfg.cache_subdir("shaders")?);
    let mut out = std::io::stdout();
    known::make(dir, &these, |n, came| {
        let told = match came {
            None => writeln!(out, "at {n}"),
            Some((ms, there)) => writeln!(out, "made {n} {ms:.2} {}", u8::from(there)),
        };
        if told.and_then(|()| out.flush()).is_err() {
            // Whoever asked is gone.
            std::process::exit(1);
        }
    });
    // Without what a run's end does (nothing to save, the host's threads).
    let _ = out.flush();
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn select_pipelines_skips_current_bad_and_retries_on_again_or_new_fingerprint() {
        let hex_key = |c: char| c.to_string().repeat(32);
        let (k_done, k_bad_current, k_bad_old, k_legacy_bad, k_fresh) = (
            hex_key('0'),
            hex_key('1'),
            hex_key('2'),
            hex_key('3'),
            hex_key('4'),
        );
        let keys = vec![
            k_done.clone(),
            k_bad_current.clone(),
            k_bad_old.clone(),
            k_legacy_bad.clone(),
            k_fresh.clone(),
        ];
        let done = HashSet::from([k_done.as_str()]);
        let early = HashSet::from([k_bad_current.as_str(), k_fresh.as_str()]);
        let current_entry = known::format_bad_entry(&k_bad_current, "1111111111111111", "exit code: 0xc0000005");
        assert_eq!(current_entry.split(' ').next(), Some(k_bad_current.as_str()));
        let old_entry = known::format_bad_entry(&k_bad_old, "0000000000000000", "hung for 180 s");
        let bad_text = format!(
            "# comment\n\
             {current_entry}\n\
             {old_entry}\n\
             {k_legacy_bad} legacy crash\n"
        );
        let bad = known::parse_bad_keys(&bad_text, "1111111111111111");
        assert_eq!(bad, HashSet::from([k_bad_current.clone(), k_legacy_bad]));

        // Early non-done/non-bad first (k_fresh = 4), then stale-fingerprint k_bad_old (2).
        assert_eq!(select_pipelines(&keys, &done, &bad, &early, false, false, None), vec![4, 2]);
        // --again retries done and bad pipelines (early indices 1, 4 first, then 0, 2, 3).
        assert_eq!(select_pipelines(&keys, &done, &bad, &early, true, false, None), vec![1, 4, 0, 2, 3]);
        assert_eq!(select_pipelines(&keys, &done, &bad, &early, true, true, None), vec![1, 4]);
        // Simulating SCARLETT_WARM_BAD=0 clears bad while still honoring done.
        assert_eq!(
            select_pipelines(&keys, &done, &HashSet::new(), &early, false, false, None),
            vec![1, 4, 2, 3]
        );
    }
}
