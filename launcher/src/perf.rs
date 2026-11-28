//! The performance readout after a run, parsed from its log: average frame rate, the worst
//! frame interval and the time from start to the world (the first interval that draws a world's
//! worth of geometry). Also the run log files the launcher keeps (`<data>\logs`).
use std::path::{Path, PathBuf};

/// Draws per frame from which a frame counts as "in the world" (menus draw a few dozen).
pub const WORLD_DRAWS: u64 = 1000;
/// Run logs kept.
pub const KEEP_LOGS: usize = 10;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Perf {
    pub avg_fps: Option<f32>,
    pub worst_ms: Option<f32>,
    pub world_secs: Option<f32>,
    pub samples: usize,
}

impl Perf {
    pub fn line(&self) -> Option<String> {
        if self.samples == 0 && self.world_secs.is_none() {
            return None;
        }
        let mut parts = Vec::new();
        if let Some(f) = self.avg_fps {
            parts.push(format!("{f:.1} fps"));
        }
        if let Some(w) = self.worst_ms {
            parts.push(format!("worst frame {w:.0} ms"));
        }
        parts.push(match self.world_secs {
            Some(s) => format!("world in {}", crate::flow::duration(s as u64)),
            None => "world not reached".into(),
        });
        Some(parts.join("  ·  "))
    }
}

/// Removes ANSI colour sequences (the runtime colours its log when it thinks it has a terminal).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Seconds of the day from a leading `2026-11-25T03:19:48.820868Z`.
pub fn timestamp(line: &str) -> Option<f64> {
    let t = line.trim_start();
    let time = t.get(11..)?.split(|c: char| c == 'Z' || c.is_whitespace()).next()?;
    if t.as_bytes().get(10) != Some(&b'T') {
        return None;
    }
    let mut p = time.split(':');
    let (h, m, s): (f64, f64, f64) = (p.next()?.parse().ok()?, p.next()?.parse().ok()?, p.next()?.parse().ok()?);
    Some(h * 3600. + m * 60. + s)
}

fn number_before(s: &str) -> Option<f32> {
    let n: String = s.trim_end().chars().rev().take_while(|c| c.is_ascii_digit() || *c == '.').collect::<Vec<_>>().into_iter().rev().collect();
    n.parse().ok()
}

fn number_after(s: &str) -> Option<u64> {
    let n: String = s.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    n.parse().ok()
}

/// One interval line: `1.3 fps, 781.5 ms/frame (...), 0 draws/frame` -> (fps, ms, draws).
pub fn interval(line: &str) -> Option<(f32, f32, Option<u64>)> {
    let i = line.find(" fps, ")?;
    let fps = number_before(&line[..i])?;
    let rest = &line[i + 6..];
    let j = rest.find(" ms/frame")?;
    let ms: f32 = rest[..j].trim().parse().ok()?;
    let draws = line.find(" draws/frame").and_then(|k| number_before(&line[..k])).map(|d| d as u64);
    Some((fps, ms, draws))
}

/// Parses a run's log lines. `started` is the run's start as seconds of the day, when known
/// (else the first timestamp in the log).
pub fn parse<'a>(lines: impl IntoIterator<Item = &'a str>, started: Option<f64>) -> Perf {
    let mut first = started;
    let mut fps = Vec::new();
    let mut worst: Option<f32> = None;
    let mut world = None;
    let mut in_world = false;
    for raw in lines {
        let l = strip_ansi(raw);
        let ts = timestamp(&l);
        if first.is_none() {
            first = ts;
        }
        if let Some((f, ms, draws)) = interval(&l) {
            if world.is_none() && draws.is_some_and(|d| d >= WORLD_DRAWS) {
                world = ts.zip(first).map(|(t, s)| ((t - s).rem_euclid(86400.)) as f32);
                // boot and loading stalls are not the game's frames: count from the world on
                fps.clear();
                worst = None;
                in_world = true;
            }
            fps.push(f);
            worst = Some(worst.map_or(ms, |w: f32| w.max(ms)));
        } else if let Some(f) = crate::engine::parse_fps(&l).filter(|_| l.contains("fps over")) {
            fps.push(f);
        }
        if let Some(k) = l.find("world after") {
            // a runtime that says so itself: "... world after 102.4 s"
            if world.is_none() {
                world = number_after(&l[k + 11..]).map(|s| s as f32);
            }
        }
    }
    // the first intervals are boot and menus: average the second half of the run
    let tail = if in_world { &fps[..] } else { &fps[fps.len() / 2..] };
    Perf { avg_fps: (!tail.is_empty()).then(|| tail.iter().sum::<f32>() / tail.len() as f32), worst_ms: worst, world_secs: world, samples: fps.len() }
}

pub fn logs_dir() -> PathBuf {
    crate::runtime::data_dir().join("logs")
}

/// A new run log path (`run-<stamp>.log`), pruning old ones.
pub fn new_log() -> PathBuf {
    let dir = logs_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut logs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "log")).collect();
        logs.sort();
        let excess = (logs.len() + 1).saturating_sub(KEEP_LOGS);
        for p in &logs[..excess.min(logs.len())] {
            let _ = std::fs::remove_file(p);
        }
    }
    dir.join(format!("run-{}.log", crate::saves::now_stamp()))
}

/// Run log size in MB when `SCARLETT_RUN_LOG_MB` does not say (#511: a long session's log grew to
/// 53 MB, too big to attach to a report or to read).
pub const RUN_LOG_MB: u64 = 10;

/// The run log's size limit in bytes from `SCARLETT_RUN_LOG_MB`: unset or unreadable is
/// [`RUN_LOG_MB`], `0` or `off` keeps every line.
pub fn run_log_limit(asked: Option<&str>) -> Option<u64> {
    match asked.map(str::trim) {
        Some("0") | Some("off") => None,
        Some(v) => Some(v.parse::<u64>().ok().filter(|&n| n > 0).unwrap_or(RUN_LOG_MB).saturating_mul(1 << 20)),
        None => Some(RUN_LOG_MB << 20),
    }
}

/// A play run's log file with a size limit: the first quarter of the limit keeps the start of
/// the session (startup, the time to the world), the rest keeps a rolling tail (the last frames
/// and the crash). Once the tail outgrows its share, the file is cut back to the start, a line
/// that says how much was left out, and the newest half of the tail, so the file on disk is
/// always whole and never above the limit by more than a line.
pub struct RunLog {
    file: std::fs::File,
    head_max: u64,
    tail_max: u64,
    /// Bytes of the start of the session in the file.
    head: u64,
    /// The start is full: every later line goes to the tail.
    tailing: bool,
    /// The newest lines, up to half the tail's share.
    recent: std::collections::VecDeque<String>,
    recent_bytes: u64,
    /// Bytes and lines of the tail in the file now (after the start and the marker).
    in_file: u64,
    file_lines: u64,
    left_out_lines: u64,
    left_out_bytes: u64,
}

impl RunLog {
    /// `limit` in bytes, `None` for no limit.
    pub fn new(file: std::fs::File, limit: Option<u64>) -> Self {
        let limit = limit.unwrap_or(u64::MAX);
        RunLog {
            file,
            head_max: limit / 4,
            tail_max: limit - limit / 4,
            head: 0,
            tailing: false,
            recent: Default::default(),
            recent_bytes: 0,
            in_file: 0,
            file_lines: 0,
            left_out_lines: 0,
            left_out_bytes: 0,
        }
    }

    pub fn line(&mut self, text: &str) {
        use std::io::Write;
        let n = text.len() as u64 + 1;
        if !self.tailing && self.head + n <= self.head_max {
            self.head += n;
            let _ = writeln!(self.file, "{text}");
            return;
        }
        self.tailing = true;
        let _ = writeln!(self.file, "{text}");
        self.in_file += n;
        self.file_lines += 1;
        self.recent.push_back(text.to_string());
        self.recent_bytes += n;
        while self.recent_bytes > self.tail_max / 2 && self.recent.len() > 1 {
            let old = self.recent.pop_front().unwrap_or_default();
            self.recent_bytes -= old.len() as u64 + 1;
        }
        if self.in_file > self.tail_max {
            self.cut();
        }
    }

    /// Cuts the file back to the start, the marker and the recent lines.
    fn cut(&mut self) {
        use std::io::{Seek, SeekFrom, Write};
        self.left_out_lines += self.file_lines - self.recent.len() as u64;
        self.left_out_bytes += self.in_file - self.recent_bytes;
        let mut out = String::with_capacity(self.recent_bytes as usize + 128);
        out.push_str(&self.marker());
        out.push('\n');
        for l in &self.recent {
            out.push_str(l);
            out.push('\n');
        }
        let _ = self.file.set_len(self.head);
        let _ = self.file.seek(SeekFrom::Start(self.head));
        let _ = self.file.write_all(out.as_bytes());
        self.in_file = self.recent_bytes;
        self.file_lines = self.recent.len() as u64;
    }

    fn marker(&self) -> String {
        format!(
            "--- {:.1} MB of log lines ({} lines) left out here; SCARLETT_RUN_LOG_MB=0 keeps them all ---",
            self.left_out_bytes as f64 / (1u64 << 20) as f64,
            self.left_out_lines
        )
    }
}

pub fn last_log() -> Option<PathBuf> {
    let mut logs: Vec<PathBuf> = std::fs::read_dir(logs_dir()).ok()?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "log")).collect();
    logs.sort();
    logs.pop()
}

/// The last `n` lines of a file.
pub fn tail(path: &Path, n: usize) -> Vec<String> {
    let text = std::fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let lines: Vec<String> = text.lines().map(strip_ansi).collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_intervals() {
        let l = "2026-11-25T03:19:48.820868Z  INFO scarlett_gpu::host: 1.3 fps, 781.5 ms/frame (0.2 preparing: copies 0.0 ms), 0 draws/frame, dro";
        assert_eq!(interval(l), Some((1.3, 781.5, Some(0))));
        assert_eq!(timestamp(l).map(|t| t.round()), Some((3 * 3600 + 19 * 60 + 49) as f64));
        assert_eq!(strip_ansi("\u{1b}[2mA\u{1b}[0m B"), "A B");
    }

    #[test]
    fn readout() {
        let log = [
            "2026-11-25T03:00:00.000Z  INFO x: starting",
            "2026-11-25T03:00:30.000Z  INFO scarlett_gpu::host: 30.0 fps, 33.3 ms/frame (x), 40 draws/frame",
            "2026-11-25T03:01:42.000Z  INFO scarlett_gpu::host: 3.0 fps, 263.0 ms/frame (x), 4200 draws/frame",
            "2026-11-25T03:01:52.000Z  INFO scarlett_gpu::host: 5.0 fps, 200.0 ms/frame (x), 4300 draws/frame",
        ];
        let p = parse(log, None);
        assert_eq!(p.samples, 2);
        assert_eq!(p.worst_ms, Some(263.));
        assert_eq!(p.world_secs, Some(102.));
        assert_eq!(p.avg_fps, Some(4.));
        assert!(p.line().unwrap().contains("worst frame 263 ms"));
        assert_eq!(parse(["nothing"], None).line(), None);
    }

    fn run_log_file(name: &str) -> (PathBuf, std::fs::File) {
        let dir = std::env::temp_dir().join(format!("scarlett-runlog-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("run.log");
        let f = std::fs::File::create(&path).unwrap();
        (path, f)
    }

    #[test]
    fn run_log_limit_reads_the_switch() {
        assert_eq!(run_log_limit(None), Some(RUN_LOG_MB << 20));
        assert_eq!(run_log_limit(Some("25")), Some(25 << 20));
        assert_eq!(run_log_limit(Some("0")), None);
        assert_eq!(run_log_limit(Some("off")), None);
        assert_eq!(run_log_limit(Some("lots")), Some(RUN_LOG_MB << 20));
        assert_eq!(run_log_limit(Some("99999999999999999")), Some(u64::MAX));
    }

    #[test]
    fn run_log_keeps_start_and_end_under_the_limit() {
        let (path, f) = run_log_file("cap");
        let limit = 64 * 1024;
        let mut log = RunLog::new(f, Some(limit));
        let total = 20_000;
        for i in 0..total {
            log.line(&format!("2026-11-25T03:00:00.000Z  INFO scarlett_gpu::host: line {i:06} 20.0 fps, 50.0 ms/frame"));
            let len = std::fs::metadata(&path).unwrap().len();
            assert!(len <= limit + 200, "{len} bytes after line {i}");
        }
        drop(log);
        let lines = tail(&path, usize::MAX);
        assert!(lines[0].contains("line 000000"), "the start is kept");
        assert_eq!(lines.last().unwrap(), &format!("2026-11-25T03:00:00.000Z  INFO scarlett_gpu::host: line {:06} 20.0 fps, 50.0 ms/frame", total - 1));
        let marks: Vec<&String> = lines.iter().filter(|l| l.contains("left out here")).collect();
        assert_eq!(marks.len(), 1);
        // every line is either in the file or counted by the marker
        let left_out: usize = marks[0].split('(').nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
        assert_eq!(lines.len() - 1 + left_out, total);
        // the start and the tail run on without a gap of their own
        let k = lines.iter().position(|l| l.contains("left out here")).unwrap();
        let num = |l: &str| l.split("line ").nth(1).unwrap()[..6].parse::<usize>().unwrap();
        assert_eq!(num(&lines[k + 1]) - num(&lines[k - 1]) - 1, left_out);
        assert!(lines.len() - k > 100, "a long tail is kept");
        // the readout still parses the file
        assert_eq!(parse(lines.iter().map(String::as_str), None).avg_fps, Some(20.));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn run_log_without_a_limit_keeps_everything() {
        let (path, f) = run_log_file("all");
        let mut log = RunLog::new(f, run_log_limit(Some("0")));
        for i in 0..5000 {
            log.line(&format!("line {i}"));
        }
        drop(log);
        let lines = tail(&path, usize::MAX);
        assert_eq!(lines.len(), 5000);
        assert!(!lines.iter().any(|l| l.contains("left out")));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
