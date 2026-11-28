//! Engine choice and the mapping from launcher settings to the engines' command lines and
//! environment, plus parsing of their shader-build output (pure functions, unit tested).
use crate::Config;
use std::path::{Path, PathBuf};

pub const WORLD: &str = "world";
pub const CLASSIC: &str = "classic";

pub const WORLD_LABEL: &str = "World (furthest: 3D world + gameplay, slow)";
pub const CLASSIC_LABEL: &str = "Classic (menus at 30 fps)";

/// Distinct shader programs `scarlett warm` translates for the supported title (measured).
pub const WARM_TOTAL: u64 = 25_889;
/// Graphics records in the title's precompiled pipeline cache (measured; refined as chunks finish).
pub const GRAPHICS_TOTAL: u64 = 25_471;
/// Records per `scarlett precompile-graphics` run: each run saves the pipeline library when it
/// ends, so a stop loses at most one chunk.
pub const CHUNK: u64 = 1000;
/// The runtime vendored here decodes rectangle-list pipelines right (the tile-grid fix), so play
/// runs set SCARLETT_PRECOMPILED_GRAPHICS=1 (as play-scarlett.ps1 does) and the graphics-pipeline
/// phase of the shader build runs. Both follow this one constant.
pub const PRECOMPILED_GRAPHICS: bool = true;

/// The other verified speed settings (play-scarlett.ps1 sets the same ones, besides the precise
/// syncs and settles): the read-back fix, copy-back only for read-backs, narrow root signatures,
/// views and BVH, and the prepared barriers, tables and reuse.
pub const SPEED_SETTINGS: [&str; 8] = [
    "SCARLETT_READFIX",
    "SCARLETT_COPY_BACK_ONLY_READBACK",
    "SCARLETT_NARROW_ROOT",
    "SCARLETT_NARROW_VIEWS",
    "SCARLETT_NARROW_BVH",
    "SCARLETT_PREP_BARRIERS",
    "SCARLETT_PREP_TABLES",
    "SCARLETT_PREP_REUSE",
];

/// True when the config selects the World engine (anything else is Classic).
pub fn is_world(c: &Config) -> bool {
    c.engine == WORLD
}

/// The World engine's cache folder (translated shaders, pipeline library) inside the launcher's cache.
pub fn scarlett_cache(cache_dir: &str) -> PathBuf {
    let cache = Path::new(cache_dir.trim());
    let dir = cache.join("scarlett");
    crate::runtime::migrate_legacy_cache(cache, &dir);
    dir
}

/// Arguments after the executable: `run "<game exe>" --execute [--console series-s]`.
pub fn world_args(c: &Config, game: &Path) -> Vec<String> {
    let mut a = vec!["run".to_string(), game.join(crate::GAME_EXE).display().to_string(), "--execute".to_string()];
    if c.console_s {
        a.push("--console".into());
        a.push("series-s".into());
    }
    a
}

/// `warm "<game folder>" --jobs N`: translate every shader into the cache and make the compute pipelines.
pub fn warm_args(game: &Path, jobs: usize) -> Vec<String> {
    vec!["warm".into(), game.display().to_string(), "--jobs".into(), jobs.max(1).to_string()]
}

/// Distinct shader programs `scarlett shaders` translates (runtimes since 0.4.6; refined as it runs).
pub const SHADERS_TOTAL: u64 = 26_084;

/// True when `scarlett shaders` ended with far fewer shaders than the title has (under 5%):
/// a wrong folder or an engine that cannot read it, never a prepared cache.
pub fn too_few_shaders(n: u64) -> bool {
    n < SHADERS_TOTAL / 20
}

/// `shaders "<game>" --jobs N`: the translation step of runtimes since 0.4.6 (its `warm` no longer
/// translates).
pub fn shaders_args(game: &Path, jobs: usize) -> Vec<String> {
    vec!["shaders".into(), game.display().to_string(), "--jobs".into(), jobs.max(1).to_string()]
}

/// `warm --jobs N`: runtimes since 0.4.6 make the pipelines earlier runs recorded
/// (`SCARLETT_PIPELINE_BOOK=1`); the game and the cache come from the environment.
pub fn warm_book_args(jobs: usize) -> Vec<String> {
    vec!["warm".into(), "--jobs".into(), jobs.max(1).to_string()]
}

/// `scarlett shaders` progress and summary: `4000 shaders in the cache, 37 s` -> 4000.
pub fn parse_shaders(line: &str) -> Option<u64> {
    line.trim().split_once(" shaders in the cache")?.0.trim().parse().ok()
}

/// `scarlett shaders`'s summary: `26084 shaders in the cache, 3 do not translate, 171 s` -> 3.
pub fn parse_shaders_failed(line: &str) -> Option<u64> {
    let (_, rest) = line.split_once(" shaders in the cache, ")?;
    rest.split_once(" do not translate")?.0.trim().parse().ok()
}

/// What `scarlett shaders` is on while no shader finishes (runtimes after 0.5.7):
/// `12 shaders in the cache, 709 s (reading D:/game/x.rpf)` -> `reading D:/game/x.rpf`.
pub fn parse_shaders_on(line: &str) -> Option<&str> {
    let (_, rest) = line.trim().split_once(" shaders in the cache, ")?;
    rest.split_once(" s (")?.1.strip_suffix(')')
}

/// `scarlett shaders` stopped by its watchdog: `stuck on translating #0123456789abcdef in
/// <file>@0x20: no shader finished for 120 s; ...` -> `translating #0123456789abcdef in <file>@0x20`.
pub fn parse_shaders_stuck(line: &str) -> Option<&str> {
    Some(line.trim().strip_prefix("stuck on ")?.split_once(": no shader finished for ")?.0)
}

/// `scarlett warm`'s plan: `120 pipelines known, 20 made before (f), 0 bad; 100 to make, 8 processes` -> 100.
pub fn parse_warm_plan(line: &str) -> Option<u64> {
    let (_, rest) = line.split_once(" bad; ")?;
    rest.split_once(" to make")?.0.trim().parse().ok()
}

/// `scarlett prepare` progress: `prepare progress library 1000 5200` -> (1000, 5200).
#[cfg(test)]
pub fn parse_prepare_progress(line: &str) -> Option<(u64, u64)> {
    parse_prepare_named(line).map(|(_, d, t)| (d, t))
}

/// Any `scarlett prepare` progress line with its step name:
/// `prepare progress shaders 300 26084` -> ("shaders", 300, 26084).
pub fn parse_prepare_named(line: &str) -> Option<(&str, u64, u64)> {
    let mut w = line.trim().strip_prefix("prepare progress ")?.split_whitespace();
    let (name, done, total) = (w.next()?, w.next()?, w.next()?);
    Some((name, done.parse().ok()?, total.parse().ok()?))
}

/// Translated shaders in a shader cache folder (`<cache>/shaders/xx/<rest of hash>`), counted
/// while the translation runs so the number moves between the engine's progress lines.
/// Temporary files (`.tmp-*`) are not counted.
pub fn count_cached_shaders(dir: &Path) -> u64 {
    let Ok(top) = std::fs::read_dir(dir) else { return 0 };
    let mut n = 0;
    for d in top.flatten() {
        if d.file_name().len() != 2 || !d.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if let Ok(files) = std::fs::read_dir(d.path()) {
            n += files.flatten().filter(|f| !f.file_name().to_string_lossy().starts_with('.')).count() as u64;
        }
    }
    n
}

/// `scarlett warm` progress: `1000 of 5200, 40 s` -> 1000.
pub fn parse_warm_made(line: &str) -> Option<u64> {
    let (n, rest) = line.trim().split_once(" of ")?;
    if !rest.ends_with(" s") {
        return None;
    }
    n.parse().ok()
}

/// `precompile-graphics "<shaders.rpf>" --jobs N --skip S --limit L`.
pub fn graphics_args(game: &Path, jobs: usize, skip: u64, limit: u64) -> Vec<String> {
    vec![
        "precompile-graphics".into(),
        game.join("shaders.rpf").display().to_string(),
        "--jobs".into(),
        jobs.max(1).to_string(),
        "--skip".into(),
        skip.to_string(),
        "--limit".into(),
        limit.to_string(),
    ]
}

/// Environment every World engine process gets: the game and the cache.
pub fn base_env(game: &Path, cache_dir: &str) -> Vec<(&'static str, String)> {
    vec![("SCARLETT_GAME_DIR", game.display().to_string()), ("SCARLETT_CACHE_DIR", scarlett_cache(cache_dir).display().to_string())]
}

/// Environment for a World play run (display settings are added separately). The defaults are
/// play-scarlett.ps1's.
pub fn world_env(c: &Config, game: &Path, cache_dir: &str) -> Vec<(&'static str, String)> {
    let flag = |b: bool| if b { "1" } else { "0" }.to_string();
    let mut v = base_env(game, cache_dir);
    v.extend([
        ("SCARLETT_OWN_UPSCALER", "1".to_string()),
        ("SCARLETT_NO_WATCHDOG", "1".to_string()),
        ("SCARLETT_RELAY", "2".to_string()),
        ("SCARLETT_SYNC_PRECISE", "1".to_string()),
        ("SCARLETT_READ_SETTLE_PRECISE", "1".to_string()),
        ("SCARLETT_PIPELINE_WAIT_MS", "0".to_string()),
        ("SCARLETT_RAYTRACING", flag(c.raytracing)),
        ("SCARLETT_FALLBACK_LIGHT", flag(c.fallback_light)),
        ("SCARLETT_PAD_PS", "1".to_string()),
    ]);
    if PRECOMPILED_GRAPHICS {
        v.push(("SCARLETT_PRECOMPILED_GRAPHICS", "1".to_string()));
    }
    v.extend(SPEED_SETTINGS.iter().map(|k| (*k, "1".to_string())));
    // The engine's picture and residency settings, as in Stable (ambient and sky light, complete
    // textures, buffers kept on the card); without them Custom ran darker and slower than Stable.
    v.extend(crate::presets::CAND4.iter().map(|(k, x)| (*k, x.to_string())));
    if c.raytracing {
        // The switch the runtime reads for the title's own ray-traced light.
        v.push(("SCARLETT_BVH_BUILDER", "1".to_string()));
    }
    if c.debug_view {
        v.push(("SCARLETT_DEBUG_PS", "1".to_string()));
        v.push(("SCARLETT_SHOW_TARGET", "2560x1440-f28".to_string()));
    }
    if c.auto_menu {
        v.push(("SCARLETT_AUTO_INPUT", "1".to_string()));
    }
    v
}

/// Parallel jobs for a build step: at most one per logical core but one (the PC stays usable), and
/// no more than free RAM allows at `per_job_mb` each. At least one.
pub fn jobs(cores: usize, free_mb: Option<u64>, per_job_mb: u64) -> usize {
    let by_cpu = cores.saturating_sub(1).max(1);
    let by_ram = free_mb.map_or(by_cpu, |f| (f.saturating_sub(2048) / per_job_mb.max(1)) as usize);
    by_cpu.min(by_ram).max(1)
}

/// `scarlett warm` progress: `   123 s  4000 shaders` -> 4000.
pub fn parse_warm(line: &str) -> Option<u64> {
    let t = line.trim();
    let rest = t.strip_suffix(" shaders")?;
    rest.rsplit(' ').next()?.parse().ok()
}

/// `scarlett warm`'s summary: `warm  25889 translated, 0 refused; ...` -> (translated, refused).
pub fn parse_warm_end(line: &str) -> Option<(u64, u64)> {
    let t = line.trim().strip_prefix("warm")?.trim_start();
    let (a, rest) = t.split_once(" translated, ")?;
    let (b, _) = rest.split_once(" refused")?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// `scarlett precompile-graphics` progress: `   40 s  500 records: 480 made, ...` -> 500.
pub fn parse_graphics(line: &str) -> Option<u64> {
    let t = line.trim();
    let (head, _) = t.split_once(" records:")?;
    if t.starts_with("precompile-graphics") {
        return None;
    }
    head.rsplit(' ').next()?.parse().ok()
}

/// `scarlett precompile-graphics`'s summary: `precompile-graphics  800 records: ...` -> 800.
pub fn parse_graphics_end(line: &str) -> Option<u64> {
    let t = line.trim().strip_prefix("precompile-graphics")?.trim_start();
    t.split_once(" records:")?.0.trim().parse().ok()
}

/// Frame rate from a log line: `12.5 fps`, `fps: 12.5` or `fps=12.5`. None without a rate.
pub fn parse_fps(line: &str) -> Option<f32> {
    let t = line.to_ascii_lowercase();
    let i = t.find("fps")?;
    let after = &t[i + 3..];
    if after.starts_with([':', '=']) {
        let n: String = after[1..].trim_start().chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        return n.parse().ok();
    }
    let before = t[..i].trim_end();
    let n: String = before.chars().rev().take_while(|c| c.is_ascii_digit() || *c == '.').collect::<Vec<_>>().into_iter().rev().collect();
    n.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(v: &[(&'static str, String)], k: &str) -> Option<String> {
        v.iter().find(|(n, _)| *n == k).map(|(_, x)| x.clone())
    }

    #[test]
    fn default_is_world_with_series_s() {
        let c = Config::default();
        assert!(is_world(&c));
        assert!(c.console_s && !c.raytracing && c.fallback_light && !c.debug_view);
    }

    #[test]
    fn world_args_series_x_and_s() {
        let g = PathBuf::from("D:/game");
        let mut c = Config::default();
        c.console_s = false;
        let a = world_args(&c, &g);
        assert_eq!(a[0], "run");
        assert!(a[1].ends_with(crate::GAME_EXE));
        assert_eq!(a.len(), 3);
        c.console_s = true;
        assert_eq!(&world_args(&c, &g)[3..], ["--console", "series-s"]);
    }

    #[test]
    fn world_env_mapping() {
        let g = PathBuf::from("D:/game");
        let mut c = Config::default();
        let e = world_env(&c, &g, "E:/ScarlettCache");
        assert_eq!(get(&e, "SCARLETT_CACHE_DIR").map(PathBuf::from), Some(PathBuf::from("E:/ScarlettCache").join("scarlett")));
        assert_eq!(get(&e, "SCARLETT_RELAY").as_deref(), Some("2"));
        assert_eq!(get(&e, "SCARLETT_PIPELINE_WAIT_MS").as_deref(), Some("0"));
        assert_eq!(get(&e, "SCARLETT_PRECOMPILED_GRAPHICS").is_some(), PRECOMPILED_GRAPHICS);
        for k in SPEED_SETTINGS {
            assert_eq!(get(&e, k).as_deref(), Some("1"), "{k}");
        }
        assert!(get(&e, "SCARLETT_SHADER_LOOP_LIMIT").is_none());
        assert!(get(&e, "SCARLETT_AUTO_INPUT").is_none());
        c.auto_menu = true;
        c.raytracing = true;
        let e = world_env(&c, &g, "E:/c");
        assert_eq!(get(&e, "SCARLETT_AUTO_INPUT").as_deref(), Some("1"));
        assert_eq!(get(&e, "SCARLETT_RAYTRACING").as_deref(), Some("1"));
        assert_eq!(get(&e, "SCARLETT_BVH_BUILDER").as_deref(), Some("1"));
        for (k, v) in crate::presets::CAND4 {
            assert_eq!(get(&e, k).as_deref(), Some(v), "Custom keeps the engine's settings: {k}");
        }
    }

    #[test]
    fn build_commands() {
        let g = PathBuf::from("D:/game");
        assert_eq!(warm_args(&g, 0), ["warm", "D:/game", "--jobs", "1"]);
        let a = graphics_args(&g, 4, 2000, 1000);
        assert_eq!(a[0], "precompile-graphics");
        assert!(a[1].ends_with("shaders.rpf"));
        assert_eq!(&a[2..], ["--jobs", "4", "--skip", "2000", "--limit", "1000"]);
    }

    #[test]
    fn split_build_commands_and_output() {
        let g = PathBuf::from("D:/game");
        assert_eq!(shaders_args(&g, 0), ["shaders", "D:/game", "--jobs", "1"]);
        assert_eq!(warm_book_args(6), ["warm", "--jobs", "6"]);
        assert_eq!(parse_shaders("4000 shaders in the cache, 37 s"), Some(4000));
        let end = "26084 shaders in the cache, 3 do not translate, 171 s";
        assert_eq!(parse_shaders(end), Some(26084));
        assert_eq!(parse_shaders_failed(end), Some(3));
        assert_eq!(parse_shaders_failed("4000 shaders in the cache, 37 s"), None);
        // #550: the line names what the run is on while nothing finishes, and the watchdog's.
        let quiet = "12 shaders in the cache, 709 s (reading D:/my s (copy)/x.rpf)";
        assert_eq!(parse_shaders(quiet), Some(12));
        assert_eq!(parse_shaders_failed(quiet), None);
        assert_eq!(parse_shaders_on(quiet), Some("reading D:/my s (copy)/x.rpf"));
        assert_eq!(parse_shaders_on("4000 shaders in the cache, 37 s"), None);
        assert_eq!(parse_shaders_on(end), None);
        let stuck = "stuck on translating #0123456789abcdef in D:/game/x.rpf@0x20: no shader finished for 120 s; stopped so the game can start (the rest translate when first used)";
        assert_eq!(parse_shaders_stuck(stuck), Some("translating #0123456789abcdef in D:/game/x.rpf@0x20"));
        assert_eq!(parse_shaders_stuck(quiet), None);
        // v0.4.8 on an empty or unreadable folder: exit 0 with 0, or 29 shaders.
        assert!(too_few_shaders(0) && too_few_shaders(29));
        assert!(!too_few_shaders(26_084) && !too_few_shaders(20_000));
        assert_eq!(parse_warm_plan("120 pipelines known, 20 made before (ab12), 0 bad; 100 to make, 8 processes"), Some(100));
        assert_eq!(parse_warm_made("1000 of 5200, 40 s"), Some(1000));
        assert_eq!(parse_warm_made("pipeline 7 0123abcd: compiler ended of itself s"), None);
        assert_eq!(parse_warm_made("100 made in 12 s (0 have no host pipeline, 0 ended their process); ms each: median 1.0"), None);
        assert_eq!(parse_prepare_progress("prepare progress library 1000 5200"), Some((1000, 5200)));
        assert_eq!(parse_prepare_progress("prepare step 2/3 library"), None);
    }

    #[test]
    fn jobs_are_bounded_by_cpu_and_ram() {
        assert_eq!(jobs(12, None, 1024), 11);
        assert_eq!(jobs(12, Some(2048 + 3 * 1536), 1536), 3);
        assert_eq!(jobs(12, Some(100), 1536), 1);
        assert_eq!(jobs(1, Some(64_000), 1024), 1);
    }

    #[test]
    fn parses_build_output() {
        assert_eq!(parse_warm("   123 s  4000 shaders"), Some(4000));
        assert_eq!(parse_warm("warm         25889 translated, 0 refused; 9000 compute pipelines, 0 refused; 171 s"), None);
        assert_eq!(parse_warm_end("warm         25889 translated, 3 refused; 9000 compute pipelines, 0 refused; 171 s"), Some((25889, 3)));
        assert_eq!(parse_graphics("    40 s  500 records: 480 made, 0 failed, 20 duplicates, 0 unmapped, 0 undecoded"), Some(500));
        let end = "precompile-graphics  471 records: 460 pipelines made, 1 failed, 0 left out, 10 duplicates, 0 with a shader not in the archive, 0 undecoded; 30 s";
        assert_eq!(parse_graphics(end), None);
        assert_eq!(parse_graphics_end(end), Some(471));
    }

    #[test]
    fn fps_parsing() {
        assert_eq!(parse_fps("D3D12.X: 600 frames presented, 27.2 fps over the last 300"), Some(27.2));
        assert_eq!(parse_fps("fps: 12.5"), Some(12.5));
        assert_eq!(parse_fps("nothing"), None);
    }
}
