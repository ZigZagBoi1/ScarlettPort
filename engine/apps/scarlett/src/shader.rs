//! Survey shader containers: bitcode, console additions and native code.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;
use scarlett_game::sgd5;
use scarlett_shader::gfx10::{self, Instruction};
use scarlett_shader::{Container, DxilProgram, FourCC, HwStage, ShaderError, native_programs};

/// Files above this size are skipped (read whole into memory for now).
pub(crate) const MAX_FILE: u64 = 2 << 30;

/// A shader container in a file: as is, or packed in an SGD5 record.
pub(crate) enum Source<'a> {
    Raw { offset: usize, data: &'a [u8] },
    Packed(sgd5::Record<'a>),
}

impl Source<'_> {
    /// The container bytes; `None` if a packed record is corrupt.
    pub(crate) fn bytes(&self) -> Option<Cow<'_, [u8]>> {
        match self {
            Source::Raw { data, .. } => Some(Cow::Borrowed(data)),
            Source::Packed(r) => r.unpack().map(Cow::Owned),
        }
    }

    pub(crate) fn label(&self) -> String {
        match self {
            Source::Raw { offset, .. } => format!("@{offset:#x}"),
            Source::Packed(r) => format!("@{:#x} {}", r.offset, r.name.unwrap_or("?")),
        }
    }
}

/// The packed records of SGD5 data, and every container stored as is (the
/// executable keeps some records uncompressed), found by its magic.
pub(crate) fn sources(data: &[u8]) -> Vec<Source<'_>> {
    let mut out: Vec<Source<'_>> = Vec::new();
    if sgd5::is_sgd5(data) {
        out.extend(sgd5::records(data).map(Source::Packed));
    }
    let raw = memchr::memmem::find_iter(data, b"DXBC")
        .filter_map(|offset| {
            let tail = &data[offset..];
            // DXBC total size, validated below. Hash the container, not the
            // rest of its carrier, so offline warming uses the runtime key.
            let size = u32::from_le_bytes(tail.get(0x18..0x1c)?.try_into().ok()?) as usize;
            let data = tail.get(..size)?;
            Container::parse(data).ok()?;
            Some(Source::Raw { offset, data })
        });
    out.extend(raw);
    out
}

#[cfg(test)]
mod tests {
    use super::{read_shader_carrier, shader_files, sources};

    #[test]
    fn embedded_containers_keep_runtime_hashes() {
        // A 32-byte DXBC with no parts; the scan should not add carrier bytes.
        let shader = b"DXBC\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x01\0\0\0\x20\0\0\0\0\0\0\0";
        let first = [b"MZ".as_slice(), shader, b"first suffix"].concat();
        let second = [b"PE header".as_slice(), shader, b"other suffix", b"DXBC"].concat();
        let runtime_hash = blake3::hash(shader);
        for (carrier, label) in [(&first, "@0x2"), (&second, "@0x9")] {
            let entries = sources(carrier);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].label(), label);
            assert_eq!(blake3::hash(&entries[0].bytes().unwrap()), runtime_hash);
        }
    }

    #[cfg(windows)]
    #[test]
    fn walkdir_follows_symlink_files() {
        let base = std::env::temp_dir().join(format!("scarlett-symlink-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ssd = base.join("ssd");
        let game = base.join("game");
        std::fs::create_dir_all(&ssd).unwrap();
        std::fs::create_dir_all(&game).unwrap();
        let target = ssd.join("shaders.rpf");
        let link = game.join("shaders.rpf");
        std::fs::write(&target, b"DXBC\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x01\0\0\0\x20\0\0\0\0\0\0\0").unwrap();
        if std::os::windows::fs::symlink_file(&target, &link).is_ok() {
            let found: Vec<_> = walkdir::WalkDir::new(&link)
                .follow_links(true)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_file())
                .collect();
            assert_eq!(found.len(), 1, "direct symlink to file should be discovered");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn shader_files_prioritizes_shaders_and_skips_non_shader_assets() {
        let base = std::env::temp_dir().join(format!("scarlett-scan-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("data")).unwrap();
        std::fs::write(base.join("data").join("textures.ttpf"), b"RDS5\0\0\0\0").unwrap();
        std::fs::write(base.join("other.rpf"), vec![0u8; 64]).unwrap();
        std::fs::write(base.join("app.exe"), vec![0u8; 64]).unwrap();
        std::fs::write(base.join("shaders.rpf"), vec![0u8; 128]).unwrap();

        let files = shader_files(&[base.clone()], false);
        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["shaders.rpf", "app.exe", "other.rpf"]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn read_shader_carrier_skips_rds5_texture_databases() {
        let base = std::env::temp_dir().join(format!("scarlett-rds5-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let db = base.join("renamed.bin");
        std::fs::write(&db, [b"RDS5".as_slice(), &[0u8; 64]].concat()).unwrap();
        assert!(read_shader_carrier(&db).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[derive(Default)]
struct Totals {
    containers: usize,
    with_dxil_part: usize,
    with_bitcode: usize,
    standard_layout: usize,
    xdat: usize,
    xbop: usize,
    kinds: BTreeMap<String, usize>,
    with_native: usize,
    native_stages: BTreeMap<HwStage, usize>,
    native_errors: BTreeMap<String, usize>,
    decoded: usize,
    instructions: usize,
    unnamed: BTreeMap<String, usize>,
    decode_errors: BTreeMap<&'static str, usize>,
    files_with_shaders: usize,
}

/// Extensions that hold textures, media, metadata, or OS images and never contain shaders.
const NON_SHADER_EXTS: &[&str] = &[
    "ttpf",
    "xvd",
    "png",
    "jpg",
    "jpeg",
    "dds",
    "xml",
    "json",
    "txt",
    "log",
    "pri",
    "alignmentchunk",
    "pdb",
    "bik",
    "bk2",
];

fn is_non_shader_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| NON_SHADER_EXTS.iter().any(|n| ext.eq_ignore_ascii_case(n)))
}

fn has_shader_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().contains("shader"))
}

fn scan_priority(path: &Path, len: u64) -> (u8, u64) {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let tier = if has_shader_name(path) || matches!(ext.as_str(), "cdxil" | "dxbc" | "sgd5") {
        0
    } else if matches!(ext.as_str(), "exe" | "dll") {
        1
    } else {
        2
    };
    (tier, len)
}

/// Collects candidate files for shader scanning, following symlinks, skipping known
/// non-shader asset databases when walking directories, deduplicating by canonical path,
/// and ordering shader packs and executables first so progress starts immediately.
pub(crate) fn shader_files(paths: &[PathBuf], report_large: bool) -> Vec<PathBuf> {
    shader_files_until(paths, report_large, || false)
}

/// As `shader_files`, but discards partial results if discovery is stopped.
pub(crate) fn shader_files_until(
    paths: &[PathBuf],
    report_large: bool,
    stop: impl Fn() -> bool,
) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut candidates: Vec<(u8, u64, PathBuf)> = Vec::new();
    for root in paths {
        let mut entries = walkdir::WalkDir::new(root).follow_links(true).into_iter();
        loop {
            if stop() {
                return Vec::new();
            }
            let Some(entry) = entries.next() else { break };
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if entry.depth() > 0 && is_non_shader_ext(path) {
                continue;
            }
            let len = entry.metadata().map_or(0, |m| m.len());
            if len == 0 {
                continue;
            }
            if len > MAX_FILE {
                if report_large {
                    eprintln!("skipping large file {}", path.display());
                }
                continue;
            }
            let identity = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            if !seen.insert(identity) {
                continue;
            }
            let (tier, size) = scan_priority(path, len);
            candidates.push((tier, size, path.to_path_buf()));
        }
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    candidates.into_iter().map(|(_, _, p)| p).collect()
}

fn window_has_shader_magic(buf: &[u8]) -> bool {
    if memchr::memmem::find(buf, sgd5::MAGIC).is_some() {
        return true;
    }
    for offset in memchr::memmem::find_iter(buf, b"DXBC") {
        let Some(hdr) = buf.get(offset..offset + 0x20) else {
            continue;
        };
        let ver_major = u16::from_le_bytes([hdr[0x14], hdr[0x15]]);
        let total = u32::from_le_bytes([hdr[0x18], hdr[0x19], hdr[0x1a], hdr[0x1b]]);
        let count = u32::from_le_bytes([hdr[0x1c], hdr[0x1d], hdr[0x1e], hdr[0x1f]]);
        if ver_major <= 1 && (0x20..=(64 << 20)).contains(&total) && count <= 256 {
            return true;
        }
    }
    false
}

/// Reads `path` into memory if it can contain shaders, using a bounded streaming sniff on
/// large non-shader-named archives to avoid allocating multi-hundred-MB buffers for asset packs.
pub(crate) fn read_shader_carrier(path: &Path) -> Option<Vec<u8>> {
    const STREAM_THRESHOLD: u64 = 32 << 20;
    const CHUNK: usize = 8 << 20;
    const OVERLAP: usize = 0x20;

    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if !(0x20..=MAX_FILE).contains(&len) {
        return None;
    }
    let mut head = [0u8; 4];
    if file.read_exact(&mut head).is_ok() && &head == b"RDS5" {
        return None;
    }
    drop(file);

    if len <= STREAM_THRESHOLD || has_shader_name(path) {
        return std::fs::read(path).ok();
    }

    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; CHUNK + OVERLAP];
    let mut carry = 0usize;
    loop {
        let n = file.read(&mut buf[carry..]).ok()?;
        if n == 0 {
            return None;
        }
        let valid = carry + n;
        if window_has_shader_magic(&buf[..valid]) {
            drop(file);
            return std::fs::read(path).ok();
        }
        if valid > OVERLAP {
            buf.copy_within(valid - OVERLAP..valid, 0);
            carry = OVERLAP;
        } else {
            carry = valid;
        }
    }
}

pub fn run(paths: Vec<PathBuf>, verbose: bool, disasm: bool) -> Result<()> {
    let paths = crate::default_paths(paths)?;
    let mut t = Totals::default();
    for path in shader_files(&paths, true) {
        let Some(data) = read_shader_carrier(&path) else {
            continue;
        };
        let mut found_here = 0;
        for source in sources(&data) {
            let Some(bytes) = source.bytes() else {
                eprintln!("corrupt record {}{}", path.display(), source.label());
                continue;
            };
            let Ok(container) = Container::parse(&bytes) else {
                continue;
            };
            found_here += 1;
            t.containers += 1;
            let dxil = container
                .part(FourCC::DXIL)
                .or(container.part(FourCC::ACCC));
            t.with_dxil_part += usize::from(dxil.is_some());
            let program = dxil.and_then(|p| DxilProgram::parse(p.data).ok());
            let mut listing = Vec::new();
            // Independent of the bitcode: titles may strip it.
            let native = match dxil.map(|p| native_programs(p.data)) {
                Some(Ok(programs)) if !programs.is_empty() => {
                    t.with_native += 1;
                    let mut list = Vec::new();
                    for n in &programs {
                        *t.native_stages.entry(n.stage).or_default() += 1;
                        list.push(format!("{:?} {} B", n.stage, n.code.len()));
                        if disasm {
                            listing.push(format!("    {:?} stage I/O: {:?}", n.stage, n.io));
                            listing.extend(n.layout.iter().map(|p| format!("    {p:?}")));
                            listing.extend(n.used.iter().map(|u| format!("    {u:?}")));
                        }
                        match gfx10::decode(&n.code) {
                            Ok(insts) => {
                                t.decoded += 1;
                                t.instructions += insts.len();
                                for i in insts.iter().filter(|i| i.name().is_none()) {
                                    let key = format!("{:?} {:#x}", i.encoding, i.opcode);
                                    *t.unnamed.entry(key).or_default() += 1;
                                }
                                if disasm {
                                    listing.extend(
                                        insts.iter().map(|i| format_instruction(i, n.wave64)),
                                    );
                                }
                            }
                            Err(e) => {
                                let reason = match e {
                                    ShaderError::Isa { reason, .. } => reason,
                                    _ => "other",
                                };
                                *t.decode_errors.entry(reason).or_default() += 1;
                                list.push(format!("({e})"));
                            }
                        }
                    }
                    format!(", native {}", list.join(" "))
                }
                Some(Err(e)) => {
                    *t.native_errors.entry(e.to_string()).or_default() += 1;
                    format!(", native: {e}")
                }
                _ => String::new(),
            };
            let line = match &program {
                Some(p) => {
                    t.with_bitcode += 1;
                    let x = p.xbox_extras();
                    t.standard_layout += usize::from(p.is_standard_layout());
                    t.xdat += usize::from(x.xdat);
                    t.xbop += usize::from(x.xbop);
                    *t.kinds.entry(format!("{:?}", p.kind)).or_default() += 1;
                    format!(
                        "{:?} sm{}.{} bitcode {} B, xdat {}, xbop {}",
                        p.kind,
                        p.shader_model.0,
                        p.shader_model.1,
                        p.bitcode.len(),
                        x.xdat,
                        x.xbop
                    )
                }
                None => "no parsable DXIL program".to_owned(),
            } + &native;
            if verbose {
                // As a run's log and a replay name it (`hash=`).
                let hash = blake3::hash(&bytes).to_hex();
                println!(
                    "{}{} #{}: {line}",
                    path.display(),
                    source.label(),
                    &hash[..16]
                );
                for l in &listing {
                    println!("{l}");
                }
            }
        }
        t.files_with_shaders += usize::from(found_here > 0);
    }
    println!(
        "containers found       {} (in {} files)",
        t.containers, t.files_with_shaders
    );
    println!("  with DXIL part       {}", t.with_dxil_part);
    println!("  with LLVM bitcode    {}", t.with_bitcode);
    println!("  PC-standard layout   {}", t.standard_layout);
    println!("  XDAT prefix          {}", t.xdat);
    println!("  xbop trailer         {}", t.xbop);
    for (k, n) in &t.kinds {
        println!("  {k:<20} {n}");
    }
    println!("  with native code     {}", t.with_native);
    for (stage, n) in &t.native_stages {
        println!("    {:<18} {n}", format!("{stage:?}"));
    }
    for (e, n) in &t.native_errors {
        println!("  native unreadable    {n}: {e}");
    }
    if t.with_native > 0 {
        println!(
            "  decoded programs     {} ({} instructions)",
            t.decoded, t.instructions
        );
        for (e, n) in &t.decode_errors {
            println!("  decode failed        {n}: {e}");
        }
        let mut unnamed: Vec<_> = t.unnamed.iter().collect();
        unnamed.sort_by_key(|&(_, n)| std::cmp::Reverse(*n));
        for (op, n) in unnamed.iter().take(10) {
            println!("  opcode not in table  {op} x{n}");
        }
    }
    if t.containers == 0 {
        println!("(none found: shaders may sit in compressed or encrypted archives)");
    }
    Ok(())
}

fn format_instruction(i: &Instruction, wave64: bool) -> String {
    let words: Vec<_> = i.words.iter().map(|w| format!("{w:08X}")).collect();
    let text = match i.name() {
        Some(_) => i.text(wave64),
        None => format!("<{:?} {:#x}>", i.encoding, i.opcode),
    };
    format!("    {:06x}: {:<44} {text}", i.offset, words.join(" "))
}
