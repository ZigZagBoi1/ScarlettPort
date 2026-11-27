//! "Verify game files": checks a dump folder for completeness without changing anything.
//! Every expected top-level archive, a complete `pack\` (the title's nine packs), symbolic links
//! that resolve, and no empty files. The dump's `dump-manifest.log` is compared for information
//! only: it often does not describe the files exactly. A `pack\` that is missing or split across drives is called out on its own: the
//! title crashes at boot without it. Reads metadata only; never moves, writes or deletes.
use std::path::{Path, PathBuf};

/// The title's top-level files (archives, the executable and its config).
pub const TOP: [&str; 14] = [
    "game_scarlett_master.exe",
    "MicrosoftGame.config",
    "anim.rpf",
    "appdata0.rpf",
    "audio.rpf",
    "common.rpf",
    "data.rpf",
    "levels.rpf",
    "movies.rpf",
    "navmeshes.rpf",
    "packs.rpf",
    "script.rpf",
    "shaders.rpf",
    "textures.rpf",
];

/// The packs under `pack\`, each a folder with at least one archive named after it.
pub const PACKS: [&str; 9] = [
    "core_bvh4",
    "core_licenses",
    "core_restofworld",
    "core_restofworld001",
    "shared_minigame000",
    "shared_minigame000_bvh4",
    "story000",
    "story000_bvh4",
    "story000_restofworld",
];

pub const MANIFEST: &str = "dump-manifest.log";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Ok,
    Warn,
    Bad,
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub level: Level,
    pub title: String,
    pub detail: String,
    /// What "Show me" opens in Explorer.
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
    /// `pack\` is missing, incomplete or partly elsewhere.
    pub pack_problem: bool,
    pub files: u64,
    pub bytes: u64,
    /// The dump's own file list, compared for information only: dump tools often write a
    /// manifest that does not describe the files exactly, and that never decides the result.
    pub manifest_note: Option<ManifestNote>,
}

/// A calm, collapsed note about `dump-manifest.log`; `details` sit behind an expander.
#[derive(Clone, Debug)]
pub struct ManifestNote {
    pub summary: String,
    pub details: Vec<String>,
}

impl Report {
    pub fn worst(&self) -> Level {
        if self.findings.iter().any(|f| f.level == Level::Bad) {
            Level::Bad
        } else if self.findings.iter().any(|f| f.level == Level::Warn) {
            Level::Warn
        } else {
            Level::Ok
        }
    }
    pub fn problems(&self) -> usize {
        self.findings.iter().filter(|f| f.level != Level::Ok).count()
    }
}

/// One manifest line: `SHA-256 | SIZE | MODIFIED | REL_PATH`.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub size: u64,
    pub path: String,
}

/// The entries and the "Total entries" the header states (a manifest may list only some).
pub fn parse_manifest(text: &str) -> (Vec<Entry>, Option<u64>) {
    let mut total = None;
    let mut out = Vec::new();
    for l in text.lines() {
        let t = l.trim();
        if let Some(rest) = t.strip_prefix('#') {
            if let Some(i) = rest.find("Total entries:") {
                let n: String = rest[i + 14..].trim_start().chars().take_while(|c| c.is_ascii_digit() || *c == ',').filter(|c| *c != ',').collect();
                total = n.parse().ok();
            }
            continue;
        }
        let parts: Vec<&str> = t.split('|').map(str::trim).collect();
        if parts.len() == 4 {
            if let Ok(size) = parts[1].parse() {
                out.push(Entry { size, path: parts[3].replace('/', "\\") });
            }
        }
    }
    (out, total)
}

/// Size of `p` following links; Err says why it cannot be read.
fn resolved_len(p: &Path) -> Result<u64, String> {
    std::fs::metadata(p).map(|m| if m.is_dir() { 0 } else { m.len() }).map_err(|e| e.to_string())
}

fn is_link(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// True when `pack_dir` (or any symlink inside it) resolves into `target_dir`.
fn pack_uses_dir(pack_dir: &Path, target_dir: &Path) -> bool {
    let Ok(canon_target) = std::fs::canonicalize(target_dir) else {
        return false;
    };
    fn scan(dir: &Path, canon_target: &Path, depth: u32) -> bool {
        if let Ok(c) = std::fs::canonicalize(dir) {
            if c.starts_with(canon_target) {
                return true;
            }
        }
        if depth >= 4 {
            return false;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return false };
        for e in rd.flatten() {
            let p = e.path();
            if is_link(&p) {
                if let Ok(c) = std::fs::canonicalize(&p) {
                    if c.starts_with(canon_target) {
                        return true;
                    }
                }
            }
            if e.file_type().is_ok_and(|t| t.is_dir()) && scan(&p, canon_target, depth + 1) {
                return true;
            }
        }
        false
    }
    scan(pack_dir, &canon_target, 0)
}

/// Walks `dir` (following links), counting files and noting empty files and broken links.
fn walk(dir: &Path, rep: &mut Report, empty: &mut Vec<PathBuf>, broken: &mut Vec<(PathBuf, String)>, depth: u32) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if is_link(&p) {
            if let Err(err) = std::fs::metadata(&p) {
                broken.push((p, err.to_string()));
                continue;
            }
        }
        match std::fs::metadata(&p) {
            Ok(m) if m.is_dir() => {
                if depth < 12 {
                    walk(&p, rep, empty, broken, depth + 1);
                }
            }
            Ok(m) => {
                rep.files += 1;
                rep.bytes += m.len();
                if m.len() == 0 {
                    empty.push(p);
                }
            }
            Err(err) => broken.push((p, err.to_string())),
        }
    }
}

/// Folders holding the targets of the dump's top-level links (where a split copy may live).
fn link_homes(dir: &Path) -> Vec<PathBuf> {
    let mut homes: Vec<PathBuf> = Vec::new();
    for n in TOP {
        let p = dir.join(n);
        if is_link(&p) {
            if let Some(h) = std::fs::read_link(&p).ok().and_then(|t| t.parent().map(Path::to_path_buf)) {
                if !homes.contains(&h) {
                    homes.push(h);
                }
            }
        }
    }
    homes
}

pub fn run(dir: &Path) -> Report {
    let mut rep = Report::default();
    let push = |rep: &mut Report, level, title: String, detail: String, path: Option<PathBuf>| rep.findings.push(Finding { level, title, detail, path });

    // ---- top level
    let mut missing = Vec::new();
    let mut top_bad = Vec::new();
    for n in TOP {
        let p = dir.join(n);
        let link = is_link(&p);
        match resolved_len(&p) {
            Ok(0) => top_bad.push((n, "is empty (0 bytes)".to_string())),
            Ok(_) => {}
            Err(e) if link => top_bad.push((n, format!("is a link that does not resolve ({e})"))),
            Err(_) => missing.push(n),
        }
    }
    if missing.is_empty() && top_bad.is_empty() {
        push(&mut rep, Level::Ok, format!("All {} top-level files present", TOP.len()), TOP[2..].join(", "), Some(dir.to_path_buf()));
    }
    if !missing.is_empty() {
        push(&mut rep, Level::Bad, format!("Missing {} top-level file(s)", missing.len()), missing.join(", "), Some(dir.to_path_buf()));
    }
    for (n, why) in top_bad {
        push(&mut rep, Level::Bad, format!("{n} {why}"), "Re-copy it from the console dump, or fix the link.".into(), Some(dir.join(n)));
    }

    // ---- pack\
    let pack = dir.join("pack");
    let pack_ok = std::fs::metadata(&pack).is_ok_and(|m| m.is_dir());
    let elsewhere: Vec<PathBuf> = link_homes(dir)
        .into_iter()
        .map(|h| h.join("pack"))
        .filter(|p| p.is_dir() && !same_dir(p, &pack) && !pack_uses_dir(&pack, p))
        .collect();
    if !pack_ok {
        rep.pack_problem = true;
        let detail = match elsewhere.first() {
            Some(e) => format!("There is no pack\\ here, but one exists at {}. The game crashes at boot without pack\\ in the game folder: copy (or link) it here yourself.", e.display()),
            None => "The game crashes at boot without it. Copy pack\\ from the console dump into the game folder.".into(),
        };
        push(&mut rep, Level::Bad, "pack\\ is missing".into(), detail, Some(elsewhere.first().cloned().unwrap_or_else(|| dir.to_path_buf())));
    } else {
        let mut absent = Vec::new();
        for n in PACKS {
            let d = pack.join(n);
            let has_archive = std::fs::read_dir(&d).is_ok_and(|rd| {
                rd.flatten().any(|e| {
                    let f = e.file_name().to_string_lossy().to_lowercase();
                    f.starts_with(n) && f.ends_with(".rpf") && resolved_len(&e.path()).is_ok_and(|l| l > 0)
                })
            });
            if !has_archive {
                absent.push(n);
            }
        }
        let extra: Vec<&PathBuf> = elsewhere.iter().collect();
        if absent.is_empty() {
            push(&mut rep, Level::Ok, format!("pack\\ complete: all {} packs", PACKS.len()), PACKS.join(", "), Some(pack.clone()));
            if let Some(e) = extra.first() {
                push(&mut rep, Level::Warn, "A second pack\\ exists elsewhere".into(), format!("{} also has a pack\\ folder. The game reads only the one in the game folder; the other is unused.", e.display()), Some((*e).clone()));
            }
        } else {
            rep.pack_problem = true;
            let split = extra.first().map(|e| format!(" Part of it is at {}: the pack\\ is split across folders.", e.display())).unwrap_or_default();
            push(
                &mut rep,
                Level::Bad,
                format!("pack\\ incomplete: {} of {} packs missing", absent.len(), PACKS.len()),
                format!("Missing {}.{split} The game crashes at boot until pack\\ is whole in the game folder.", absent.join(", ")),
                Some(extra.first().map(|e| (*e).clone()).unwrap_or_else(|| pack.clone())),
            );
        }
    }

    // ---- every file: empty files and broken links (the port's own _native_port folder is not game data)
    let mut empty = Vec::new();
    let mut broken = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_lowercase();
            if name == "_native_port" {
                continue;
            }
            match std::fs::metadata(&p) {
                Ok(m) if m.is_dir() => walk(&p, &mut rep, &mut empty, &mut broken, 0),
                Ok(m) => {
                    rep.files += 1;
                    rep.bytes += m.len();
                    if m.len() == 0 && !TOP.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
                        empty.push(p);
                    }
                }
                Err(err) if !TOP.iter().any(|t| t.eq_ignore_ascii_case(&name)) => broken.push((p, err.to_string())),
                Err(_) => {}
            }
        }
    }
    if broken.is_empty() {
        push(&mut rep, Level::Ok, "Links resolve".into(), "Every symbolic link points at a file that exists.".into(), None);
    } else {
        let list: Vec<String> = broken.iter().take(6).map(|(p, _)| rel(dir, p)).collect();
        push(&mut rep, Level::Bad, format!("{} link(s) do not resolve", broken.len()), list.join(", "), broken.first().map(|(p, _)| p.clone()));
    }
    // An empty archive or library is truncated; other empty files (logs, markers) are normal.
    let needed = |p: &PathBuf| p.extension().is_some_and(|e| ["rpf", "dll", "exe", "bin"].iter().any(|x| e.eq_ignore_ascii_case(x)));
    let (bad_empty, other_empty): (Vec<PathBuf>, Vec<PathBuf>) = empty.into_iter().partition(needed);
    if bad_empty.is_empty() {
        let mut d = format!("{} files, {}", rep.files, crate::game::human(rep.bytes));
        if !other_empty.is_empty() {
            d += &format!(" ({} empty non-archive file(s), which is normal)", other_empty.len());
        }
        push(&mut rep, Level::Ok, "No truncated archives".into(), d, None);
    } else {
        let list: Vec<String> = bad_empty.iter().take(6).map(|p| rel(dir, p)).collect();
        push(&mut rep, Level::Warn, format!("{} empty archive(s)", bad_empty.len()), format!("{} (0 bytes: re-copy from the console dump)", list.join(", ")), bad_empty.first().cloned());
    }

    // ---- the dump's manifest: advisory only. The checks above decide the result.
    let mp = dir.join(MANIFEST);
    if let Ok(text) = std::fs::read_to_string(&mp) {
        let (entries, total) = parse_manifest(&text);
        let mut gone = Vec::new();
        let mut sized = Vec::new();
        for e in &entries {
            match resolved_len(&dir.join(&e.path)) {
                Err(_) => gone.push(e.path.clone()),
                Ok(l) if l != e.size => sized.push(format!("{} ({} here, {} listed)", e.path, crate::game::human(l), crate::game::human(e.size))),
                Ok(_) => {}
            }
        }
        if !gone.is_empty() || !sized.is_empty() {
            let mut details = Vec::new();
            if total.is_some_and(|t| t > entries.len() as u64) {
                details.push(format!("{} lists {} of its {} entries in this file.", MANIFEST, entries.len(), total.unwrap_or(0)));
            }
            if !gone.is_empty() {
                details.push(format!("Listed but not in the folder ({}): {}", gone.len(), gone.join(", ")));
            }
            if !sized.is_empty() {
                details.push(format!("Listed with another size ({}): {}", sized.len(), sized.join(", ")));
            }
            rep.manifest_note = Some(ManifestNote {
                summary: "Your dump's file list doesn't match these files exactly. That's common and doesn't affect the game.".into(),
                details,
            });
        }
    }
    rep
}

fn rel(base: &Path, p: &Path) -> String {
    p.strip_prefix(base).map(|r| r.display().to_string()).unwrap_or_else(|_| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sp-verify-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn full_dump(d: &Path) {
        for n in TOP {
            std::fs::write(d.join(n), b"x").unwrap();
        }
        for n in PACKS {
            std::fs::create_dir_all(d.join("pack").join(n)).unwrap();
            std::fs::write(d.join("pack").join(n).join(format!("{n}.rpf")), b"x").unwrap();
        }
    }

    #[test]
    fn manifest_parse() {
        let t = "# Total entries: 24,484  |  Total size: 1 bytes\n#\nabc | 12 | 2026 | pack/story000/story000.rpf\nnot a line\n";
        let (e, total) = parse_manifest(t);
        assert_eq!(total, Some(24484));
        assert_eq!(e, vec![Entry { size: 12, path: "pack\\story000\\story000.rpf".into() }]);
    }

    #[test]
    fn complete_dump_passes() {
        let d = tmp("ok");
        full_dump(&d);
        std::fs::write(d.join(MANIFEST), "a | 1 | t | pack\\core_bvh4\\core_bvh4.rpf\n").unwrap();
        let r = run(&d);
        assert_eq!(r.worst(), Level::Ok, "{:?}", r.findings);
        assert!(!r.pack_problem);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn flags_missing_pack_and_archive() {
        let d = tmp("nopack");
        full_dump(&d);
        std::fs::remove_dir_all(d.join("pack").join("story000")).unwrap();
        std::fs::remove_file(d.join("levels.rpf")).unwrap();
        std::fs::write(d.join("empty.bin"), b"").unwrap();
        std::fs::write(d.join("note.txt"), b"").unwrap();
        let r = run(&d);
        assert!(r.pack_problem);
        assert_eq!(r.worst(), Level::Bad);
        assert!(r.findings.iter().any(|f| f.title.contains("pack\\ incomplete") && f.detail.contains("story000")));
        assert!(r.findings.iter().any(|f| f.title.contains("top-level") && f.detail.contains("levels.rpf")));
        assert!(r.findings.iter().any(|f| f.title.contains("1 empty archive")));
        std::fs::remove_dir_all(d.join("pack")).unwrap();
        assert!(run(&d).findings.iter().any(|f| f.title == "pack\\ is missing"));
        let _ = std::fs::remove_dir_all(d);
    }

    /// Shaped like a real dump tool's manifest: partial, listing files that are not there and
    /// sizes that differ, nothing under pack\. All synthetic.
    const LOOSE_MANIFEST: &str = r"# Dump Manifest
# Total entries: 1,200  |  Total size: 9 bytes
#
# Format: SHA-256 | SIZE_BYTES | MODIFIED_UTC | REL_PATH
aa | 288000000 | 2026-01-01T00:00:00Z | game_scarlett_master.exe
bb | 12441 | 2026-01-01T00:00:00Z | appxmanifest.xml
cc | 8206 | 2026-01-01T00:00:00Z | MicrosoftGame.config
dd | 8589934592 | 2026-01-01T00:00:00Z | data\levels\world_main.rpf
ee | 4194304 | 2026-01-01T00:00:00Z | era\runtime\runtime_bridge.dll
ff | 83886080 | 2026-01-01T00:00:00Z | saves\profile_player.sav
# ... 1,194 additional entries omitted
";

    #[test]
    fn loose_manifest_is_only_a_note() {
        let d = tmp("loose");
        full_dump(&d);
        std::fs::write(d.join("appxmanifest.xml"), b"<x/>").unwrap();
        std::fs::write(d.join(MANIFEST), LOOSE_MANIFEST).unwrap();
        let r = run(&d);
        assert_eq!(r.worst(), Level::Ok, "{:?}", r.findings);
        assert_eq!(r.problems(), 0);
        assert!(!r.pack_problem);
        assert!(!r.findings.iter().any(|f| f.title.contains("manifest")));
        let n = r.manifest_note.expect("a calm note");
        assert!(n.summary.contains("doesn't affect the game"));
        let all = n.details.join(" ");
        assert!(all.contains("world_main.rpf") && all.contains("profile_player.sav"));
        assert!(all.contains("Listed but not in the folder (3)") && all.contains("another size (3)"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn loose_manifest_still_flags_a_truncated_archive() {
        let d = tmp("trunc");
        full_dump(&d);
        std::fs::write(d.join("levels.rpf"), b"").unwrap();
        std::fs::write(d.join(MANIFEST), LOOSE_MANIFEST).unwrap();
        let r = run(&d);
        assert_eq!(r.worst(), Level::Bad);
        assert!(r.findings.iter().any(|f| f.title.contains("levels.rpf") && f.title.contains("0 bytes")));
        let _ = std::fs::remove_dir_all(d);
    }

    /// Runs on a real dump when `SP_VERIFY_DIR` names one (not part of CI).
    #[test]
    #[ignore]
    fn real_dump() {
        let Some(d) = std::env::var_os("SP_VERIFY_DIR") else { return };
        let r = run(Path::new(&d));
        for f in &r.findings {
            eprintln!("{:?} {} :: {}", f.level, f.title, f.detail);
        }
        eprintln!("note: {:?}", r.manifest_note);
        assert_eq!(r.worst(), Level::Ok);
    }

    #[test]
    fn no_manifest_is_fine() {
        let d = tmp("nomani");
        full_dump(&d);
        let r = run(&d);
        assert_eq!(r.worst(), Level::Ok, "{:?}", r.findings);
        assert!(r.manifest_note.is_none());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn never_writes() {
        let d = tmp("ro");
        full_dump(&d);
        let before: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
        run(&d);
        let after: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(before, after);
        let _ = std::fs::remove_dir_all(d);
    }

    #[cfg(windows)]
    #[test]
    fn split_storage_symlinks_pass_without_second_pack_warning() {
        let d = tmp("split-game");
        let ssd = tmp("split-ssd");
        full_dump(&d);
        std::fs::create_dir_all(ssd.join("pack").join("core_licenses")).unwrap();
        std::fs::write(ssd.join("shaders.rpf"), b"shaders").unwrap();
        std::fs::write(ssd.join("pack").join("core_licenses").join("core_licenses.rpf"), b"lic").unwrap();
        std::fs::remove_file(d.join("shaders.rpf")).unwrap();
        std::fs::remove_file(d.join("pack").join("core_licenses").join("core_licenses.rpf")).unwrap();
        if std::os::windows::fs::symlink_file(ssd.join("shaders.rpf"), d.join("shaders.rpf")).is_ok()
            && std::os::windows::fs::symlink_file(
                ssd.join("pack").join("core_licenses").join("core_licenses.rpf"),
                d.join("pack").join("core_licenses").join("core_licenses.rpf"),
            )
            .is_ok()
        {
            let r = run(&d);
            assert_eq!(r.problems(), 0, "{:?}", r.findings);
            assert_eq!(r.worst(), Level::Ok);
        }
        let _ = std::fs::remove_dir_all(d);
        let _ = std::fs::remove_dir_all(ssd);
    }
}
