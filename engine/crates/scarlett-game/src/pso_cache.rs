//! The title's precompiled pipelines: `platform:/shaders/v2.d3d12PipelineCache`,
//! the last file of the shader archive (`shaders.rpf`), stored as a chain
//! of zlib streams of 64 KiB each. Its records are what the title hands the
//! console device's `DeserializeGraphicsPipelineStateX` and
//! `DeserializeComputePipelineStateX` (device slots 0x1c8 and 0x218):
//!
//! ```text
//! +0x00 u32 0x09005350, char[32] build, char[32] driver,
//!       u64, then u32 counts: graphics records (0x70 B), compute records
//!       (0x18 B), blocks, codes, block bytes, code bytes; 0x80 bytes in all
//! graphics u64 key, u32 x2 state blocks, u64 x5 hashes, 7 stage slots of
//!          (u32 code, u32 metadata block), 0xffffffff where unused
//! compute  u64 key, u32 x2 state blocks, u32 code, u32 metadata block
//! blocks   (u32 offset, u32 size) into the block bytes; codes likewise
//!          into the code bytes (0x100-aligned; the title copies them into
//!          GPU memory)
//! ```
//!
//! Stage slots, by the programs they hold: 0 pixel, 1 vertex, 2 mesh; the
//! others are not seen holding a known stage. The code is native GFX10
//! compiled for the pipeline (not the archive's programs), but it names its
//! source: the first six dwords are two VOP3 instructions whose operands
//! spell the container's `HASH` digest ([`code_shader_hash`]); the
//! metadata block ends with the same digest (entry 0x43,
//! [`meta_shader_hash`]). The state blocks are decoded by
//! [`crate::pso_state`].
//!
//! (Ported from ScarlettPort before the 0.4.6 engine swap, `pso_cache.rs`
//! at 424fc91^, with zlib-rs in place of miniz_oxide.)

/// The file's first word.
pub const MAGIC: u32 = 0x0900_5350;
const HEADER: usize = 0x80;
const GRAPHICS_RECORD: usize = 0x70;
const COMPUTE_RECORD: usize = 0x18;
const NONE: u32 = u32::MAX;
/// Stage slots of a graphics record.
pub const GRAPHICS_SLOTS: usize = 7;
/// The file is ~300 MB; refuse headers that claim much more.
const MAX_FILE: usize = 2 << 30;

/// The stage a graphics stage slot holds, as the pipeline code names
/// stages; `None` for slots not seen holding a known stage.
pub fn slot_stage(slot: usize) -> Option<&'static str> {
    match slot {
        0 => Some("pixel"),
        1 => Some("vertex"),
        2 => Some("mesh"),
        _ => None,
    }
}

/// A parsed cache file (borrowing its bytes).
#[derive(Debug, Clone, Copy)]
pub struct Cache<'a> {
    pub build: &'a str,
    pub driver: &'a str,
    graphics: &'a [u8],
    compute: &'a [u8],
    blocks: &'a [u8],
    codes: &'a [u8],
    block_bytes: &'a [u8],
    code_bytes: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Graphics {
    pub key: u64,
    pub state: [u32; 2],
    pub hashes: [u64; 5],
    /// Per slot: (code, metadata block), indices into the tables.
    pub stages: [Option<(u32, u32)>; GRAPHICS_SLOTS],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compute {
    pub key: u64,
    pub state: [u32; 2],
    pub code: u32,
    pub meta: u32,
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o.checked_add(4)?)
        .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
}

fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    b.get(o..o.checked_add(8)?)
        .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
}

fn text(b: &[u8]) -> &str {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    std::str::from_utf8(&b[..end]).unwrap_or("")
}

/// The table sizes a header gives, in its order; `None` if they overflow.
fn sizes(header: &[u8]) -> Option<[usize; 6]> {
    let count = |i: usize| u32_at(header, 0x50 + 4 * i).map(|c| c as usize);
    Some([
        count(0)?.checked_mul(GRAPHICS_RECORD)?,
        count(1)?.checked_mul(COMPUTE_RECORD)?,
        count(2)?.checked_mul(8)?,
        count(3)?.checked_mul(8)?,
        count(4)?,
        count(5)?,
    ])
}

impl<'a> Cache<'a> {
    /// The file, if its header and tables fit it exactly.
    pub fn parse(file: &'a [u8]) -> Result<Cache<'a>, &'static str> {
        if file.len() < HEADER || u32_at(file, 0) != Some(MAGIC) {
            return Err("not a pipeline cache");
        }
        let sizes = sizes(file).ok_or("pipeline cache tables overflow")?;
        if Cache::size_of(file) != Some(file.len()) {
            return Err("pipeline cache tables do not add up to its size");
        }
        let mut at = HEADER;
        let mut take = |n: usize| {
            let part = &file[at..at + n];
            at += n;
            part
        };
        Ok(Cache {
            build: text(&file[4..0x24]),
            driver: text(&file[0x24..0x44]),
            graphics: take(sizes[0]),
            compute: take(sizes[1]),
            blocks: take(sizes[2]),
            codes: take(sizes[3]),
            block_bytes: take(sizes[4]),
            code_bytes: take(sizes[5]),
        })
    }

    /// The total size the header of a file says it has (to know when a
    /// chain of streams is complete).
    pub fn size_of(header: &[u8]) -> Option<usize> {
        if u32_at(header, 0)? != MAGIC {
            return None;
        }
        let total = sizes(header)?
            .iter()
            .try_fold(HEADER, |sum, &s| sum.checked_add(s))?;
        (total <= MAX_FILE).then_some(total)
    }

    pub fn graphics_count(&self) -> usize {
        self.graphics.len() / GRAPHICS_RECORD
    }

    pub fn compute_count(&self) -> usize {
        self.compute.len() / COMPUTE_RECORD
    }

    pub fn graphics(&self, i: usize) -> Option<Graphics> {
        let r = self
            .graphics
            .get(i * GRAPHICS_RECORD..(i + 1) * GRAPHICS_RECORD)?;
        let mut stages = [None; GRAPHICS_SLOTS];
        for (s, slot) in stages.iter_mut().enumerate() {
            let (code, meta) = (u32_at(r, 0x38 + 8 * s)?, u32_at(r, 0x3c + 8 * s)?);
            *slot = (code != NONE).then_some((code, meta));
        }
        Some(Graphics {
            key: u64_at(r, 0)?,
            state: [u32_at(r, 8)?, u32_at(r, 12)?],
            hashes: [0, 1, 2, 3, 4].map(|k| u64_at(r, 0x10 + 8 * k).expect("in the record")),
            stages,
        })
    }

    pub fn compute(&self, i: usize) -> Option<Compute> {
        let r = self
            .compute
            .get(i * COMPUTE_RECORD..(i + 1) * COMPUTE_RECORD)?;
        Some(Compute {
            key: u64_at(r, 0)?,
            state: [u32_at(r, 8)?, u32_at(r, 12)?],
            code: u32_at(r, 16)?,
            meta: u32_at(r, 20)?,
        })
    }

    /// A state or metadata block.
    pub fn block(&self, i: u32) -> Option<&'a [u8]> {
        slice(self.blocks, self.block_bytes, i)
    }

    /// A stage's native code.
    pub fn code(&self, i: u32) -> Option<&'a [u8]> {
        slice(self.codes, self.code_bytes, i)
    }
}

fn slice<'a>(table: &[u8], bytes: &'a [u8], i: u32) -> Option<&'a [u8]> {
    let at = 8 * i as usize;
    let (offset, size) = (u32_at(table, at)? as usize, u32_at(table, at + 4)? as usize);
    bytes.get(offset..offset.checked_add(size)?)
}

/// The shader digest a native program's first two instructions spell
/// (`v_nop`-like VOP3 instructions with a literal: low half of the first
/// dword, then the low bytes of `src0` and `src1`, then the literal).
pub fn code_shader_hash(code: &[u8]) -> Option<[u8; 16]> {
    let w: Vec<u32> = (0..6).map(|i| u32_at(code, 4 * i)).collect::<Option<_>>()?;
    if w[0] >> 16 != 0xd580 || w[3] >> 16 != 0xd580 {
        return None;
    }
    let half = |a: u32, b: u32| (a & 0xffff) | (b & 0xff) << 16 | (b >> 9 & 0xff) << 24;
    let mut out = [0u8; 16];
    for (i, d) in [half(w[0], w[1]), w[2], half(w[3], w[4]), w[5]]
        .into_iter()
        .enumerate()
    {
        out[4 * i..4 * i + 4].copy_from_slice(&d.to_le_bytes());
    }
    Some(out)
}

/// The shader digest a metadata block ends with (entry 0x43). Graphics
/// blocks have a 16-byte head (u32 2, u32 size used, two words), compute
/// blocks a 24-byte head (u32 0x403, ...); entries 0x41 (descriptor ranges
/// read) and 0x42 (user-data layout) are 0x20 bytes, 0x43 is 0x14.
pub fn meta_shader_hash(meta: &[u8]) -> Option<[u8; 16]> {
    let mut at = match u32_at(meta, 0)? {
        2 => 16,
        0x403 => 24,
        _ => return None,
    };
    loop {
        match u32_at(meta, at)? {
            0x41 | 0x42 => at += 0x20,
            0x43 => return meta.get(at + 4..at + 20)?.try_into().ok(),
            _ => return None,
        }
    }
}

/// The digest a stage names: its metadata's, else its code's; `None` if
/// neither has one or they disagree.
pub fn stage_shader_hash(code: &[u8], meta: &[u8]) -> Option<[u8; 16]> {
    match (meta_shader_hash(meta), code_shader_hash(code)) {
        (Some(m), Some(c)) if m != c => None,
        (Some(h), _) | (None, Some(h)) => Some(h),
        (None, None) => None,
    }
}

/// The digest of a shader container's `HASH` part (after its u32 of
/// flags): the identity the cache's stages name their source by. `None` if
/// the container has no such part.
pub fn container_shader_hash(container: &[u8]) -> Option<[u8; 16]> {
    if container.get(..4) != Some(b"DXBC") {
        return None;
    }
    let parts = u32_at(container, 0x1c)? as usize;
    if parts > 64 {
        return None;
    }
    (0..parts).find_map(|i| {
        let at = u32_at(container, 0x20 + 4 * i)? as usize;
        if container.get(at..at + 4) != Some(b"HASH") || u32_at(container, at + 4)? < 20 {
            return None;
        }
        container.get(at + 12..at + 28)?.try_into().ok()
    })
}

/// The cache file inside the shader archive: the chain of zlib streams
/// whose first unpacks to a cache header, each next stream starting at most
/// 64 bytes after the end of the one before.
pub fn find_in_archive(archive: &[u8]) -> Option<Vec<u8>> {
    let mut state = zlib_rs::Inflate::new(true, 15);
    for at in memchr::memchr_iter(0x78, archive) {
        if !zlib_header_at(archive, at) {
            continue;
        }
        let Some((first, _)) = inflate_one(&mut state, &archive[at..], HEADER) else {
            continue;
        };
        if u32_at(&first, 0) != Some(MAGIC) {
            continue;
        }
        return chain(&mut state, archive, at);
    }
    None
}

fn zlib_header_at(bytes: &[u8], at: usize) -> bool {
    bytes
        .get(at..at + 2)
        .is_some_and(|h| h[0] == 0x78 && u16::from_be_bytes([h[0], h[1]]) % 31 == 0)
}

/// The streams from `at` on, until the header's size is reached.
fn chain(state: &mut zlib_rs::Inflate, archive: &[u8], mut at: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut want = usize::MAX;
    while out.len() < want {
        let (part, used) = inflate_one(state, archive.get(at..)?, usize::MAX)?;
        out.extend_from_slice(&part);
        if want == usize::MAX && out.len() >= HEADER {
            want = Cache::size_of(&out)?;
            out.reserve(want.saturating_sub(out.len()));
        }
        if out.len() >= want {
            break;
        }
        let next = at + used;
        at = (next..(next + 64).min(archive.len().saturating_sub(1)))
            .find(|&i| zlib_header_at(archive, i))?;
    }
    (out.len() == want).then_some(out)
}

/// One zlib stream at the start of `input`: its bytes (at most `limit`) and
/// how many input bytes it took (when unpacked whole).
fn inflate_one(
    state: &mut zlib_rs::Inflate,
    input: &[u8],
    limit: usize,
) -> Option<(Vec<u8>, usize)> {
    state.reset(true);
    let mut out = Vec::new();
    let mut buf = vec![0u8; limit.min(1 << 16)];
    let (mut used, mut made) = (0usize, 0usize);
    loop {
        let room = buf.len().min(limit - out.len());
        let status = state
            .decompress(
                &input[used..],
                &mut buf[..room],
                zlib_rs::InflateFlush::NoFlush,
            )
            .ok()?;
        let (now_in, now_out) = (state.total_in() as usize, state.total_out() as usize);
        out.extend_from_slice(&buf[..now_out - made]);
        let moved = now_in != used || now_out != made;
        (used, made) = (now_in, now_out);
        if out.len() >= limit {
            return Some((out, used));
        }
        match status {
            zlib_rs::Status::StreamEnd => return Some((out, used)),
            zlib_rs::Status::Ok if moved => {}
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A code block whose prologue spells `hash`.
    fn code_with(hash: [u8; 16], body: &[u32]) -> Vec<u8> {
        let d = |i: usize| u32::from_le_bytes(hash[4 * i..4 * i + 4].try_into().unwrap());
        let ops = |x: u32| 0xff << 18 | (0x100 | (x >> 24 & 0xff)) << 9 | 0x100 | (x >> 16 & 0xff);
        let mut w = vec![
            0xd580_0000 | (d(0) & 0xffff),
            ops(d(0)),
            d(1),
            0xd580_0000 | (d(2) & 0xffff),
            ops(d(2)),
            d(3),
        ];
        w.extend_from_slice(body);
        w.extend([0xbf9f_0000; 4]);
        w.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    fn meta_with(head: &[u32], hash: [u8; 16]) -> Vec<u8> {
        let mut m: Vec<u8> = head.iter().flat_map(|x| x.to_le_bytes()).collect();
        for kind in [0x41u32, 0x42] {
            m.extend_from_slice(&kind.to_le_bytes());
            m.extend_from_slice(&[7; 0x1c]);
        }
        m.extend_from_slice(&0x43u32.to_le_bytes());
        m.extend_from_slice(&hash);
        m.extend_from_slice(&[0; 12]);
        m
    }

    /// A cache file of `graphics` records (state blocks, then per slot a
    /// shader digest) and one compute record naming `compute`.
    fn cache_of(
        graphics: &[(Vec<u8>, Vec<u8>, [Option<[u8; 16]>; GRAPHICS_SLOTS])],
        compute: [u8; 16],
    ) -> Vec<u8> {
        let (mut blocks, mut codes) = (Vec::<Vec<u8>>::new(), Vec::<Vec<u8>>::new());
        let mut g = Vec::new();
        for (n, (regs, desc, slots)) in graphics.iter().enumerate() {
            g.extend_from_slice(&(0x1122 + n as u64).to_le_bytes());
            for b in [regs, desc] {
                g.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
                blocks.push(b.clone());
            }
            for k in 0..5u64 {
                g.extend_from_slice(&(0x100 + k).to_le_bytes());
            }
            for slot in slots {
                let (c, m) = match slot {
                    Some(h) => {
                        codes.push(code_with(*h, &[0xbfa0_0003, 0xbf81_0000]));
                        blocks.push(meta_with(&[2, 0, 0x76, 0], *h));
                        (codes.len() as u32 - 1, blocks.len() as u32 - 1)
                    }
                    None => (NONE, NONE),
                };
                g.extend_from_slice(&c.to_le_bytes());
                g.extend_from_slice(&m.to_le_bytes());
            }
        }
        let mut c = 0x3344u64.to_le_bytes().to_vec();
        let first = blocks.len() as u32;
        blocks.extend([
            vec![3; 0x1c],
            vec![4; 0x14],
            meta_with(&[0x403, 0, 0x60, 0, 0, 0], compute),
        ]);
        codes.push(code_with(compute, &[0xbf81_0000]));
        for v in [first, first + 1, codes.len() as u32 - 1, first + 2] {
            c.extend_from_slice(&v.to_le_bytes());
        }
        let table = |parts: &[Vec<u8>], align: usize| {
            let (mut t, mut bytes) = (Vec::new(), Vec::new());
            for p in parts {
                bytes.resize(bytes.len().next_multiple_of(align), 0);
                t.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                t.extend_from_slice(&(p.len() as u32).to_le_bytes());
                bytes.extend_from_slice(p);
            }
            (t, bytes)
        };
        let (bt, bb) = table(&blocks, 4);
        let (ct, cb) = table(&codes, 0x100);
        let mut f = vec![0u8; HEADER];
        f[..4].copy_from_slice(&MAGIC.to_le_bytes());
        f[4..12].copy_from_slice(b"build-01");
        f[0x24..0x2c].copy_from_slice(b"driver-1");
        for (i, n) in [
            graphics.len(),
            1,
            blocks.len(),
            codes.len(),
            bb.len(),
            cb.len(),
        ]
        .into_iter()
        .enumerate()
        {
            f[0x50 + 4 * i..0x54 + 4 * i].copy_from_slice(&(n as u32).to_le_bytes());
        }
        for part in [g, c, bt, ct, bb, cb] {
            f.extend_from_slice(&part);
        }
        f
    }

    const H: [[u8; 16]; 3] = [[0xa1; 16], [0xb2; 16], [0xc3; 16]];

    fn one_mesh_record() -> Vec<u8> {
        let mut slots = [None; GRAPHICS_SLOTS];
        slots[0] = Some(H[0]);
        slots[2] = Some(H[1]);
        cache_of(&[(vec![1; 0x184], vec![2; 0x50], slots)], H[2])
    }

    #[test]
    fn parses_records_blocks_and_codes() {
        let file = one_mesh_record();
        let cache = Cache::parse(&file).unwrap();
        assert_eq!((cache.build, cache.driver), ("build-01", "driver-1"));
        assert_eq!((cache.graphics_count(), cache.compute_count()), (1, 1));
        assert_eq!(Cache::size_of(&file[..HEADER]), Some(file.len()));
        let g = cache.graphics(0).unwrap();
        assert_eq!(g.key, 0x1122);
        assert_eq!(g.hashes[4], 0x104);
        assert_eq!(cache.block(g.state[0]).unwrap().len(), 0x184);
        assert_eq!(cache.block(g.state[1]).unwrap().len(), 0x50);
        let used: Vec<usize> = (0..GRAPHICS_SLOTS)
            .filter(|&s| g.stages[s].is_some())
            .collect();
        assert_eq!(used, [0, 2]);
        assert_eq!(
            used.iter()
                .map(|&s| slot_stage(s).unwrap())
                .collect::<Vec<_>>(),
            ["pixel", "mesh"]
        );
        for (slot, want) in [(0, H[0]), (2, H[1])] {
            let (code, meta) = g.stages[slot].unwrap();
            let (code, meta) = (cache.code(code).unwrap(), cache.block(meta).unwrap());
            assert_eq!(stage_shader_hash(code, meta), Some(want));
        }
        let c = cache.compute(0).unwrap();
        assert_eq!(c.key, 0x3344);
        assert_eq!(cache.block(c.state[0]).unwrap().len(), 0x1c);
        assert_eq!(
            stage_shader_hash(cache.code(c.code).unwrap(), cache.block(c.meta).unwrap()),
            Some(H[2])
        );
        assert!(cache.graphics(1).is_none() && cache.code(9).is_none());
        // A size that disagrees with the tables is refused.
        assert!(Cache::parse(&file[..file.len() - 1]).is_err());
        assert!(Cache::parse(&file[..HEADER - 1]).is_err());
        let mut other = file.clone();
        other[0] ^= 1;
        assert!(Cache::parse(&other).is_err());
    }

    #[test]
    fn the_code_prologue_and_the_metadata_spell_the_digest() {
        let digest: [u8; 16] = std::array::from_fn(|i| (i as u8).wrapping_mul(37) ^ 0x5c);
        let code = code_with(digest, &[]);
        assert_eq!(code_shader_hash(&code), Some(digest));
        assert_eq!(code_shader_hash(&code[4..]), None);
        assert_eq!(code_shader_hash(&code[..20]), None);
        let meta = meta_with(&[2, 0, 0x76, 0], digest);
        assert_eq!(meta_shader_hash(&meta), Some(digest));
        assert_eq!(stage_shader_hash(&code, &meta), Some(digest));
        // Metadata and code that disagree name nothing.
        let other = meta_with(&[2, 0, 0x76, 0], [9; 16]);
        assert_eq!(stage_shader_hash(&code, &other), None);
        // Either alone is enough.
        assert_eq!(stage_shader_hash(&code, &[]), Some(digest));
        assert_eq!(stage_shader_hash(&[0; 24], &meta), Some(digest));
        assert_eq!(meta_shader_hash(&[5, 0, 0, 0]), None);
        // An entry of an unknown kind ends the walk.
        let mut odd = meta.clone();
        odd[16] = 0x44;
        assert_eq!(meta_shader_hash(&odd), None);
    }

    /// A container of `parts`, as DXBC lays them out.
    fn container(parts: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut out = b"DXBC".to_vec();
        out.extend([0; 16]);
        out.extend(1u32.to_le_bytes());
        let head = 0x20 + 4 * parts.len();
        let total = head + parts.iter().map(|p| 8 + p.1.len()).sum::<usize>();
        out.extend((total as u32).to_le_bytes());
        out.extend((parts.len() as u32).to_le_bytes());
        let mut at = head;
        for p in parts {
            out.extend((at as u32).to_le_bytes());
            at += 8 + p.1.len();
        }
        for (fourcc, data) in parts {
            out.extend(*fourcc);
            out.extend((data.len() as u32).to_le_bytes());
            out.extend(data);
        }
        out
    }

    #[test]
    fn a_container_s_digest_is_its_hash_part() {
        let mut hash = vec![0; 4];
        hash.extend([0x5a; 16]);
        let c = container(&[
            (b"ISG1", vec![1; 40]),
            (b"HASH", hash),
            (b"DXIL", vec![2; 64]),
        ]);
        assert_eq!(container_shader_hash(&c), Some([0x5a; 16]));
        assert_eq!(
            container_shader_hash(&container(&[(b"DXIL", vec![2; 8])])),
            None
        );
        assert_eq!(container_shader_hash(b"DXBX"), None);
        // A HASH part too short to hold a digest.
        assert_eq!(
            container_shader_hash(&container(&[(b"HASH", vec![0; 8])])),
            None
        );
    }

    fn zlib(bytes: &[u8]) -> Vec<u8> {
        let mut buf = vec![0u8; zlib_rs::compress_bound(bytes.len())];
        let (packed, rc) = zlib_rs::compress_slice(&mut buf, bytes, zlib_rs::DeflateConfig::new(9));
        assert_eq!(rc, zlib_rs::ReturnCode::Ok);
        packed.to_vec()
    }

    #[test]
    fn finds_the_chain_of_streams_in_an_archive() {
        let file = one_mesh_record();
        let mut archive = vec![0x78u8, 0x9c, 0x00, 0x01, 0x55];
        archive.extend(zlib(b"another file"));
        // The cache in 100-byte pieces, with a few bytes between streams.
        for (i, piece) in file.chunks(100).enumerate() {
            archive.extend(zlib(piece));
            archive.extend(std::iter::repeat_n(0u8, i % 13));
        }
        assert_eq!(find_in_archive(&archive).as_deref(), Some(&file[..]));
        // A chain that stops early is no file.
        let cut = archive.len() - 40;
        assert_eq!(find_in_archive(&archive[..cut]), None);
        assert_eq!(find_in_archive(b"no streams here"), None);
    }

    #[test]
    fn one_large_stream_is_read_whole() {
        // More than the 64 KiB a step unpacks at once.
        let mut slots = [None; GRAPHICS_SLOTS];
        slots[1] = Some(H[0]);
        let records: Vec<_> = (0..1200)
            .map(|i| (vec![i as u8; 0x184], vec![2; 0x50], slots))
            .collect();
        let file = cache_of(&records, H[2]);
        assert!(file.len() > 1 << 17);
        let archive = zlib(&file);
        assert_eq!(find_in_archive(&archive).as_deref(), Some(&file[..]));
    }
}
