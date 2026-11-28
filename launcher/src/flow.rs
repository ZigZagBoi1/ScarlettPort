//! Guided Play flow: stage state machine and progress-line parsing for the runtime's output.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    GameNotSet,
    ShadersNotBuilt,
    BuildingShaders,
    Ready,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum World {
    /// Shaders are not ready yet.
    Waiting,
    /// No journal: play once to record world pipelines.
    NeedPlay,
    /// Journal present and not built yet (or it grew since).
    Available,
    Building,
    Done,
}

pub struct Inputs<'a> {
    pub game_complete: bool,
    pub precompiling: bool,
    pub built_hash: &'a str,
    /// None while the runtime is unpacking: staleness is then unknown and not reported.
    pub runtime_hash: Option<&'a str>,
    /// The engine builds its shaders while you play: nothing to build up front.
    pub inline_shaders: bool,
}

pub fn shaders_current(i: &Inputs) -> bool {
    i.inline_shaders || (!i.built_hash.is_empty() && i.runtime_hash.is_none_or(|h| h == i.built_hash))
}

pub fn stage(i: &Inputs) -> Stage {
    if i.precompiling {
        Stage::BuildingShaders
    } else if !i.game_complete {
        Stage::GameNotSet
    } else if shaders_current(i) {
        Stage::Ready
    } else {
        Stage::ShadersNotBuilt
    }
}

pub fn world(stage: Stage, building: bool, journal_len: Option<u64>, built_len: u64) -> World {
    if building {
        World::Building
    } else if stage != Stage::Ready {
        World::Waiting
    } else {
        match journal_len {
            None => World::NeedPlay,
            Some(l) if l == built_len => World::Done,
            Some(_) => World::Available,
        }
    }
}

/// `  6800/26503 (42 s)` -> (6800, 26503, 42)
pub fn parse_shader(line: &str) -> Option<(u64, u64, u64)> {
    let t = line.trim();
    let (a, rest) = t.split_once('/')?;
    if a.is_empty() || !a.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let total: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let inner = rest[total.len()..].trim_start().strip_prefix('(')?;
    let secs: String = inner.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !inner[secs.len()..].trim_start().starts_with('s') {
        return None;
    }
    Some((a.parse().ok()?, total.parse().ok()?, secs.parse().ok()?))
}

/// `  120 done, 880 left (3.5/s, ~4 min to go)` -> (120, 880)
pub fn parse_pipelines_progress(line: &str) -> Option<(u64, u64)> {
    let t = line.trim();
    let (done, rest) = t.split_once(" done, ")?;
    let (left, _) = rest.split_once(" left")?;
    Some((done.trim().parse().ok()?, left.trim().parse().ok()?))
}

/// Rate from the first sample of this run, so a resumed build's earlier work does not skew it.
#[derive(Default)]
pub struct Eta {
    first: Option<(u64, u64)>,
}

impl Eta {
    pub fn reset(&mut self) {
        self.first = None;
    }

    pub fn remaining_secs(&mut self, done: u64, total: u64, secs: u64) -> Option<u64> {
        let (d0, s0) = *self.first.get_or_insert((done, secs));
        let left = total.saturating_sub(done);
        if done > d0 && secs > s0 {
            Some(left * (secs - s0) / (done - d0))
        } else {
            None
        }
    }
}

pub fn duration(secs: u64) -> String {
    if secs >= 3600 {
        format!("{} h {} min", secs / 3600, secs % 3600 / 60)
    } else if secs >= 60 {
        format!("{} min", secs.div_ceil(60))
    } else {
        format!("{secs} s")
    }
}

/// Seconds left from the time so far and the fraction done (None until 2% is done).
pub fn eta_secs(elapsed: u64, fraction: f32) -> Option<u64> {
    (0.02..1.).contains(&fraction).then(|| (elapsed as f32 * (1. - fraction) / fraction).round() as u64)
}

/// A plain reason for a run that stopped on a known problem with the game files, from the
/// run's last log lines (newest last), or `None` when nothing in them is recognised.
pub fn stop_reason(lines: &[String]) -> Option<String> {
    for l in lines.iter().rev() {
        let t = l.trim();
        // The run's command line: what is older belongs to an earlier run (#508 blamed the
        // second run's stop on the first one's missing DLLs).
        if t.starts_with("> ") {
            break;
        }
        if t.starts_with("not a title") {
            // The runtime found Windows' own DLLs among the executable's imports (#508).
            return Some(format!(
                "{exe} in the game folder is not the game: it imports Windows' own DLLs, as a PC program does (it may be a renamed copy of this launcher). Put the {exe} of your own copy of the game in its place. Copying Windows' DLLs into the game folder does not help.",
                exe = crate::GAME_EXE
            ));
        }
        if let Some(rest) = t.strip_prefix("missing dlls") {
            // "missing dlls   A.dll, B.dll not in <game>, and neither ... the package is incomplete; ..."
            let rest = rest.trim_start();
            let names = rest.split(" not in ").next().unwrap_or(rest).trim();
            return Some(format!(
                "The game folder is missing {names}: the game package is incomplete. Copy every file of the package into the game folder, then run Verify game files."
            ));
        }
        if t.contains("malformed image") || t.contains("extends beyond file bounds") {
            return Some("The game's executable is incomplete or damaged, so it cannot start. Run Verify game files; if it passes, copy the game folder again from its source.".into());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_dlls_are_named_plainly() {
        let lines = vec![
            "title          7952987A, sa".to_string(),
            r"missing dlls   XboxBvhBuilder.dll not in <game>, and neither the host nor ScarlettPort provides it: the package is incomplete; copy every file of the package next to the executable".into(),
            "Error: the title's own DLLs are missing: ...".into(),
            "Game ended. Stopped (exit code 1).".into(),
        ];
        let r = stop_reason(&lines).unwrap();
        assert!(r.contains("missing XboxBvhBuilder.dll:") && r.contains("Verify"), "{r}");
        assert!(stop_reason(&["Error: malformed image: section '.text' needs ...".into()]).unwrap().contains("Verify"));
        assert_eq!(stop_reason(&["Game ended. Stopped (exit code 1).".into()]), None);
    }

    #[test]
    fn a_pc_program_is_not_blamed_on_the_package() {
        // #508: the launcher renamed to the game's executable.
        let lines = vec![
            r"> <game>\runtime\ScarlettPort-Runtime.exe run <game>\game_scarlett_master.exe --execute".to_string(),
            "not a title    game_scarlett_master.exe imports USER32.dll, gdi32.dll: Windows' own DLLs, not the game package's. ...".into(),
            "Error: game_scarlett_master.exe is not an Xbox Series title: ...".into(),
        ];
        let r = stop_reason(&lines).unwrap();
        assert!(r.contains("is not the game") && r.contains("does not help"), "{r}");
        assert!(!r.contains("Copy every file"), "{r}");
    }

    #[test]
    fn an_earlier_run_does_not_explain_this_one() {
        // #508: the second run stopped on a 32-bit DLL and was told the first run's reason.
        let lines = vec![
            "> ScarlettPort-Runtime.exe run game_scarlett_master.exe --execute".to_string(),
            r"missing dlls   XINPUT1_4.dll not in <game>, and neither the host nor ScarlettPort provides it: ...".into(),
            "Game ended. Stopped (exit code 1).".into(),
            "> ScarlettPort-Runtime.exe run game_scarlett_master.exe --execute".into(),
            "Error: something else".into(),
        ];
        assert_eq!(stop_reason(&lines), None);
        assert!(stop_reason(&lines[..3]).unwrap().contains("missing XINPUT1_4.dll"));
    }

    fn inp<'a>(game: bool, pre: bool, built: &'a str, rt: Option<&'a str>) -> Inputs<'a> {
        Inputs { game_complete: game, precompiling: pre, built_hash: built, runtime_hash: rt, inline_shaders: false }
    }

    #[test]
    fn parses_shader_lines() {
        assert_eq!(parse_shader("  6800/26503 (42 s)"), Some((6800, 26503, 42)));
        assert_eq!(parse_shader("2600/26503 (718 s)"), Some((2600, 26503, 718)));
        assert_eq!(parse_shader("a/b (1 s)"), None);
        assert_eq!(parse_shader("12/34 (5 x)"), None);
        assert_eq!(parse_shader("hello"), None);
        assert_eq!(parse_pipelines_progress("  120 done, 880 left (3.5/s, ~4 min to go)"), Some((120, 880)));
        assert_eq!(parse_pipelines_progress("nothing"), None);
    }

    #[test]
    fn eta() {
        let mut e = Eta::default();
        assert_eq!(e.remaining_secs(6000, 10000, 100), None); // resumed: no rate yet
        assert_eq!(e.remaining_secs(6100, 10000, 110), Some(3900 * 10 / 100));
        let mut f = Eta::default();
        assert_eq!(f.remaining_secs(0, 1000, 0), None);
        assert_eq!(f.remaining_secs(100, 1000, 10), Some(90));
        assert_eq!(duration(7500), "2 h 5 min");
        assert_eq!(duration(90), "2 min");
    }

    #[test]
    fn stages() {
        assert_eq!(stage(&inp(false, false, "", None)), Stage::GameNotSet);
        assert_eq!(stage(&inp(true, false, "", Some("h"))), Stage::ShadersNotBuilt);
        assert_eq!(stage(&inp(true, true, "", Some("h"))), Stage::BuildingShaders);
        assert_eq!(stage(&inp(true, false, "h", Some("h"))), Stage::Ready);
        // a new runtime marks shaders stale
        assert_eq!(stage(&inp(true, false, "old", Some("new"))), Stage::ShadersNotBuilt);
        // runtime still unpacking: trust the stored hash
        assert_eq!(stage(&inp(true, false, "old", None)), Stage::Ready);
        assert_eq!(stage(&inp(false, false, "h", Some("h"))), Stage::GameNotSet);
    }

    #[test]
    fn inline_shaders_are_ready() {
        let i = Inputs { game_complete: true, precompiling: false, built_hash: "", runtime_hash: Some("h"), inline_shaders: true };
        assert_eq!(stage(&i), Stage::Ready);
        let i = Inputs { game_complete: false, ..i };
        assert_eq!(stage(&i), Stage::GameNotSet);
    }

    #[test]
    fn world_steps() {
        assert_eq!(world(Stage::ShadersNotBuilt, false, Some(99), 0), World::Waiting);
        assert_eq!(world(Stage::Ready, false, None, 0), World::NeedPlay);
        assert_eq!(world(Stage::Ready, false, Some(99), 0), World::Available);
        assert_eq!(world(Stage::Ready, false, Some(99), 99), World::Done);
        assert_eq!(world(Stage::Ready, false, Some(120), 99), World::Available);
        assert_eq!(world(Stage::Ready, true, Some(99), 0), World::Building);
    }

    #[test]
    fn eta_from_progress() {
        assert_eq!(super::eta_secs(60, 0.25), Some(180));
        assert_eq!(super::eta_secs(5, 0.01), None);
        assert_eq!(super::eta_secs(100, 1.0), None);
    }
}
