//! Runtime features added on 2026-11-24/25 (full textures, HDR, PlayStation pads, rumble, keyboard
//! hints, frame cap, upscaler and frame generation): the launcher's settings for them, the
//! environment variables they set and the `scarlett.toml` keys they write. A setting at its
//! default sets nothing, so the Stable preset stays exactly play.ps1's environment.
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Tuning {
    pub full_textures: bool,
    pub hdr: bool,
    pub pad_ps: bool,
    pub rumble: bool,
    pub rumble_strength: f32,
    /// "off", "auto" or "always".
    pub kbm_hints: String,
    /// 0 = no cap (the runtime's own pacing), else 30 or 60.
    pub frame_cap: u32,
    /// "" = the runtime's default, else "title", "stretch" or "fsr".
    pub upscaler: String,
    /// native | quality | balanced | performance | ultra
    pub quality: String,
    pub sharpness: f32,
    pub framegen: bool,
    /// Adds `scarlett_gpu::host=info` to the log level so the run log has frame times
    /// (the performance readout). Off keeps the log at play.ps1's `warn`.
    pub perf_log: bool,
    /// the title's own ray-traced indirect light (SCARLETT_BVH_BUILDER=1). Lit
    /// interiors, about a third slower.
    pub ray_tracing: bool,
    /// fps, frame time and the longest frame on the picture (SCARLETT_PANEL=1; F3
    /// shows and hides it in game).
    pub overlay: bool,
    /// The mouse turns the camera (the right stick) while the game window has focus
    /// (SCARLETT_MOUSE_LOOK: 0 is off, else the sensitivity; the engine's default is 1).
    pub mouse_look: bool,
    pub mouse_sensitivity: f32,
    /// The game runs below normal priority so the desktop stays responsive while it plays.
    pub friendly: bool,
    /// Leaves two logical cores to the desktop while the game plays (PCs with 6 or more).
    pub free_cores: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            full_textures: false,
            hdr: false,
            pad_ps: false,
            rumble: true,
            rumble_strength: 1.0,
            kbm_hints: "auto".into(),
            frame_cap: 0,
            upscaler: String::new(),
            quality: "quality".into(),
            sharpness: 0.2,
            framegen: false,
            perf_log: false,
            ray_tracing: false,
            overlay: false,
            mouse_look: true,
            mouse_sensitivity: 1.0,
            friendly: true,
            free_cores: true,
        }
    }
}

pub const HINTS: [&str; 3] = ["off", "auto", "always"];
pub const UPSCALERS: [&str; 4] = ["", "title", "stretch", "fsr"];
pub const QUALITIES: [&str; 5] = ["native", "quality", "balanced", "performance", "ultra"];
pub const CAPS: [u32; 3] = [0, 30, 60];

/// Every key the launcher owns in `scarlett.toml`, including the ones it writes only in some
/// states. [`merge_toml`] drops the managed keys it is not asked to write, so turning a feature
/// back off cannot leave its old value in the file for a runtime that reads these sections.
pub const MANAGED: [(&str, &str); 10] = [
    ("display", "hdr"),
    ("game", "rumble"),
    ("game", "rumble_strength"),
    ("game", "kbm_hints"),
    ("pace", "pace"),
    ("pace", "fps"),
    ("upscale", "upscaler"),
    ("upscale", "quality"),
    ("upscale", "sharpness"),
    ("upscale", "framegen"),
];

/// FidelityFX DLLs the runtime loads for FSR (either the loader or an MIT-built API DLL).
pub const FSR_DLLS: [&str; 2] = ["amd_fidelityfx_loader_dx12.dll", "amd_fidelityfx_dx12.dll"];

/// FSR can run: a FidelityFX DLL beside the runtime, or the game's own AMD upscaler
/// (`amd_fidelityfx_upscaler*`) in the game folder, which is where the engine looks.
pub fn fsr_available(runtime_dir: &Path, game_dir: &Path) -> bool {
    FSR_DLLS.iter().any(|d| runtime_dir.join(d).is_file())
        || std::fs::read_dir(game_dir).is_ok_and(|entries| entries.flatten().any(|e| e.file_name().to_string_lossy().to_ascii_lowercase().starts_with("amd_fidelityfx_upscaler")))
}

fn num(x: f32) -> String {
    let s = format!("{x:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.contains('.') { s.to_string() } else { format!("{s}.0") }
}

impl Tuning {
    /// Environment variables for the settings that differ from the default. `fsr` says whether
    /// the FidelityFX DLL is present (FSR and frame generation are not asked for without it).
    pub fn env(&self, fsr: bool) -> Vec<(&'static str, String)> {
        let d = Tuning::default();
        let mut v = Vec::new();
        if self.full_textures {
            v.push(("SCARLETT_TEXTURES", "full".to_string()));
        }
        if self.hdr {
            v.push(("SCARLETT_HDR", "1".to_string()));
        }
        if self.pad_ps {
            v.push(("SCARLETT_PAD_PS", "1".to_string()));
        }
        if !self.rumble {
            v.push(("SCARLETT_RUMBLE", "0".to_string()));
        } else if (self.rumble_strength - d.rumble_strength).abs() > 0.001 {
            v.push(("SCARLETT_RUMBLE_STRENGTH", num(self.rumble_strength)));
        }
        if self.kbm_hints != d.kbm_hints {
            v.push(("SCARLETT_KBM_HINTS", self.kbm_hints.clone()));
        }
        if self.frame_cap > 0 {
            v.push(("SCARLETT_PACE", "1".to_string()));
            v.push(("SCARLETT_PACE_FPS", self.frame_cap.to_string()));
        }
        if !self.upscaler.is_empty() && (self.upscaler != "fsr" || fsr) {
            v.push(("SCARLETT_UPSCALER", self.upscaler.clone()));
            if self.upscaler == "fsr" {
                v.push(("SCARLETT_UPSCALE_QUALITY", self.quality.clone()));
                v.push(("SCARLETT_UPSCALE_SHARPNESS", num(self.sharpness)));
                v.push(("SCARLETT_FRAMEGEN", if self.framegen { "1" } else { "0" }.to_string()));
            }
        }
        if self.ray_tracing {
            v.push(("SCARLETT_BVH_BUILDER", "1".to_string()));
        }
        if self.overlay {
            v.push(("SCARLETT_PANEL", "1".to_string()));
        }
        if !self.mouse_look {
            v.push(("SCARLETT_MOUSE_LOOK", "0".to_string()));
        } else if (self.mouse_sensitivity - d.mouse_sensitivity).abs() > 0.001 {
            v.push(("SCARLETT_MOUSE_LOOK", num(self.mouse_sensitivity)));
        }
        v
    }

    /// `(section, key, TOML value)` for `scarlett.toml`. Managed keys not returned here are
    /// removed by [`merge_toml`], so a setting turned back to its default is reset in the file too.
    pub fn toml_keys(&self, fsr: bool) -> Vec<(&'static str, &'static str, String)> {
        let q = |s: &str| format!("\"{s}\"");
        let mut v = vec![
            ("display", "hdr", self.hdr.to_string()),
            ("game", "rumble", self.rumble.to_string()),
            ("game", "rumble_strength", num(self.rumble_strength)),
            ("game", "kbm_hints", q(&self.kbm_hints)),
            ("pace", "pace", (self.frame_cap > 0).to_string()),
        ];
        if self.frame_cap > 0 {
            v.push(("pace", "fps", self.frame_cap.to_string()));
        }
        if !self.upscaler.is_empty() && (self.upscaler != "fsr" || fsr) {
            v.push(("upscale", "upscaler", q(&self.upscaler)));
            v.push(("upscale", "quality", q(&self.quality)));
            v.push(("upscale", "sharpness", num(self.sharpness)));
            v.push(("upscale", "framegen", (self.framegen && fsr).to_string()));
        }
        v
    }
}

/// Sets `keys` in TOML `text`, keeping every other line (comments, `[controls]`, what the F1
/// overlay saved). A key is replaced in its section, or added at the section's end; a missing
/// section is appended.
pub fn merge_toml(text: &str, keys: &[(&str, &str, String)]) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let header = |l: &str| {
        let t = l.trim();
        (t.starts_with('[') && !t.starts_with("[[")).then(|| t.trim_start_matches('[').split(']').next().unwrap_or("").trim().to_string())
    };
    // A managed key that is absent from `keys` has been turned off (or its DLL is gone): remove
    // it rather than leaving a stale value the UI no longer shows. Everything else is kept.
    let wanted: Vec<(&str, &str)> = keys.iter().map(|(s, k, _)| (*s, *k)).collect();
    let mut kept: Vec<(String, String)> = Vec::new();
    let mut section = String::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim_start().starts_with("[[") {
            section.clear(); // an array of tables is not a section this launcher manages
        } else if let Some(s) = header(&lines[i]) {
            section = s;
        } else if let Some(k) = lines[i].trim_start().split('=').next() {
            let k = k.trim().trim_matches(['"', '\'']).to_string();
            if MANAGED.iter().any(|(s, m)| *s == section.as_str() && *m == k.as_str()) {
                // Keep the first line of a key that is still managed; drop the rest, and every
                // line of one that has been turned off.
                let keep = wanted.iter().any(|(s, w)| *s == section.as_str() && *w == k.as_str())
                    && !kept.iter().any(|(s, w)| s == &section && w == &k);
                if keep {
                    kept.push((section.clone(), k));
                } else {
                    lines.remove(i);
                    continue;
                }
            }
        }
        i += 1;
    }
    for (section, key, value) in keys {
        let start = lines.iter().position(|l| header(l).as_deref() == Some(*section));
        let line = format!("{key} = {value}");
        match start {
            Some(s) => {
                let end = lines[s + 1..].iter().position(|l| header(l).is_some()).map_or(lines.len(), |e| s + 1 + e);
                let hit = (s + 1..end).find(|&i| {
                    let t = lines[i].trim_start();
                    t.split('=').next().is_some_and(|k| k.trim() == *key) && t.contains('=')
                });
                match hit {
                    Some(i) => lines[i] = line,
                    None => {
                        // after the section's last non-blank line
                        let mut at = end;
                        while at > s + 1 && lines[at - 1].trim().is_empty() {
                            at -= 1;
                        }
                        lines.insert(at, line);
                    }
                }
            }
            None => {
                if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                    lines.push(String::new());
                }
                lines.push(format!("[{section}]"));
                lines.push(line);
            }
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Writes the managed keys into `path` (created when missing).
///
/// The Runtime page asks for this on every frame it draws, so an unchanged file is not written
/// again, and a write goes to a file beside it that is then renamed over it: a crash or a power
/// cut in the middle leaves the old file, not one full of zero bytes (#555). A file that is not
/// TOML at all is moved aside to `scarlett.toml.damaged` and a new one is started; the returned
/// line says so.
pub fn write_toml(path: &Path, keys: &[(&str, &str, String)]) -> std::io::Result<Option<String>> {
    const NEW: &str = "# Written by the ScarlettPort launcher (Settings, Features). Other lines are kept.\n";
    let mut note = None;
    let (old, there) = match std::fs::read(path) {
        Err(_) => (NEW.to_string(), false),
        Ok(bytes) => match toml_text(&bytes) {
            Some(text) => (text.to_string(), true),
            None => {
                let mut aside = path.as_os_str().to_owned();
                aside.push(".damaged");
                let aside = std::path::PathBuf::from(aside);
                std::fs::rename(path, &aside)?;
                note = Some(format!("{} was damaged (not TOML): it is kept as {} and a new one was written.", path.display(), aside.display()));
                (NEW.to_string(), false)
            }
        },
    };
    let new = merge_toml(&old, keys);
    if there && new == old {
        return Ok(None);
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    write_atomically(path, new.as_bytes())?;
    Ok(note)
}

/// The file's text without a UTF-8 byte-order mark, or `None` when it is not TOML at all (zero
/// bytes from a write cut short, or anything else that does not parse).
fn toml_text(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    text.parse::<toml::Table>().is_ok().then_some(text)
}

/// Writes `bytes` to a file beside `path`, flushes it to the disk, then renames it over `path`:
/// whoever reads `path` sees the old file or the new one, never a part.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp-{}", std::process::id()));
    let tmp = path.with_file_name(name);
    let res = std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(bytes)?;
        f.sync_all()
    });
    let res = res.and_then(|()| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

#[cfg(test)]
mod tests {
    #[test]
    fn fsr_found_in_the_game_folder() {
        let base = std::env::temp_dir().join(format!("sp-fsr-{}", std::process::id()));
        let (rt, game) = (base.join("rt"), base.join("game"));
        std::fs::create_dir_all(&rt).unwrap();
        std::fs::create_dir_all(&game).unwrap();
        assert!(!super::fsr_available(&rt, &game));
        std::fs::write(game.join("amd_fidelityfx_upscaler_xbox.dll"), b"x").unwrap();
        assert!(super::fsr_available(&rt, &game));
        let _ = std::fs::remove_dir_all(&base);
    }

    use super::*;

    /// A new folder holding `bytes` as `scarlett.toml`, and the file's path.
    fn toml_file(name: &str, bytes: &[u8]) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("sp-toml-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scarlett.toml");
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn a_damaged_toml_is_set_aside_and_written_new() {
        let keys = Tuning { frame_cap: 60, ..Tuning::default() }.toml_keys(false);
        for (name, bytes) in [("zeros", vec![0u8; 191]), ("garbage", b"\xff\xfe junk [[[".to_vec()), ("text", b"not toml at all\n".to_vec())] {
            let (dir, path) = toml_file(name, &bytes);
            let note = write_toml(&path, &keys).unwrap().expect("a line saying so");
            assert!(note.contains("scarlett.toml.damaged"), "{note}");
            assert_eq!(std::fs::read(dir.join("scarlett.toml.damaged")).unwrap(), bytes);
            let new = std::fs::read_to_string(&path).unwrap();
            assert!(new.contains("[pace]\npace = true\nfps = 60") && !new.contains('\0'), "{new}");
            assert!(toml_text(new.as_bytes()).is_some());
            // nothing is left beside it but the two files
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_byte_order_mark_is_not_damage() {
        let (dir, path) = toml_file("bom", b"\xEF\xBB\xBF[controls]\nmouse_sensitivity = 2.0\n");
        let keys = Tuning::default().toml_keys(false);
        assert_eq!(write_toml(&path, &keys).unwrap(), None);
        let new = std::fs::read_to_string(&path).unwrap();
        assert!(new.contains("[controls]\nmouse_sensitivity = 2.0"), "{new}");
        assert!(!dir.join("scarlett.toml.damaged").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unchanged_toml_is_not_written_again_and_a_missing_one_is() {
        let dir = std::env::temp_dir().join(format!("sp-toml-same-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("scarlett.toml");
        // with no keys, a missing file is still written, as the runtime is pointed at it
        assert_eq!(write_toml(&path, &[]).unwrap(), None);
        assert!(path.is_file());
        let keys = Tuning { frame_cap: 30, ..Tuning::default() }.toml_keys(false);
        write_toml(&path, &keys).unwrap();
        let first = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_toml(&path, &keys).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn defaults_set_nothing() {
        assert!(Tuning::default().env(true).is_empty());
    }

    #[test]
    fn env_mapping() {
        let t = Tuning { full_textures: true, hdr: true, pad_ps: true, rumble_strength: 1.5, kbm_hints: "off".into(), frame_cap: 30, upscaler: "fsr".into(), framegen: true, ..Tuning::default() };
        let e = t.env(true);
        let get = |k: &str| e.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("SCARLETT_TEXTURES"), Some("full"));
        assert_eq!(get("SCARLETT_HDR"), Some("1"));
        assert_eq!(get("SCARLETT_PAD_PS"), Some("1"));
        assert_eq!(get("SCARLETT_RUMBLE_STRENGTH"), Some("1.5"));
        assert_eq!(get("SCARLETT_KBM_HINTS"), Some("off"));
        assert_eq!(get("SCARLETT_PACE_FPS"), Some("30"));
        assert_eq!(get("SCARLETT_UPSCALER"), Some("fsr"));
        assert_eq!(get("SCARLETT_FRAMEGEN"), Some("1"));
        // without the DLL, FSR is not asked for
        assert!(t.env(false).iter().all(|(k, _)| !k.starts_with("SCARLETT_UPSCAL") && *k != "SCARLETT_FRAMEGEN"));
        let off = Tuning { rumble: false, ..Tuning::default() };
        assert_eq!(off.env(false), vec![("SCARLETT_RUMBLE", "0".to_string())]);
    }

    #[test]
    fn new_engine_switches() {
        let t = Tuning { ray_tracing: true, overlay: true, ..Tuning::default() };
        let e = t.env(false);
        let get = |k: &str| e.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("SCARLETT_BVH_BUILDER"), Some("1"));
        assert_eq!(get("SCARLETT_PANEL"), Some("1"));
        assert_eq!(e.len(), 2);
        // Settings saved before these existed load with them off.
        let old: Tuning = serde_json::from_str(r#"{"hdr": true}"#).unwrap();
        assert!(old.hdr && !old.ray_tracing && !old.overlay);
    }

    #[test]
    fn toml_merge_keeps_other_lines() {
        let text = "[paths]\ngame_dir = 'D:\\g'\n\n[controls]\nmouse_sensitivity = 2.0\n\n[game]\nrumble = true\n";
        let keys = Tuning { rumble: false, frame_cap: 60, ..Tuning::default() }.toml_keys(false);
        let out = merge_toml(text, &keys);
        assert!(out.contains("game_dir = 'D:\\g'"));
        assert!(out.contains("mouse_sensitivity = 2.0"));
        assert!(out.contains("rumble = false"));
        assert!(!out.contains("rumble = true"));
        assert!(out.contains("[pace]\npace = true\nfps = 60"));
        assert!(out.contains("[display]\nhdr = false"));
        // idempotent
        assert_eq!(merge_toml(&out, &keys), out);
    }

    #[test]
    fn toml_merge_removes_keys_when_a_feature_is_turned_off() {
        let text = "[paths]\ngame_dir = 'D:\\g'\n\n[controls]\nmouse_sensitivity = 2.0\n";
        let on = Tuning { frame_cap: 60, upscaler: "fsr".into(), framegen: true, ..Tuning::default() }.toml_keys(true);
        let enabled = merge_toml(text, &on);
        assert!(enabled.contains("[upscale]\nupscaler = \"fsr\"\nquality = \"quality\"\nsharpness = 0.2\nframegen = true"));
        assert!(enabled.contains("[pace]\npace = true\nfps = 60"));

        let off = Tuning::default().toml_keys(true);
        let disabled = merge_toml(&enabled, &off);
        for key in ["upscaler", "quality", "sharpness", "framegen"] {
            assert!(!disabled.contains(&format!("{key} =")), "{key} was left behind:\n{disabled}");
        }
        assert!(!disabled.contains("fps ="), "pace.fps was left behind:\n{disabled}");
        assert!(disabled.contains("pace = false"));
        assert!(disabled.contains("game_dir = 'D:\\g'"));
        assert!(disabled.contains("mouse_sensitivity = 2.0"));
        // no stale keys to remove on a second pass
        assert_eq!(merge_toml(&disabled, &off), disabled);
    }

    #[test]
    fn toml_merge_removes_fsr_keys_when_the_dll_is_gone() {
        let on = Tuning { upscaler: "fsr".into(), ..Tuning::default() }.toml_keys(true);
        let enabled = merge_toml("[game]\nrumble = true\n", &on);
        // The launcher stops asking for FSR without the DLL; its old settings must go too.
        let without_dll = Tuning { upscaler: "fsr".into(), ..Tuning::default() }.toml_keys(false);
        let disabled = merge_toml(&enabled, &without_dll);
        assert!(!disabled.contains("upscaler ="));
        assert!(!disabled.contains("framegen ="));
        assert!(disabled.contains("rumble = true"));
    }

    #[test]
    fn toml_merge_is_section_scoped_and_deduplicates() {
        // `[controls] fps` is not the launcher's `pace.fps`; it must survive. A stale quoted
        // key and duplicate lines of a managed key must not survive.
        let text = "[pace]\npace = true # a comment\nfps = 30\n\"fps\" = 60\n\n[controls]\nfps = 30\n";
        let off = Tuning::default().toml_keys(true);
        let out = merge_toml(text, &off);
        let pace = out.split("[pace]\n").nth(1).unwrap().split("\n[").next().unwrap();
        assert!(pace.contains("pace = false"));
        assert!(!pace.contains("fps"), "{pace}");
        assert!(out.contains("[controls]\nfps = 30"));
        assert_eq!(merge_toml(&out, &off), out);

        let on = Tuning { frame_cap: 60, ..Tuning::default() }.toml_keys(true);
        let deduped = merge_toml("[pace]\nfps = 30\nfps = 45\nfps = 60\n", &on);
        assert_eq!(deduped.matches("fps = ").count(), 1, "{deduped}");
        assert!(deduped.contains("fps = 60"));
    }
}
