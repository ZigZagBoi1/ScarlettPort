//! `SCARLETT_FAST_BOOT=1` (testing only, off by default): press through the
//! title's logos, menus and the account prompt ("ACCOUNT PICKER (Y) /
//! PLAYER (A)": A takes the signed-in player) without a person, then stop
//! for good once the title is in the world.
//!
//! "In the world" is read from the render targets the title binds per
//! presented frame: menus and loading bind a handful, the world a hundred or
//! more. [`THRESHOLD`] binds on [`SUSTAIN`] consecutive frames ends the mode.
//! The fallback is a time limit, `SCARLETT_FAST_BOOT_MAX_S` (default
//! [`MAX_SECONDS`]; `SCARLETT_FAST_BOOT_MAX_FRAMES` sets a frame limit too).
//! After the world is detected, with `SCARLETT_SKIP_CUTSCENES=1`, A is still
//! pressed for [`SKIP_SECONDS`] so the intro cutscene's prompt skips it.
//! Only our synthetic keys depend on this; the title's clocks and our frame
//! pacing are never touched.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

/// Render-target binds in one frame that mark a world frame.
pub const THRESHOLD: u32 = 100;
/// Consecutive world frames needed.
pub const SUSTAIN: u32 = 3;
/// Fallback: fast boot ends this long after the start at the latest.
pub const MAX_SECONDS: u64 = 420;
/// After the world is detected: seconds A is still pressed (cutscene skip).
pub const SKIP_SECONDS: u64 = 25;

static WORLD_AT: OnceLock<Instant> = OnceLock::new();
static MAX_S: OnceLock<u64> = OnceLock::new();
static ENABLED: OnceLock<bool> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();
static IN_WORLD: AtomicBool = AtomicBool::new(false);
static BINDS: AtomicU32 = AtomicU32::new(0);
static FRAME: AtomicU64 = AtomicU64::new(0);
static MAX: OnceLock<u64> = OnceLock::new();
static RUN: AtomicU32 = AtomicU32::new(0);

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn truthy(v: Option<String>) -> bool {
    v.is_some_and(|v| !matches!(v.trim(), "" | "0" | "false" | "off"))
}

/// Fast boot was asked for.
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        let on = truthy(env("SCARLETT_FAST_BOOT"));
        if on {
            START.get_or_init(Instant::now);
        }
        on
    })
}

/// Fast boot is on and the title is not in the world yet: synthetic input applies.
pub fn active() -> bool {
    enabled() && !IN_WORLD.load(Ordering::Relaxed)
}

/// `SCARLETT_SKIP_CUTSCENES=1` and the world was detected less than
/// [`SKIP_SECONDS`] ago: A is still pressed so the intro cutscene skips.
pub fn skipping() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| truthy(env("SCARLETT_SKIP_CUTSCENES")))
        && WORLD_AT.get().is_some_and(|t| t.elapsed().as_secs() < SKIP_SECONDS)
}

/// The detector saw the world (or gave up waiting). Without fast boot this
/// is always false.
pub fn in_world() -> bool {
    IN_WORLD.load(Ordering::Relaxed)
}

/// Presented frames seen so far (while fast boot is on).
pub fn frame() -> u64 {
    FRAME.load(Ordering::Relaxed)
}

/// A frame bound render targets (called per bind).
pub fn note_render_targets() {
    if enabled() {
        BINDS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Pure rule: the world run length after a frame with `binds` binds.
pub fn next_run(run: u32, binds: u32) -> u32 {
    if binds >= THRESHOLD { run + 1 } else { 0 }
}

/// A frame was presented: updates the detector; returns the log line once,
/// when fast boot ends.
pub fn frame_presented() -> Option<String> {
    if !active() {
        return None;
    }
    let frame = FRAME.fetch_add(1, Ordering::Relaxed) + 1;
    let binds = BINDS.swap(0, Ordering::Relaxed);
    let run = next_run(RUN.load(Ordering::Relaxed), binds);
    RUN.store(run, Ordering::Relaxed);
    let max = *MAX.get_or_init(|| {
        env("SCARLETT_FAST_BOOT_MAX_FRAMES").and_then(|v| v.parse().ok()).unwrap_or(u64::MAX)
    });
    let max_s = *MAX_S.get_or_init(|| {
        env("SCARLETT_FAST_BOOT_MAX_S").and_then(|v| v.parse().ok()).unwrap_or(MAX_SECONDS)
    });
    let elapsed = START.get().map_or(0, |s| s.elapsed().as_secs());
    let why = if run >= SUSTAIN {
        WORLD_AT.get_or_init(Instant::now);
        "render targets"
    } else if frame >= max {
        "frame limit"
    } else if elapsed >= max_s {
        "time limit"
    } else {
        return None;
    };
    if IN_WORLD.swap(true, Ordering::Relaxed) {
        return None;
    }
    let secs = START.get().map_or(0.0, |s| s.elapsed().as_secs_f64());
    Some(format!("fast boot: in world after {frame} frames, {secs:.1} s ({why})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_needs_sustained_many_binds() {
        let mut run = 0;
        for binds in [3, 8, 5] {
            run = next_run(run, binds);
        }
        assert_eq!(run, 0);
        run = next_run(run, 200);
        run = next_run(run, 12);
        assert_eq!(run, 0);
        for _ in 0..SUSTAIN {
            run = next_run(run, 200);
        }
        assert!(run >= SUSTAIN);
    }

    #[test]
    fn switch_parsing() {
        assert!(!truthy(None));
        assert!(!truthy(Some("0".into())));
        assert!(truthy(Some("1".into())));
    }
}
