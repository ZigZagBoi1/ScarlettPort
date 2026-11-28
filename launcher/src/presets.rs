//! Play presets: Stable (the verified C1 setup, the same environment as play.ps1 without -Old),
//! Experimental (C1 plus the fx-cand2 switches judged safe) and Custom (the launcher's own
//! World options). Pure functions, unit tested.
use std::path::Path;

pub const STABLE: &str = "stable";
pub const EXPERIMENTAL: &str = "experimental";
pub const CUSTOM: &str = "custom";
pub const LOOK: &str = "look";
pub const ALL: [&str; 4] = [STABLE, LOOK, EXPERIMENTAL, CUSTOM];

/// Look (opt-in, not part of Stable): Stable plus the picture fixes (`SCARLETT_LOOK`: ambient light
/// in dark rooms, texture and colour fixes, faces, boats that float) and the more accurate shader
/// translation (`SCARLETT_SHADER_FIXES`). Not yet measured on the RTX 3060.
pub const LOOK_ON: [(&str, &str); 2] = [("SCARLETT_LOOK", "1"), ("SCARLETT_SHADER_FIXES", "1")];

/// The runtime build the bundled runtime folder is made from (see the repository's runtime/).
pub const BUNDLED_BUILD: &str = concat!("v", env!("CARGO_PKG_VERSION"), " runtime");
/// The build the Experimental switches were merged into (fx-cand2); older builds ignore them.
pub const EXPERIMENTAL_BUILD: &str = "fx-cand2";

/// C1, promoted 2026-11-25: the precompiled graphics pipelines and the verified speed switches.
pub const C1: [&str; 9] = [
    "SCARLETT_PRECOMPILED_GRAPHICS",
    "SCARLETT_READFIX",
    "SCARLETT_COPY_BACK_ONLY_READBACK",
    "SCARLETT_NARROW_ROOT",
    "SCARLETT_NARROW_VIEWS",
    "SCARLETT_NARROW_BVH",
    "SCARLETT_PREP_BARRIERS",
    "SCARLETT_PREP_TABLES",
    "SCARLETT_PREP_REUSE",
];

/// The engine's picture and residency settings (since 0.4.6): ambient and sky light, textures shown
/// complete, the threaded list and submission path, and buffers of 64 KB and up kept on the card.
pub const CAND4: [(&str, &str); 18] = [
    ("SCARLETT_AMBIENT_AIR", "1.5"),
    ("SCARLETT_TEXTURE_NO_WAIT", "1"),
    ("SCARLETT_WIDE_BACK", "1"),
    ("SCARLETT_CARD_REHASH_RUNS", "1"),
    ("SCARLETT_JOIN_COPIES", "1"),
    ("SCARLETT_INDIRECT_WINDOW", "1"),
    ("SCARLETT_RELAY_COPY", "1"),
    ("SCARLETT_CARD_STEPS", "2"),
    ("SCARLETT_INDIRECT_BATCH", "2"),
    ("SCARLETT_MAP_THREADS", "1"),
    ("SCARLETT_CARD_AWAKE", "1"),
    ("SCARLETT_SHOW_AFTER", "1"),
    ("SCARLETT_AMBIENT", "0.8"),
    ("SCARLETT_THIN", "1"),
    ("SCARLETT_JOIN_PARTS", "8"),
    ("SCARLETT_UPLOAD_AHEAD", "1"),
    ("SCARLETT_LEVELS_ON_DEMAND", "1"),
    ("SCARLETT_CARD_BUFFERS_MIN_KB", "64"),
];

/// Variables play.ps1 clears before a plain run (so nothing left in the launcher's own
/// environment changes the run).
pub const CLEARED: [&str; 8] = [
    "SCARLETT_TEXTURES",
    "SCARLETT_HDR",
    "SCARLETT_PROFILE",
    "SCARLETT_FAST_BOOT",
    "SCARLETT_CAPTURE",
    "SCARLETT_CAPTURE_EVERY",
    "SCARLETT_PRESS_FILE",
    "SCARLETT_RENDER_SCALE",
];

/// fx-cand2 switches Experimental adds to C1: the GPU step of the A/B plan (C1 is GPU-bound)
/// and the world-entry crash guard (2 clean runs against 3 crashes upstream).
pub const EXPERIMENTAL_ON: [(&str, &str); 6] = [
    ("SCARLETT_INDIRECT_WINDOW", "1"),
    ("SCARLETT_INDIRECT_KEEP", "1"),
    ("SCARLETT_TABLED_SKIP_FRAMES", "2"),
    // 0.5.0 load work: booted cleanly in the REL050-ON run, but its load-time gain is inside the
    // boot-to-boot spread (199-404 s), so it stays opt-in.
    ("SCARLETT_LOAD_UNPACED", "1"),
    ("SCARLETT_PREFETCH", "1"),
    ("SCARLETT_PIPELINE_BOOK", "1"),
];

/// 0.5.0 switches the engine turns on by default (measured on the RTX 3060 in REL050-ON: world
/// at 178 s against 244 s, the upscaler's slowest pass 0.1-0.3 ms/frame against 55-71 ms, picture
/// kept). Stable sets them explicitly so an older local build behaves the same.
pub const STABLE_050: [(&str, &str); 4] = [
    ("SCARLETT_SAVE_NO_WAIT", "1"),
    ("SCARLETT_CARD_BIG_ROOM", "1"),
    ("SCARLETT_GUARD_SINK", "1"),
    ("SCARLETT_LOADS_WIDE", "1"),
];

/// fx-cand2 switches left out of Experimental, and why.
pub const EXPERIMENTAL_OFF: [(&str, &str); 6] = [
    ("SCARLETT_INDIRECT_DIRECT", "can hang the GPU"),
    ("SCARLETT_SHADER_OPT", "needs its own cache per value"),
    ("SCARLETT_WIDE_PAGE_SPLIT", "changes the translation, own cache"),
    ("SCARLETT_SAFE_COPY / SKIP_SAME / WRITE_BESIDE / FILL_FREE", "not measured yet, one at a time"),
    ("SCARLETT_COPY_FETCH / QUEUE2", "A/B against each other, not stacked"),
    ("SCARLETT_EARLY_PIPELINES=0", "diagnostic only"),
];

/// Switches the launcher can set that the bundled runtime (engine/ in this repository) does not
/// read: they matter only to a local build that has them (Settings, Runtime). Kept exact by the
/// `bundled_runtime_reads_every_other_switch` test, which reads the engine's sources.
pub const NOT_IN_BUNDLED: [&str; 31] = [
    // C1 and play.ps1's fixed settings (fx- builds)
    "SCARLETT_PRECOMPILED_GRAPHICS",
    "SCARLETT_READFIX",
    "SCARLETT_COPY_BACK_ONLY_READBACK",
    "SCARLETT_NARROW_ROOT",
    "SCARLETT_NARROW_VIEWS",
    "SCARLETT_NARROW_BVH",
    "SCARLETT_PREP_BARRIERS",
    "SCARLETT_PREP_TABLES",
    "SCARLETT_PREP_REUSE",
    "SCARLETT_NO_WATCHDOG",
    "SCARLETT_SYNC_PRECISE",
    "SCARLETT_READ_SETTLE_PRECISE",
    // Look and Experimental
    "SCARLETT_LOOK",
    "SCARLETT_SHADER_FIXES",
    "SCARLETT_INDIRECT_KEEP",
    "SCARLETT_TABLED_SKIP_FRAMES",
    // Custom
    "SCARLETT_RAYTRACING",
    "SCARLETT_FALLBACK_LIGHT",
    "SCARLETT_DEBUG_PS",
    "SCARLETT_SHOW_TARGET",
    "SCARLETT_AUTO_INPUT",
    // Settings, Features
    "SCARLETT_TEXTURES",
    "SCARLETT_HDR",
    "SCARLETT_RUMBLE",
    "SCARLETT_RUMBLE_STRENGTH",
    "SCARLETT_KBM_HINTS",
    "SCARLETT_PACE",
    "SCARLETT_PACE_FPS",
    "SCARLETT_UPSCALE_QUALITY",
    "SCARLETT_UPSCALE_SHARPNESS",
    "SCARLETT_FRAMEGEN",
];

/// True when `text` (a row's explanation) names a switch the bundled runtime does not read.
pub fn names_unread_switch(text: &str) -> bool {
    NOT_IN_BUNDLED.iter().any(|n| text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).any(|w| w == *n))
}

pub fn label(preset: &str) -> &'static str {
    match preset {
        EXPERIMENTAL => "Experimental",
        LOOK => "Look",
        CUSTOM => "Custom",
        _ => "Stable",
    }
}

pub fn blurb(preset: &str) -> &'static str {
    match preset {
        EXPERIMENTAL => "Stable plus unproven switches: loading frames without the frame wait (SCARLETT_LOAD_UNPACED), boot read-ahead (SCARLETT_PREFETCH), pipeline list recording (SCARLETT_PIPELINE_BOOK), and the fx-cand2 indirect keep and tabled skip frames for a local fx build. Booted cleanly, load gain not proven. Own cache.",
        LOOK => "Stable plus the picture fixes (SCARLETT_LOOK) and the shader translation fixes (SCARLETT_SHADER_FIXES) for a local build that has them; the bundled runtime ignores both. Not measured yet.",
        CUSTOM => "Stable plus your World options from Settings, Game (console profile, ray tracing; stand-in light and debug view need a local build).",
        _ => "The engine's default setup: full picture (ambient and sky light, complete textures), threaded submissions, buffers kept on the card, the upscaler's network in card memory, no save-folder retry wait.",
    }
}

/// Exactly play.ps1's environment for a plain run (no -Old and no other switch): game, cache,
/// slot, upscaler, watchdog, relay, pipeline wait, log level, precise syncs, then C1.
pub fn stable_env(game: &Path, scarlett_cache: &Path) -> Vec<(&'static str, String)> {
    let mut v = vec![
        ("SCARLETT_GAME_DIR", game.display().to_string()),
        ("SCARLETT_CACHE_DIR", scarlett_cache.display().to_string()),
        ("SCARLETT_OWN_UPSCALER", "1".to_string()),
        ("SCARLETT_NO_WATCHDOG", "1".to_string()),
        ("SCARLETT_RELAY", "2".to_string()),
        ("SCARLETT_PIPELINE_WAIT_MS", "0".to_string()),
        ("SCARLETT_LOG", "warn".to_string()),
        ("SCARLETT_SYNC_PRECISE", "1".to_string()),
        ("SCARLETT_READ_SETTLE_PRECISE", "1".to_string()),
    ];
    v.extend(C1.iter().map(|k| (*k, "1".to_string())));
    v.extend(CAND4.iter().map(|(k, x)| (*k, x.to_string())));
    v.extend(STABLE_050.iter().map(|(k, x)| (*k, x.to_string())));
    v
}

pub fn look_env(game: &Path, scarlett_cache: &Path) -> Vec<(&'static str, String)> {
    let mut v = stable_env(game, scarlett_cache);
    v.extend(LOOK_ON.iter().map(|(k, x)| (*k, x.to_string())));
    v
}

pub fn experimental_env(game: &Path, scarlett_cache: &Path) -> Vec<(&'static str, String)> {
    let mut v = stable_env(game, scarlett_cache);
    v.extend(EXPERIMENTAL_ON.iter().map(|(k, x)| (*k, x.to_string())));
    v
}

/// The cache a preset uses: Experimental gets its own folder (fx-cand2's cache key includes
/// every switch the translator reads).
pub fn cache_for(preset: &str, base: &Path) -> std::path::PathBuf {
    if preset == EXPERIMENTAL {
        let name = base.file_name().map(|n| format!("{}-experimental", n.to_string_lossy())).unwrap_or_else(|| "experimental".into());
        base.with_file_name(name)
    } else {
        base.to_path_buf()
    }
}

/// Sets `k=v`, replacing an earlier value of the same variable.
pub fn set(env: &mut Vec<(&'static str, String)>, k: &'static str, v: String) {
    env.retain(|(n, _)| *n != k);
    env.push((k, v));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn get(v: &[(&'static str, String)], k: &str) -> Option<String> {
        v.iter().find(|(n, _)| *n == k).map(|(_, x)| x.clone())
    }

    #[test]
    fn stable_matches_play_ps1() {
        let e = stable_env(Path::new("D:/dump"), Path::new("F:/c"));
        assert_eq!(e.len(), 9 + C1.len() + CAND4.len() + STABLE_050.len());
        assert!(get(&e, "SCARLETT_SAVE_SLOT").is_none());
        assert!(get(&e, "SCARLETT_REMAP_LAZY").is_none());
        for (k, v) in [
            ("SCARLETT_OWN_UPSCALER", "1"),
            ("SCARLETT_NO_WATCHDOG", "1"),
            ("SCARLETT_RELAY", "2"),
            ("SCARLETT_PIPELINE_WAIT_MS", "0"),
            ("SCARLETT_LOG", "warn"),
            ("SCARLETT_SYNC_PRECISE", "1"),
            ("SCARLETT_READ_SETTLE_PRECISE", "1"),
        ] {
            assert_eq!(get(&e, k).as_deref(), Some(v), "{k}");
        }
        for k in C1 {
            assert_eq!(get(&e, k).as_deref(), Some("1"), "{k}");
        }
        for (k, v) in CAND4.iter().chain(STABLE_050.iter()) {
            assert_eq!(get(&e, k).as_deref(), Some(*v), "{k}");
        }
        assert!(get(&e, "SCARLETT_LOAD_UNPACED").is_none());
        assert!(get(&e, "SCARLETT_FAST_BOOT").is_none());
        assert!(get(&e, "SCARLETT_LOOK").is_none());
        assert!(get(&e, "SCARLETT_SHADER_FIXES").is_none());
        for k in CLEARED {
            assert!(get(&e, k).is_none());
        }
    }

    #[test]
    fn experimental_adds_only_safe_switches() {
        let e = experimental_env(Path::new("D:/dump"), Path::new("F:/c"));
        assert_eq!(get(&e, "SCARLETT_INDIRECT_KEEP").as_deref(), Some("1"));
        assert_eq!(get(&e, "SCARLETT_TABLED_SKIP_FRAMES").as_deref(), Some("2"));
        assert!(get(&e, "SCARLETT_INDIRECT_DIRECT").is_none());
        assert!(get(&e, "SCARLETT_SHADER_OPT").is_none());
        assert_eq!(cache_for(EXPERIMENTAL, Path::new("F:/S/cache-fast")), PathBuf::from("F:/S/cache-fast-experimental"));
        assert_eq!(cache_for(STABLE, Path::new("F:/S/cache-fast")), PathBuf::from("F:/S/cache-fast"));
    }

    #[test]
    fn look_is_stable_plus_look() {
        let e = look_env(Path::new("D:/dump"), Path::new("F:/c"));
        assert_eq!(e.len(), stable_env(Path::new("D:/dump"), Path::new("F:/c")).len() + LOOK_ON.len());
        assert_eq!(get(&e, "SCARLETT_LOOK").as_deref(), Some("1"));
        assert_eq!(get(&e, "SCARLETT_SHADER_FIXES").as_deref(), Some("1"));
    }

    /// Every `SCARLETT_*` name in the engine's Rust sources (the former spelling counts too).
    fn engine_names() -> std::collections::HashSet<String> {
        fn walk(d: &Path, out: &mut std::collections::HashSet<String>) {
            for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    if p.file_name().is_some_and(|n| n != "target") {
                        walk(&p, out);
                    }
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let t = std::fs::read_to_string(&p).unwrap_or_default();
                    for w in t.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
                        for pre in ["SCARLETT_", concat!("VIC", "TORY_")] {
                            if let Some(rest) = w.strip_prefix(pre) {
                                out.insert(format!("SCARLETT_{rest}"));
                            }
                        }
                    }
                }
            }
        }
        let mut out = std::collections::HashSet::new();
        walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("engine"), &mut out);
        out
    }

    #[test]
    fn bundled_runtime_reads_every_other_switch() {
        let read = engine_names();
        if read.is_empty() {
            return; // the launcher was built outside the repository
        }
        let (g, c) = (Path::new("D:/dump"), Path::new("F:/c"));
        let mut cfg = crate::Config::default();
        (cfg.raytracing, cfg.debug_view, cfg.auto_menu) = (true, true, true);
        let mut t = crate::tuning::Tuning { full_textures: true, hdr: true, pad_ps: true, rumble: false, frame_cap: 30, upscaler: "fsr".into(), ray_tracing: true, overlay: true, gpu: "1".into(), ..Default::default() };
        t.kbm_hints = "off".into();
        let mut set: Vec<&str> = Vec::new();
        for e in [stable_env(g, c), look_env(g, c), experimental_env(g, c), crate::engine::world_env(&cfg, g, "F:/c"), t.env(true)] {
            set.extend(e.iter().map(|(k, _)| *k));
        }
        t.rumble = true;
        t.rumble_strength = 2.0;
        set.extend(t.env(true).iter().map(|(k, _)| *k));
        for k in set {
            let listed = NOT_IN_BUNDLED.contains(&k);
            assert_eq!(read.contains(k), !listed, "{k}: {}", if listed { "listed as unread, but the engine reads it" } else { "the engine does not read it; add it to NOT_IN_BUNDLED" });
        }
        for k in NOT_IN_BUNDLED {
            assert!(!read.contains(k), "{k} is read by the engine now");
        }
        assert!(names_unread_switch("Ray tracing (SCARLETT_RAYTRACING)."));
        assert!(!names_unread_switch("overlay (SCARLETT_PANEL)."));
        assert!(!names_unread_switch("SCARLETT_PACE_FPSX"));
    }

    #[test]
    fn set_replaces() {
        let mut e = vec![("A", "1".to_string())];
        set(&mut e, "A", "2".into());
        assert_eq!(e, vec![("A", "2".to_string())]);
    }
}
