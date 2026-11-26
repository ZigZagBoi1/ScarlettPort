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
pub const BUNDLED_BUILD: &str = "v0.4.7 runtime";
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
pub const EXPERIMENTAL_ON: [(&str, &str); 3] = [("SCARLETT_INDIRECT_WINDOW", "1"), ("SCARLETT_INDIRECT_KEEP", "1"), ("SCARLETT_TABLED_SKIP_FRAMES", "2")];

/// fx-cand2 switches left out of Experimental, and why.
pub const EXPERIMENTAL_OFF: [(&str, &str); 6] = [
    ("SCARLETT_INDIRECT_DIRECT", "can hang the GPU"),
    ("SCARLETT_SHADER_OPT", "needs its own cache per value"),
    ("SCARLETT_WIDE_PAGE_SPLIT", "changes the translation, own cache"),
    ("SCARLETT_SAFE_COPY / SKIP_SAME / WRITE_BESIDE / FILL_FREE", "not measured yet, one at a time"),
    ("SCARLETT_COPY_FETCH / QUEUE2", "A/B against each other, not stacked"),
    ("SCARLETT_EARLY_PIPELINES=0", "diagnostic only"),
];

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
        EXPERIMENTAL => "C1 plus the safe fx-cand2 switches (indirect window and keep, tabled skip frames). Own cache.",
        LOOK => "Stable plus the picture fixes (SCARLETT_LOOK) and the shader translation fixes (SCARLETT_SHADER_FIXES). Opt-in, not yet measured on the RTX 3060; the first run rebuilds shaders.",
        CUSTOM => "Your World options from Settings (ray tracing, stand-in light, debug view).",
        _ => "The engine's default setup: full picture (ambient and sky light, complete textures), threaded submissions, buffers kept on the card.",
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
        assert_eq!(e.len(), 9 + C1.len() + CAND4.len());
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
        for (k, v) in CAND4 {
            assert_eq!(get(&e, k).as_deref(), Some(v), "{k}");
        }
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

    #[test]
    fn set_replaces() {
        let mut e = vec![("A", "1".to_string())];
        set(&mut e, "A", "2".into());
        assert_eq!(e, vec![("A", "2".to_string())]);
    }
}
