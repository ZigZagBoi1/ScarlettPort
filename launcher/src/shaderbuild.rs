//! The shader build, run on its own thread and reported phase by phase.
//!
//! World engine: `scarlett warm` translates every shader into the cache and makes the compute
//! pipelines (about 3 min); when the runtime uses them (`engine::PRECOMPILED_GRAPHICS`),
//! `scarlett precompile-graphics` then makes the title's graphics pipelines in chunks of
//! `engine::CHUNK` records, each chunk saved when it ends. Runtimes since 0.4.6 have other
//! commands (found from their `--help`): `scarlett shaders <game>` translates, then
//! `scarlett warm` makes the pipelines earlier runs recorded.
//! Classic engine: `xcl shaders <pack> --precompile --pipelines --only cs`, which already skips
//! what is in its cache.
//!
//! Resumable: finished phases and chunks are written to a state file in the cache, keyed by the
//! engine's hash, and are not run again. With "skip slow shaders", a step that prints nothing
//! for `slow_secs` is stopped and its shaders are left to compile on demand in game.
use crate::{engine, flow, LogKind, Message};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What the UI shows while the build runs.
#[derive(Clone, Debug)]
pub struct Progress {
    pub phase: u8,
    pub phases: u8,
    pub name: &'static str,
    pub done: u64,
    pub total: u64,
    /// What is being worked on now, and since when.
    pub current: String,
    pub current_since: Instant,
    pub last_output: Instant,
    /// 0..1 over all phases.
    pub overall: f32,
    /// Shaders or records left to compile on demand in game.
    pub on_demand: u64,
    pub started: Instant,
    pub jobs: usize,
}

#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
pub struct State {
    pub hash: String,
    /// World: `scarlett warm` finished.
    #[serde(default)]
    pub warm: bool,
    /// World: the first graphics record not done yet.
    #[serde(default)]
    pub next: u64,
    /// World: graphics records in the title's cache, once a chunk came back short.
    #[serde(default)]
    pub total: Option<u64>,
    /// World: chunks (first record) stopped as slow; Classic: one entry, the count left.
    #[serde(default)]
    pub on_demand: Vec<u64>,
}

impl State {
    pub fn load(path: &Path, hash: &str) -> State {
        let s: State = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if s.hash == hash {
            s
        } else {
            State { hash: hash.to_string(), ..Default::default() }
        }
    }

    fn save(&self, path: &Path) {
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(j) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(path, j);
        }
    }
}

/// The state file's key for a World build: the engine hash and the preset's cache folder, so a
/// build for one preset's cache never counts for another's.
pub fn state_key(hash: &str, cache: &Path) -> String {
    format!("{hash} {}", cache.display())
}

pub fn state_path(cache_dir: &str, world: bool) -> PathBuf {
    Path::new(cache_dir.trim()).join(if world { "build-world.json" } else { "build-classic.json" })
}

pub struct Job {
    pub world: bool,
    pub exe: PathBuf,
    pub runtime: PathBuf,
    pub game: PathBuf,
    pub cache_dir: String,
    pub hash: String,
    /// The World engine's environment: the selected preset's, so translated shaders land in the
    /// cache the play runs use, under the same switches.
    pub env: Vec<(&'static str, String)>,
    /// Some(N): stop a step that prints nothing for N seconds; its shaders compile in game.
    pub slow_secs: Option<u64>,
    pub stop: Arc<AtomicBool>,
    /// Pause: the running engine process is suspended while set, and resumed when cleared.
    pub pause: Arc<AtomicBool>,
    pub tx: Sender<Message>,
}

enum Outcome {
    Exit(Option<i32>),
    Stopped,
    Idle,
}

pub fn cores() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// Free physical memory in MB.
#[cfg(windows)]
pub fn free_mb() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a correctly sized, zeroed MEMORYSTATUSEX.
    (unsafe { GlobalMemoryStatusEx(&mut m) } != 0).then_some(m.ullAvailPhys / (1024 * 1024))
}

#[cfg(not(windows))]
pub fn free_mb() -> Option<u64> {
    None
}

fn log(tx: &Sender<Message>, s: impl Into<String>) {
    let _ = tx.send(Message::Log(s.into(), LogKind::Info));
}

/// Runs one engine process, streaming its lines to the log and to `on_line`.
fn run(mut cmd: Command, job: &Job, idle: Option<Duration>, mut on_line: impl FnMut(&str)) -> Result<Outcome, String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW (output is piped to the Logs page), BELOW_NORMAL_PRIORITY_CLASS
        cmd.creation_flags(0x0800_0000 | 0x0000_4000);
    }
    let mut child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", job.exe.display()))?;
    let (ltx, lrx) = channel::<String>();
    for s in [child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)]
        .into_iter()
        .flatten()
    {
        let ltx = ltx.clone();
        std::thread::spawn(move || crate::split_lines(s, |l| {
            let _ = ltx.send(l);
        }));
    }
    drop(ltx);
    let mut last = Instant::now();
    let mut exited: Option<Option<i32>> = None;
    let mut suspended = false;
    loop {
        match lrx.recv_timeout(Duration::from_millis(250)) {
            Ok(l) => {
                last = Instant::now();
                on_line(&l);
                let _ = job.tx.send(Message::Log(l, LogKind::Out));
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(code) = exited {
                    return Ok(Outcome::Exit(code));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        on_line("");
        let want = job.pause.load(Ordering::Relaxed);
        if want != suspended && exited.is_none() && crate::firstrun::set_suspended(&child, want) {
            suspended = want;
            log(&job.tx, if want { "Paused: the preparation is suspended." } else { "Resumed." });
        }
        if suspended {
            last = Instant::now();
        }
        if job.stop.load(Ordering::Relaxed) {
            if suspended {
                crate::firstrun::set_suspended(&child, false);
            }
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Outcome::Stopped);
        }
        if idle.is_some_and(|d| last.elapsed() >= d) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Outcome::Idle);
        }
        if exited.is_none() {
            if let Ok(Some(s)) = child.try_wait() {
                exited = Some(s.code());
            }
        }
    }
}

/// Runs the whole build; the result is a one-line summary or the reason it did not finish.
pub fn build(job: Job) {
    let r = if job.world { world(&job) } else { classic(&job) };
    let _ = job.tx.send(Message::BuildEnded(r));
}

fn send(job: &Job, p: &Progress) {
    let _ = job.tx.send(Message::Build(p.clone()));
}

/// Whether the runtime has the commands before 0.4.6 (`warm <game>` translating, and
/// `precompile-graphics`). The 0.4.6 runtime translates with `shaders` and its `warm` makes the
/// pipelines earlier runs recorded.
fn older_commands(job: &Job) -> bool {
    help(job).is_none_or(|h| h.contains("precompile-graphics"))
}

/// The engine's `--help` text, None when it does not run.
fn help(job: &Job) -> Option<String> {
    let mut cmd = Command::new(&job.exe);
    cmd.current_dir(&job.runtime).arg("--help").stdin(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.output().ok().map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

fn world(job: &Job) -> Result<String, String> {
    if !older_commands(job) {
        return world_split(job);
    }
    let path = state_path(&job.cache_dir, true);
    let mut st = State::load(&path, &job.hash);
    let phases = if engine::PRECOMPILED_GRAPHICS { 2 } else { 1 };
    let warm_share = if phases == 2 { 0.3 } else { 1.0 };
    let now = Instant::now();
    let mut p = Progress {
        phase: 1,
        phases,
        name: "Translating shaders",
        done: 0,
        total: engine::WARM_TOTAL,
        current: String::new(),
        current_since: now,
        last_output: now,
        overall: 0.,
        on_demand: st.on_demand.len() as u64 * engine::CHUNK,
        started: now,
        jobs: 1,
    };
    let base = |c: &mut Command| {
        c.current_dir(&job.runtime);
        for (k, v) in &job.env {
            crate::runtime::set_switch(c, k, v);
        }
    };
    if st.warm {
        log(&job.tx, "Shaders already translated for this engine build: skipping `scarlett warm`.");
    } else {
        p.jobs = engine::jobs(cores(), free_mb(), 512);
        let mut cmd = Command::new(&job.exe);
        base(&mut cmd);
        cmd.args(engine::warm_args(&job.game, p.jobs));
        log(&job.tx, format!("> scarlett warm \"{}\" --jobs {}  (phase 1 of {phases}: translate every shader, about 3 min; already translated ones are read from the cache)", job.game.display(), p.jobs));
        p.current = "Working, first progress report soon".into();
        send(job, &p);
        let mut tick = Instant::now();
        let dir = shader_dir(job);
        let mut counted = Instant::now();
        let out = run(cmd, job, None, |l| {
            if let Some(n) = engine::parse_warm(l) {
                p.done = p.done.max(n.min(p.total));
                p.current = "translating shaders".into();
                p.current_since = Instant::now();
            }
            if counted.elapsed() >= Duration::from_secs(3) {
                counted = Instant::now();
                let n = engine::count_cached_shaders(&dir).min(p.total);
                if n > p.done {
                    p.done = n;
                    p.last_output = Instant::now();
                }
            }
            if let Some((t, r)) = engine::parse_warm_end(l) {
                p.done = t + r;
                p.total = p.total.max(p.done);
            }
            if !l.is_empty() {
                p.last_output = Instant::now();
            }
            p.overall = warm_share * p.done as f32 / p.total.max(1) as f32;
            if !l.is_empty() || tick.elapsed() >= Duration::from_secs(1) {
                tick = Instant::now();
                send(job, &p);
            }
        })?;
        match out {
            Outcome::Exit(Some(0)) => {
                st.warm = true;
                st.save(&path);
            }
            Outcome::Exit(c) => return Err(format!("scarlett warm failed (exit code {c:?}); see Logs")),
            Outcome::Stopped | Outcome::Idle => return Err("Stopped. Press Build shaders again to resume.".into()),
        }
    }
    if phases == 2 {
        p.phase = 2;
        p.name = "Preparing shaders";
        p.total = st.total.unwrap_or(engine::GRAPHICS_TOTAL);
        p.jobs = engine::jobs(cores(), free_mb(), 1024);
        let idle = job.slow_secs.map(Duration::from_secs);
        while st.total.is_none_or(|t| st.next < t) {
            let skip = st.next;
            let mut cmd = Command::new(&job.exe);
            base(&mut cmd);
            cmd.args(engine::graphics_args(&job.game, p.jobs, skip, engine::CHUNK));
            log(&job.tx, format!("> scarlett precompile-graphics --jobs {} --skip {skip} --limit {}  (phase 2 of 2, prepare shaders: makes the graphics pipelines now, at low priority, so moving in the world does not stutter on first use; Stop resumes later)", p.jobs, engine::CHUNK));
            p.current = format!("pipeline records {}–{}", skip + 1, skip + engine::CHUNK);
            p.current_since = Instant::now();
            p.done = skip;
            send(job, &p);
            let mut records = None;
            let mut tick = Instant::now();
            let out = run(cmd, job, idle, |l| {
                if let Some(n) = engine::parse_graphics(l) {
                    p.done = skip + n;
                }
                if let Some(n) = engine::parse_graphics_end(l) {
                    records = Some(n);
                }
                if !l.is_empty() {
                    p.last_output = Instant::now();
                }
                p.overall = warm_share + (1. - warm_share) * p.done as f32 / p.total.max(1) as f32;
                if !l.is_empty() || tick.elapsed() >= Duration::from_secs(1) {
                    tick = Instant::now();
                    send(job, &p);
                }
            })?;
            match out {
                Outcome::Exit(Some(0)) => {
                    let n = records.unwrap_or(engine::CHUNK);
                    if n < engine::CHUNK {
                        st.total = Some(skip + n);
                        p.total = skip + n;
                    }
                    st.next = skip + n.max(1).min(engine::CHUNK);
                }
                Outcome::Idle => {
                    log(&job.tx, format!("Records {}–{} printed nothing for {} s: left to compile in game.", skip + 1, skip + engine::CHUNK, job.slow_secs.unwrap_or(0)));
                    st.on_demand.push(skip);
                    st.next = skip + engine::CHUNK;
                    p.on_demand += engine::CHUNK;
                }
                Outcome::Exit(c) => return Err(format!("scarlett precompile-graphics failed (exit code {c:?}); see Logs")),
                Outcome::Stopped => {
                    st.save(&path);
                    return Err("Stopped. Press Build shaders again to resume.".into());
                }
            }
            st.save(&path);
        }
    }
    finish(job, &mut p);
    Ok(if st.on_demand.is_empty() {
        format!("Shaders built in {}.", flow::duration(p.started.elapsed().as_secs()))
    } else {
        format!("Shaders built in {}; {} pipeline chunks compile in game.", flow::duration(p.started.elapsed().as_secs()), st.on_demand.len())
    })
}

/// The 0.4.6 runtime: `scarlett shaders <game>` translates every shader into the cache, then
/// `scarlett warm` makes the host pipelines of those earlier runs recorded. Both skip what is
/// already done, so a stop resumes; `warm` prints only every 1,000 pipelines, so the slow-step
/// stop does not apply to it.
fn world_split(job: &Job) -> Result<String, String> {
    let path = state_path(&job.cache_dir, true);
    let mut st = State::load(&path, &job.hash);
    let share = 0.8;
    let now = Instant::now();
    let mut p = Progress {
        phase: 1,
        phases: 2,
        name: "Translating shaders",
        done: 0,
        total: engine::SHADERS_TOTAL,
        current: String::new(),
        current_since: now,
        last_output: now,
        overall: 0.,
        on_demand: 0,
        started: now,
        jobs: 1,
    };
    let base = |c: &mut Command| {
        c.current_dir(&job.runtime);
        for (k, v) in &job.env {
            crate::runtime::set_switch(c, k, v);
        }
    };
    let mut untranslated = 0;
    if !st.warm {
        let dir = shader_dir(job);
        if let Some((translator, cached)) = matching_cli_shader_cache(&dir, &job.hash) {
            log(&job.tx, format!("Reusing {cached} existing cached shaders for matching translator ({translator}); skipping `scarlett shaders`."));
            st.warm = true;
            st.save(&path);
        }
    }
    if st.warm {
        log(&job.tx, "Shaders already translated for this engine build: skipping `scarlett shaders`.");
    } else {
        p.jobs = engine::jobs(cores(), free_mb(), 512);
        let mut cmd = Command::new(&job.exe);
        base(&mut cmd);
        cmd.args(engine::shaders_args(&job.game, p.jobs));
        log(&job.tx, format!("> scarlett shaders \"{}\" --jobs {}  (phase 1 of 2: translate every shader, a few minutes; already translated ones are skipped)", job.game.display(), p.jobs));
        // The engine reads the game files first and prints its first count only after that.
        p.name = "Scanning game files";
        p.current = "Working, first progress report soon".into();
        send(job, &p);
        let mut tick = Instant::now();
        let dir = shader_dir(job);
        let mut counted = Instant::now();
        let out = run(cmd, job, None, |l| {
            let reported = engine::parse_shaders(l).or_else(|| engine::parse_prepare_named(l).filter(|(n, _, _)| *n == "shaders").map(|(_, d, t)| {
                if t > 0 {
                    p.total = t;
                }
                d
            }));
            if let Some(n) = reported {
                p.name = "Translating shaders";
                p.done = p.done.max(n);
                p.total = p.total.max(p.done);
                p.current = "translating shaders".into();
                p.current_since = Instant::now();
            }
            // Between progress lines: count the translations landing in the cache folder.
            if counted.elapsed() >= Duration::from_secs(3) {
                counted = Instant::now();
                let n = engine::count_cached_shaders(&dir);
                if n > p.done {
                    p.name = "Translating shaders";
                    p.done = n.min(p.total);
                    p.current = "translating shaders".into();
                    p.last_output = Instant::now();
                }
            }
            if let Some(f) = engine::parse_shaders_failed(l) {
                untranslated = f;
            }
            if !l.is_empty() {
                p.last_output = Instant::now();
            }
            p.overall = share * p.done as f32 / p.total.max(1) as f32;
            if !l.is_empty() || tick.elapsed() >= Duration::from_secs(1) {
                tick = Instant::now();
                send(job, &p);
            }
        })?;
        match out {
            Outcome::Exit(Some(0)) if engine::too_few_shaders(p.done) => {
                return Err(format!(
                    "Preparation translated only {} shaders (about {} expected) from {}: wrong game folder, or the engine cannot read it. Nothing was marked as prepared; see Logs.",
                    p.done,
                    engine::SHADERS_TOTAL,
                    job.game.display()
                ));
            }
            Outcome::Exit(Some(0)) => {
                st.warm = true;
                st.save(&path);
            }
            Outcome::Exit(c) => return Err(format!("scarlett shaders failed (exit code {c:?}){}; see Logs", agility_note(&job.exe))),
            Outcome::Stopped | Outcome::Idle => return Err("Stopped. Press Build shaders again to resume.".into()),
        }
    }
    let shader_total = p.total;
    p.phase = 2;
    p.name = "Preparing pipelines";
    p.done = 0;
    p.total = 0;
    p.jobs = engine::jobs(cores(), free_mb(), 1024);
    p.current = "reading the pipelines earlier runs recorded".into();
    p.current_since = Instant::now();
    p.overall = share;
    // The most complete pipeline step this engine offers; `warm` when there is nothing better.
    let cmds = help(job).map(|h| crate::prep::commands(&h)).unwrap_or_default();
    let step = crate::prep::pipeline_command(&cmds).unwrap_or("warm");
    let mut planned = None;
    let mut out = pipelines(job, &base, &mut p, share, step, &mut planned)?;
    if step != "warm" && matches!(out, Outcome::Exit(Some(c)) if c != 0) {
        log(&job.tx, format!("`scarlett {step}` failed; falling back to `scarlett warm`."));
        planned = None;
        out = pipelines(job, &base, &mut p, share, "warm", &mut planned)?;
    }
    match out {
        Outcome::Exit(Some(0)) => {}
        // The 0.4.6 runtime's warm fails after "0 to make" when no run recorded pipelines (its
        // pipelines folder is missing); nothing was left undone.
        Outcome::Exit(_) if planned == Some(0) => {}
        Outcome::Exit(c) => return Err(format!("scarlett {step} failed (exit code {c:?}); see Logs")),
        Outcome::Stopped | Outcome::Idle => return Err("Stopped. Press Build shaders again to resume.".into()),
    }
    let took = flow::duration(p.started.elapsed().as_secs());
    if p.total == 0 {
        p.total = shader_total;
    }
    finish(job, &mut p);
    Ok(done_note(&took, untranslated, shader_total))
}

/// A finished build: the bar at 100% and the phase reading Done.
fn finish(job: &Job, p: &mut Progress) {
    p.phase = p.phases;
    p.name = "Done";
    p.overall = 1.;
    p.done = p.total;
    p.current = "everything is prepared".into();
    send(job, p);
}

/// The line shown when a build finishes; untranslated shaders are put in proportion.
pub fn done_note(took: &str, untranslated: u64, total: u64) -> String {
    if untranslated == 0 {
        return format!("Shaders built in {took}.");
    }
    let of = if total > untranslated { format!(" of ~{}", thousands(total)) } else { String::new() };
    format!("Shaders built in {took}. {untranslated}{of} shaders can't be translated yet. The game still runs; those effects may be missing. Details in Logs.")
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A hint when the D3D12 Agility folder is missing beside the engine (D3D12 then fails to start,
/// 0x887E0003); the release ZIP has it in runtime\D3D12.
fn agility_note(exe: &Path) -> String {
    match exe.parent() {
        Some(d) if !d.join("D3D12").join("D3D12Core.dll").is_file() => format!(". No D3D12/D3D12Core.dll beside {}: keep the D3D12 folder from the release next to it", exe.display()),
        _ => String::new(),
    }
}

/// The folder translated shaders land in: `<SCARLETT_CACHE_DIR>/shaders`.
fn shader_dir(job: &Job) -> PathBuf {
    job.env
        .iter()
        .find(|(k, _)| *k == "SCARLETT_CACHE_DIR")
        .map(|(_, v)| PathBuf::from(v))
        .unwrap_or_else(|| engine::scarlett_cache(&job.cache_dir))
        .join("shaders")
}

/// Phase 2: `scarlett <step> --jobs N` (`warm`, or a command that builds every pipeline).
fn pipelines(job: &Job, base: &dyn Fn(&mut Command), p: &mut Progress, share: f32, step: &str, planned: &mut Option<u64>) -> Result<Outcome, String> {
    let mut cmd = Command::new(&job.exe);
    base(&mut cmd);
    cmd.arg(step).args(&engine::warm_book_args(p.jobs)[1..]);
    log(&job.tx, format!("> scarlett {step} --jobs {}  (phase 2 of 2: makes the pipelines ahead of time, at low priority)", p.jobs));
    send(job, p);
    let mut tick = Instant::now();
    run(cmd, job, None, |l| {
        if let Some(t) = engine::parse_warm_plan(l) {
            *planned = Some(t);
            p.total = t;
            p.current = if t == 0 { "no recorded pipelines to make".into() } else { format!("{t} pipelines (the engine reports every 1,000)") };
            p.current_since = Instant::now();
        }
        if let Some(n) = engine::parse_warm_made(l) {
            p.done = n.min(p.total);
        }
        if let Some((n, t)) = engine::parse_prepare_named(l).filter(|(s, _, _)| *s != "shaders").map(|(_, d, t)| (d, t)) {
            if *planned != Some(t) {
                *planned = Some(t);
                p.total = t;
                p.current = if t == 0 { "no recorded pipelines to make".into() } else { format!("{t} pipelines") };
                p.current_since = Instant::now();
            }
            p.done = n.min(t);
        }
        if !l.is_empty() {
            p.last_output = Instant::now();
        }
        p.overall = share + (1. - share) * if p.total == 0 { 0. } else { p.done as f32 / p.total as f32 };
        if !l.is_empty() || tick.elapsed() >= Duration::from_secs(1) {
            tick = Instant::now();
            send(job, p);
        }
    })
}

fn classic(job: &Job) -> Result<String, String> {
    let path = state_path(&job.cache_dir, false);
    let mut st = State::load(&path, &job.hash);
    let pack = job.game.join("shaders.rpf");
    if !pack.is_file() {
        return Err("No shaders.rpf in the game folder.".into());
    }
    // Each of the biggest driver compiles takes about 1.5 GB.
    let pipes = engine::jobs(cores(), free_mb(), 1536).min(8);
    let dxc = (cores() / 2).max(2);
    let mut cmd = Command::new(&job.exe);
    cmd.current_dir(&job.runtime)
        .env("XCL_NO_WATCHDOG", "1")
        .env("XCL_DXC_DIR", &job.runtime)
        .env("XCL_DXIL_CACHE", crate::game::dxil_dir(&job.cache_dir))
        .env("XCL_DXC_THREADS", dxc.to_string())
        .env("XCL_PIPELINE_THREADS", pipes.to_string())
        .arg("shaders")
        .arg(&pack)
        .args(["--precompile", "--pipelines", "--only", "cs"]);
    log(&job.tx, format!("> xcl.exe shaders shaders.rpf --precompile --pipelines --only cs  ({dxc} compile threads, {pipes} driver compiles at once; cached shaders are skipped)"));
    let now = Instant::now();
    let mut p = Progress {
        phase: 1,
        phases: 1,
        name: "Compiling shaders and pipelines",
        done: 0,
        total: 0,
        current: "reading the shader pack".into(),
        current_since: now,
        last_output: now,
        overall: 0.,
        on_demand: 0,
        started: now,
        jobs: pipes,
        };
    send(job, &p);
    let idle = job.slow_secs.map(Duration::from_secs);
    let mut tick = Instant::now();
    let mut last_done = 0;
    let out = run(cmd, job, idle, |l| {
        if let Some((d, t, _)) = flow::parse_shader(l) {
            p.done = d;
            p.total = t;
            if d != last_done {
                last_done = d;
                p.current = format!("shaders after {d} (the largest take the driver 6–190 s each)");
                p.current_since = Instant::now();
            }
        }
        if !l.is_empty() {
            p.last_output = Instant::now();
        }
        p.overall = p.done as f32 / p.total.max(1) as f32;
        if !l.is_empty() || tick.elapsed() >= Duration::from_secs(1) {
            tick = Instant::now();
            send(job, &p);
        }
    })?;
    match out {
        Outcome::Exit(Some(0)) => {
            st.on_demand.clear();
            st.save(&path);
            Ok(format!("Shaders built in {}.", flow::duration(p.started.elapsed().as_secs())))
        }
        Outcome::Idle => {
            let left = p.total.saturating_sub(p.done);
            st.on_demand = vec![left];
            st.save(&path);
            log(&job.tx, format!("Nothing finished for {} s: the remaining {left} shaders compile in game (a rebuild resumes them).", job.slow_secs.unwrap_or(0)));
            Ok(format!("Shaders built; {left} slow ones compile in game."))
        }
        Outcome::Stopped => Err("Stopped. Press Build shaders again to resume (finished shaders are kept).".into()),
        Outcome::Exit(c) => Err(format!("xcl shaders failed (exit code {c:?}); see Logs")),
    }
}

/// Returns `(translator, count)` if `dir` holds a completed CLI shader cache whose recorded
/// `.translator` fingerprint matches `job_hash` (`translator:<hex> ...`).
fn matching_cli_shader_cache(dir: &Path, job_hash: &str) -> Option<(String, u64)> {
    let expected = job_hash
        .strip_prefix("translator:")
        .and_then(|rest| rest.split_whitespace().next())
        .filter(|h| !h.is_empty())?;
    let recorded = std::fs::read_to_string(dir.join(".translator")).ok()?;
    if !recorded.trim().eq_ignore_ascii_case(expected) {
        return None;
    }
    let cached = engine::count_cached_shaders(dir);
    (!engine::too_few_shaders(cached)).then(|| (expected.to_string(), cached))
}

#[cfg(test)]
mod tests {
    #[test]
    fn done_note_in_proportion() {
        assert_eq!(super::done_note("6 min", 0, 26000), "Shaders built in 6 min.");
        let n = super::done_note("6 min", 229, 26503);
        assert!(n.contains("229 of ~26,503 shaders can't be translated yet. The game still runs"), "{n}");
    }

    use super::*;

    #[test]
    fn state_is_kept_per_preset_cache() {
        let d = std::env::temp_dir().join(format!("sp-state-{}", std::process::id()));
        let path = d.join("build-world.json");
        let stable = state_key("abc", Path::new("F:/c/scarlett"));
        let exp = state_key("abc", Path::new("F:/c/scarlett-experimental"));
        State { hash: stable.clone(), warm: true, ..Default::default() }.save(&path);
        assert!(State::load(&path, &stable).warm);
        assert!(!State::load(&path, &exp).warm, "another preset's cache is not built");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn state_resets_for_another_engine_build() {
        let dir = std::env::temp_dir().join(format!("sp-state-{}", std::process::id()));
        let path = dir.join("s.json");
        let s = State { hash: "a".into(), warm: true, next: 3000, total: None, on_demand: vec![1000] };
        s.save(&path);
        assert_eq!(State::load(&path, "a"), s);
        assert_eq!(State::load(&path, "b"), State { hash: "b".into(), ..Default::default() });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cli_shader_cache_reuse_requires_matching_translator_and_enough_files() {
        let dir = std::env::temp_dir().join(format!("sp-cli-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sub = dir.join("00");
        std::fs::create_dir_all(&sub).unwrap();
        let needed = (engine::SHADERS_TOTAL / 20) as usize;
        for i in 0..needed {
            std::fs::write(sub.join(format!("s_{i}.bin")), b"x").unwrap();
        }
        std::fs::write(dir.join(".translator"), "abcdef0123456789").unwrap();

        let matched = matching_cli_shader_cache(&dir, "translator:ABCDEF0123456789 C:/cache");
        assert_eq!(matched, Some(("ABCDEF0123456789".into(), needed as u64)));
        assert_eq!(matching_cli_shader_cache(&dir, "translator:9999999999999999 C:/cache"), None);
        assert_eq!(matching_cli_shader_cache(&dir, "build:deadbeef C:/cache"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
