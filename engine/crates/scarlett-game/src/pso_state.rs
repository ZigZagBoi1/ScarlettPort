//! The two state blocks of a precompiled graphics pipeline
//! ([`crate::pso_cache`]), decoded into what a D3D12 graphics pipeline
//! description holds.
//!
//! - Block 0 (380 to 520 bytes) is the console driver's register image of
//!   the pipeline: GFX10 context registers in an order of the driver's own,
//!   the same for every record from the dword `0x060201bc` on (dword 29 of
//!   a vertex-shader pipeline, dword 50 of a mesh one; dword 1 is 4 or 6).
//!   Blend, depth/stencil and rasteriser state are read from it.
//! - Block 1 (80 to 340 bytes) is the driver's copy of the description's
//!   other fields: render target and depth formats, their count, the
//!   topology type, sample mask and count, the stream output (always
//!   empty) and the input layout with its semantic names.
//!
//! Ported from ScarlettPort before the 0.4.6 engine swap (`pso_state.rs`
//! at 424fc91^). Its tests there read blocks of the title's file, which
//! may not be kept in the repository, so the tests below build their own.
//! Checked then against the title: the
//! pipeline journal of a run (56,015 graphics descriptions the title passed
//! to `CreateGraphicsPipelineState`, every field, with its own shaders)
//! names the shaders of 13,398 of the 13,409 vertex-shader records, and
//! 13,185 of them decode to a description the title created field for
//! field (blend of every target it has, sample mask, the whole rasteriser
//! state with depth bias and conservative rasterisation, depth/stencil,
//! input layout, topology, strip cut, formats, sample count). All but one
//! of the other 213 differ from the title's pipelines with the same
//! shaders in their target or depth formats only: other pipelines than
//! the run made.
//! Mesh records are created from streams, which the journal does not have.
//!
//! Fields are taken only where the title's pipelines confirm the
//! decoding. A field holding a value they do not confirm (a logic
//! operation, a fill mode other than solid, a topology type other than
//! triangles or rectangles, a sample count above one, ...) makes the
//! record refused ([`Error::Unverified`]): the title then creates the
//! pipeline from its stream.

/// One render target's blend state, as `D3D12_RENDER_TARGET_BLEND_DESC`
/// holds it (D3D12 enumeration values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetBlend {
    pub blend: bool,
    pub logic_op_enable: bool,
    pub src: u32,
    pub dst: u32,
    pub op: u32,
    pub src_alpha: u32,
    pub dst_alpha: u32,
    pub op_alpha: u32,
    pub logic_op: u32,
    pub write_mask: u8,
}

/// `D3D12_DEPTH_STENCILOP_DESC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StencilFace {
    pub fail: u32,
    pub depth_fail: u32,
    pub pass: u32,
    pub func: u32,
}

/// `D3D12_INPUT_ELEMENT_DESC` (format: the console's DXGI number).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputElement {
    pub name: String,
    pub index: u32,
    pub format: u32,
    pub slot: u32,
    pub offset: u32,
    pub class: u32,
    pub step_rate: u32,
}

/// A graphics pipeline description without its shaders and root
/// signature. Enumerations are D3D12's values; formats the console's
/// DXGI numbers (as the title passes them in a stream).
#[derive(Debug, Clone, PartialEq)]
pub struct GraphicsDesc {
    /// The record's shader stages feed a mesh shader (no input layout).
    pub mesh: bool,
    pub alpha_to_coverage: bool,
    pub independent_blend: bool,
    pub targets_blend: [TargetBlend; 8],
    pub sample_mask: u32,
    pub fill_mode: u32,
    pub cull_mode: u32,
    pub front_ccw: bool,
    pub depth_bias: i32,
    pub depth_bias_clamp: f32,
    pub slope_scaled_depth_bias: f32,
    pub depth_clip: bool,
    /// `D3D12_CONSERVATIVE_RASTERIZATION_MODE_ON` (overestimation).
    pub conservative_raster: bool,
    pub depth_enable: bool,
    /// `D3D12_DEPTH_WRITE_MASK`: 0 or 1.
    pub depth_write: u32,
    pub depth_func: u32,
    pub stencil_enable: bool,
    pub stencil_read_mask: u8,
    pub stencil_write_mask: u8,
    pub front: StencilFace,
    pub back: StencilFace,
    pub depth_bounds: bool,
    pub inputs: Vec<InputElement>,
    pub strip_cut: u32,
    pub topology: u32,
    pub num_targets: u32,
    pub target_formats: [u32; 8],
    pub depth_format: u32,
    pub sample_count: u32,
    pub sample_quality: u32,
}

/// Why a pair of state blocks does not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The register block is too short or has no anchor where expected.
    Registers,
    /// A register field holds a value without a D3D12 equivalent.
    Value(&'static str, u32),
    /// The description block does not hold what its sizes say.
    Desc(&'static str),
    /// A field holds a value whose decoding no pipeline of the title
    /// confirms: not guessed, the record is refused.
    Unverified(&'static str, u32),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Registers => f.write_str("register block of unknown layout"),
            Error::Value(what, v) => write!(f, "{what} {v:#x} has no D3D12 value"),
            Error::Desc(what) => write!(f, "description block: {what}"),
            Error::Unverified(what, v) => {
                write!(
                    f,
                    "{what} {v:#x}: a value the title's pipelines do not confirm"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

/// The dword every register block has where its fixed part starts.
pub const ANCHOR: u32 = 0x0602_01bc;

/// Register block dwords, from [`ANCHOR`].
mod at {
    pub const CB_COLOR_CONTROL: usize = 2;
    pub const PA_CL_CLIP_CNTL: usize = 10;
    pub const PA_SU_SC_MODE_CNTL: usize = 11;
    pub const POLY_OFFSET_CLAMP: usize = 14;
    pub const POLY_OFFSET_SCALE: usize = 15;
    pub const POLY_OFFSET_OFFSET: usize = 16;
    pub const DB_DEPTH_CONTROL: usize = 17;
    pub const DB_STENCIL_CONTROL: usize = 18;
    pub const DB_STENCILREFMASK: usize = 19;
    pub const DB_STENCILREFMASK_BF: usize = 20;
    /// `CB_TARGET_MASK` as the description gives it: 4 bits per target,
    /// all eight.
    pub const CB_TARGET_MASK: usize = 22;
    pub const CB_BLEND0_CONTROL: usize = 23;
    pub const SX_PS_DOWNCONVERT: usize = 32;
    /// `PA_SC_CONSERVATIVE_RASTERIZATION_CNTL`.
    pub const CONSERVATIVE: usize = 43;
    /// Dwords from the anchor on that a block has at least.
    pub const END: usize = 46;
}

/// Where the anchor is: dword 29 of a vertex-shader pipeline's block
/// (dword 1 = 4), 50 of a mesh one's (dword 1 = 6).
fn anchor(w: &[u32]) -> Option<(usize, bool)> {
    let (a, mesh) = match w.get(1)? {
        4 => (29, false),
        6 => (50, true),
        _ => return None,
    };
    (w.get(a) == Some(&ANCHOR) && w.len() >= a + at::END).then_some((a, mesh))
}

fn dwords(b: &[u8]) -> Vec<u32> {
    b.chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().expect("4 bytes")))
        .collect()
}

fn bits(v: u32, lo: u32, n: u32) -> u32 {
    v >> lo & ((1 << n) - 1)
}

/// `V_028780_BLEND_*` to `D3D12_BLEND` (for the alpha factors, the colour
/// ones read as their alpha ones, which is what they are there).
fn blend_factor(hw: u32, alpha: bool) -> Result<u32, Error> {
    Ok(match (hw, alpha) {
        (0..=10, false) => hw + 1,
        (2, true) => 5,
        (3, true) => 6,
        (8, true) => 7,
        (9, true) => 8,
        (0 | 1 | 4..=7 | 10, true) => hw + 1,
        (13, _) => 14,
        (14, _) => 15,
        (15, false) => 16,
        (16, false) => 17,
        (15, true) | (17, _) => 18,
        (16, true) | (18, _) => 19,
        (19, _) => 20,
        (20, _) => 21,
        _ => return Err(Error::Value("blend factor", hw)),
    })
}

/// `COMB_FCN` to `D3D12_BLEND_OP`.
fn blend_op(hw: u32) -> Result<u32, Error> {
    Ok(match hw {
        0 => 1, // add
        1 => 2, // subtract
        4 => 3, // reverse subtract
        2 => 4, // min
        3 => 5, // max
        _ => return Err(Error::Value("blend function", hw)),
    })
}

/// `CB_COLOR_CONTROL.ROP3`: copy (0xcc) is no logic operation, the only
/// value of the title's file; any other is refused (no pipeline of the
/// title confirms how the driver writes D3D12's logic operations).
const ROP3_COPY: u32 = 0xcc;

/// `PA_SC_CONSERVATIVE_RASTERIZATION_CNTL` as the driver writes it: off
/// (`NULL_SQUAD_AA_MASK_ENABLE` alone) or overestimation
/// (`OVER_RAST_ENABLE`, `UNDER_RAST_SAMPLE_SELECT` 1,
/// `PBB_UNCERTAINTY_REGION_ENABLE`, the coverage, pre-Z and post-Z AA
/// masks: what Mesa's radv writes for it). Some records have the second;
/// all those the title's journal names it created with
/// `ConservativeRaster` on, all others off.
const CONSERVATIVE_OFF: u32 = 0x0010_0000;
const CONSERVATIVE_ON: u32 = 0x00e0_0441;

/// `STENCIL_*` to `D3D12_STENCIL_OP` (both replace forms are replace; the
/// clamped and wrapped add and subtract of the operation value 1 are
/// D3D12's increments and decrements).
fn stencil_op(hw: u32) -> Result<u32, Error> {
    Ok(match hw {
        0 => 1,
        1 => 2,
        3 | 4 => 3,
        5 => 4,
        6 => 5,
        7 => 6,
        8 => 7,
        9 => 8,
        _ => return Err(Error::Value("stencil operation", hw)),
    })
}

fn f32_of(v: u32) -> f32 {
    f32::from_bits(v)
}

/// The register image's part of the description.
fn registers(block: &[u8], d: &mut GraphicsDesc) -> Result<(), Error> {
    let w = dwords(block);
    let (a, mesh) = anchor(&w).ok_or(Error::Registers)?;
    let r = |i: usize| w[a + i];
    d.mesh = mesh;

    // Blend.
    let rop3 = bits(r(at::CB_COLOR_CONTROL), 16, 8);
    if rop3 != ROP3_COPY {
        return Err(Error::Unverified("logic operation (ROP3)", rop3));
    }
    let masks = r(at::CB_TARGET_MASK);
    for (t, out) in d.targets_blend.iter_mut().enumerate() {
        let b = r(at::CB_BLEND0_CONTROL + t);
        let write_mask = bits(masks, 4 * t as u32, 4) as u8;
        *out = if b >> 30 & 1 == 1 {
            TargetBlend {
                blend: true,
                src: blend_factor(bits(b, 0, 5), false)?,
                op: blend_op(bits(b, 5, 3))?,
                dst: blend_factor(bits(b, 8, 5), false)?,
                src_alpha: blend_factor(bits(b, 16, 5), true)?,
                op_alpha: blend_op(bits(b, 21, 3))?,
                dst_alpha: blend_factor(bits(b, 24, 5), true)?,
                write_mask,
                ..DEFAULT_BLEND
            }
        } else {
            TargetBlend {
                write_mask,
                ..DEFAULT_BLEND
            }
        };
    }
    d.independent_blend = d.targets_blend.iter().any(|t| *t != d.targets_blend[0]);

    // Rasteriser.
    let mode = r(at::PA_SU_SC_MODE_CNTL);
    d.cull_mode = match bits(mode, 0, 2) {
        0 => 1, // none
        1 => 2, // front
        2 => 3, // back
        // Both culled: D3D12 has no such mode.
        _ => return Err(Error::Value("cull mode", bits(mode, 0, 2))),
    };
    // FACE 0 (front is counter-clockwise) in nearly all records; both
    // values occur, and the title's pipelines confirm both.
    d.front_ccw = bits(mode, 2, 1) == 0;
    // POLY_MODE is 0 (solid; the type fields then do not apply) in every
    // record and the title never asks for wireframe: refused otherwise.
    if bits(mode, 3, 2) != 0 {
        return Err(Error::Unverified("polygon mode", bits(mode, 3, 2)));
    }
    d.fill_mode = 3;
    // PA_SU_POLY_OFFSET_CLAMP, _FRONT_SCALE (1/16 pixel units, so the
    // slope factor times 16), _FRONT_OFFSET (the constant, as a float):
    // pipelines of the title with a bias, a slope and a clamp set
    // confirm order and scale.
    d.depth_bias_clamp = f32_of(r(at::POLY_OFFSET_CLAMP));
    d.slope_scaled_depth_bias = f32_of(r(at::POLY_OFFSET_SCALE)) / 16.0;
    let offset = f32_of(r(at::POLY_OFFSET_OFFSET));
    if offset.fract() != 0.0 || offset.abs() > i32::MAX as f32 {
        return Err(Error::Value("depth bias", r(at::POLY_OFFSET_OFFSET)));
    }
    d.depth_bias = offset as i32;
    let clip = r(at::PA_CL_CLIP_CNTL);
    // ZCLIP_NEAR_DISABLE and ZCLIP_FAR_DISABLE: both or neither.
    d.depth_clip = match bits(clip, 26, 2) {
        0 => true,
        3 => false,
        v => return Err(Error::Value("depth clip (one plane only)", v)),
    };
    d.conservative_raster = match r(at::CONSERVATIVE) {
        CONSERVATIVE_OFF => false,
        CONSERVATIVE_ON => true,
        v => return Err(Error::Unverified("conservative rasterisation", v)),
    };

    // Depth and stencil.
    let dc = r(at::DB_DEPTH_CONTROL);
    let sc = r(at::DB_STENCIL_CONTROL);
    let refmask = r(at::DB_STENCILREFMASK);
    d.stencil_enable = bits(dc, 0, 1) == 1;
    d.depth_enable = bits(dc, 1, 1) == 1;
    d.depth_write = bits(dc, 2, 1);
    // DEPTH_BOUNDS_ENABLE: never set in the title's file.
    if bits(dc, 3, 1) == 1 {
        return Err(Error::Unverified("depth bounds test", dc));
    }
    d.depth_bounds = false;
    d.depth_func = bits(dc, 4, 3) + 1;
    let back_func = if bits(dc, 7, 1) == 1 {
        bits(dc, 20, 3)
    } else {
        bits(dc, 8, 3)
    };
    let face = |at: u32, func: u32| -> Result<StencilFace, Error> {
        Ok(StencilFace {
            fail: stencil_op(bits(sc, at, 4))?,
            pass: stencil_op(bits(sc, at + 4, 4))?,
            depth_fail: stencil_op(bits(sc, at + 8, 4))?,
            func: func + 1,
        })
    };
    d.front = face(0, bits(dc, 8, 3))?;
    d.back = face(12, back_func)?;
    d.stencil_read_mask = bits(refmask, 8, 8) as u8;
    d.stencil_write_mask = bits(refmask, 16, 8) as u8;
    // D3D12 has one pair of masks for both faces.
    let back = r(at::DB_STENCILREFMASK_BF);
    if bits(back, 8, 16) != bits(refmask, 8, 16) {
        return Err(Error::Value("back-face stencil masks", back));
    }
    Ok(())
}

const DEFAULT_BLEND: TargetBlend = TargetBlend {
    blend: false,
    logic_op_enable: false,
    src: 2,
    dst: 1,
    op: 1,
    src_alpha: 2,
    dst_alpha: 1,
    op_alpha: 1,
    logic_op: 4,
    write_mask: 0xf,
};

/// The description block's part.
///
/// ```text
/// +0x00 u32 hash, u32 0x1000, u32 0, u32 input slots used (a bit each)
/// +0x10 u8[8] render target formats
/// +0x18 u32 0, u32 sample mask, u32 0
/// +0x24 u8 depth format, u8 render targets, u8 topology type, u8 0
/// +0x28 u32 sample count (low byte), u32 x3 (zero but in a few records)
/// +0x38 u32 input layout bytes (from +0x4c), u32 stream output bytes (12)
/// +0x40 stream output: u32 entries, strides, rasterized stream (all 0)
/// +0x4c u32 input elements, then each: u32 name bytes (with the NUL),
///       the name, u32 index, format, slot, offset, class, step rate
/// ```
/// The console's rectangle-list topology type, as the title's pipeline
/// streams name it.
pub const RECTANGLES: u32 = 19;
const TRIANGLES: u32 = 3;

/// The topology byte at +0x26: the title's `PrimitiveTopologyType`
/// itself, all of it. 0x13 is the console's rectangle list, not a
/// triangle type with a strip cut: taken apart as such, its pipelines lost
/// the geometry stage that completes each rectangle and drew half of each
/// one. Triangles and rectangles are what the title's pipelines confirm;
/// lines (one record) and any other value are refused.
fn topology(byte: u32) -> Result<u32, Error> {
    match byte {
        TRIANGLES | RECTANGLES => Ok(byte),
        _ => Err(Error::Unverified("topology type", byte)),
    }
}

/// +0x28..+0x38 of a few records, each alone with its shaders: the same
/// four dwords in all, with a sample count of 1. The title creates such
/// pipelines with sample count 1 and quality 0, every other field as the
/// record has it: something of the console's own, not part of a D3D12
/// description.
const SAMPLE_DWORDS: [[u32; 4]; 2] = [[1, 0, 0, 0], [0x0080_0001, 0x0404_0400, 0x0404_0404, 4]];

fn description(block: &[u8], d: &mut GraphicsDesc) -> Result<(), Error> {
    let u = |at: usize| -> Result<u32, Error> {
        block
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
            .ok_or(Error::Desc("too short"))
    };
    if block.len() < 0x50 {
        return Err(Error::Desc("too short"));
    }
    for (t, f) in d.target_formats.iter_mut().enumerate() {
        *f = u32::from(block[0x10 + t]);
    }
    d.sample_mask = u(0x1c)?;
    let w = u(0x24)?;
    d.depth_format = bits(w, 0, 8);
    d.num_targets = bits(w, 8, 8);
    if d.num_targets > 8 {
        return Err(Error::Desc("more than eight render targets"));
    }
    d.topology = topology(bits(w, 16, 8))?;
    if bits(w, 24, 8) != 0 {
        return Err(Error::Unverified(
            "byte after the topology type",
            bits(w, 24, 8),
        ));
    }
    // No byte holds a strip cut value: every pipeline of the title has
    // none (D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_DISABLED).
    d.strip_cut = 0;
    let samples = [u(0x28)?, u(0x2c)?, u(0x30)?, u(0x34)?];
    if !SAMPLE_DWORDS.contains(&samples) {
        // A sample count above 1 among them: no record has one, and the
        // title's pipelines are all single-sampled.
        return Err(Error::Unverified("sample count", samples[0]));
    }
    d.sample_count = 1;
    d.sample_quality = 0;
    if u(0x3c)? != 12 || u(0x40)? != 0 || u(0x44)? != 0 {
        return Err(Error::Desc("a stream output"));
    }
    let layout = u(0x38)? as usize;
    let n = u(0x4c)? as usize;
    let mut at = 0x50;
    for _ in 0..n {
        let len = u(at)? as usize;
        let name = block
            .get(at + 4..at + 4 + len)
            .ok_or(Error::Desc("input element name"))?;
        let name = name
            .strip_suffix(&[0])
            .ok_or(Error::Desc("input element name without its end"))?;
        let name = std::str::from_utf8(name).map_err(|_| Error::Desc("input element name"))?;
        at += 4 + len;
        d.inputs.push(InputElement {
            name: name.to_owned(),
            index: u(at)?,
            format: u(at + 4)?,
            slot: u(at + 8)?,
            offset: u(at + 12)?,
            class: u(at + 16)?,
            step_rate: u(at + 20)?,
        });
        at += 24;
    }
    if at - 0x4c != layout {
        return Err(Error::Desc("input layout size"));
    }
    // +0x0c: the input slots the layout reads, a bit each (all records).
    let slots = d
        .inputs
        .iter()
        .try_fold(0u32, |m, e| 1u32.checked_shl(e.slot).map(|b| m | b));
    if slots != Some(u(0x0c)?) {
        return Err(Error::Desc("input slots"));
    }
    Ok(())
}

/// A record's two state blocks, decoded.
pub fn decode(registers_block: &[u8], description_block: &[u8]) -> Result<GraphicsDesc, Error> {
    let mut d = GraphicsDesc {
        mesh: false,
        alpha_to_coverage: false,
        independent_blend: false,
        targets_blend: [DEFAULT_BLEND; 8],
        sample_mask: u32::MAX,
        fill_mode: 3,
        cull_mode: 3,
        front_ccw: false,
        depth_bias: 0,
        depth_bias_clamp: 0.0,
        slope_scaled_depth_bias: 0.0,
        depth_clip: true,
        conservative_raster: false,
        depth_enable: false,
        depth_write: 0,
        depth_func: 8,
        stencil_enable: false,
        stencil_read_mask: 0xff,
        stencil_write_mask: 0xff,
        front: StencilFace {
            fail: 1,
            depth_fail: 1,
            pass: 1,
            func: 8,
        },
        back: StencilFace {
            fail: 1,
            depth_fail: 1,
            pass: 1,
            func: 8,
        },
        depth_bounds: false,
        inputs: Vec::new(),
        strip_cut: 0,
        topology: 3,
        num_targets: 0,
        target_formats: [0; 8],
        depth_format: 0,
        sample_count: 1,
        sample_quality: 0,
    };
    registers(registers_block, &mut d)?;
    description(description_block, &mut d)?;
    if d.mesh && !d.inputs.is_empty() {
        return Err(Error::Desc("an input layout for a mesh pipeline"));
    }
    Ok(d)
}

/// `SX_PS_DOWNCONVERT`'s format for a render target format of the console
/// (`None`: not one seen in the title's records). For [`consistent`].
fn downconvert(format: u32) -> Option<&'static [u32]> {
    Some(match format {
        28 | 29 | 49 | 61 | 62 | 87 | 51 => &[0, 5],
        24 => &[4],
        26 => &[0, 3],
        34 | 35 => &[0, 9],
        41 | 42 => &[1],
        54 | 56 | 57 => &[0, 10],
        2 | 3 | 10 | 17 | 65 | 86 | 115 => &[0, 5],
        67 => &[11],
        _ => return None,
    })
}

/// Whether the register image agrees with the description block on the
/// render target formats (`SX_PS_DOWNCONVERT`, derived by the driver from
/// them). A check of the decoding, not needed to use it.
pub fn consistent(registers_block: &[u8], d: &GraphicsDesc) -> bool {
    let w = dwords(registers_block);
    let Some((a, _)) = anchor(&w) else {
        return false;
    };
    let down = w[a + at::SX_PS_DOWNCONVERT];
    (0..d.num_targets as usize).all(|t| {
        downconvert(d.target_formats[t]).is_none_or(|ok| ok.contains(&bits(down, 4 * t as u32, 4)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A register block of a vertex-shader (or mesh) pipeline: no blending,
    /// all eight targets written, back faces culled, counter-clockwise
    /// front faces, depth and stencil off, conservative rasterisation off;
    /// then the dwords of `set` (from the anchor) changed.
    fn registers_block(mesh: bool, set: &[(usize, u32)]) -> Vec<u8> {
        let a = if mesh { 50 } else { 29 };
        let mut w = vec![0u32; a + at::END];
        w[1] = if mesh { 6 } else { 4 };
        w[a] = ANCHOR;
        w[a + at::CB_COLOR_CONTROL] = ROP3_COPY << 16;
        w[a + at::PA_SU_SC_MODE_CNTL] = 2;
        w[a + at::CB_TARGET_MASK] = u32::MAX;
        w[a + at::DB_STENCILREFMASK] = 0x00ff_ff00;
        w[a + at::DB_STENCILREFMASK_BF] = 0x00ff_ff00;
        w[a + at::CONSERVATIVE] = CONSERVATIVE_OFF;
        for &(i, v) in set {
            w[a + i] = v;
        }
        w.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    /// An input element of a description block: name, then index, format,
    /// slot, offset, class, step rate.
    type Element<'a> = (&'a str, [u32; 6]);

    /// A description block: `formats` of the targets, the depth format,
    /// the topology byte, the input elements.
    fn description_block(
        formats: &[u8],
        depth: u8,
        topology: u8,
        inputs: &[Element<'_>],
    ) -> Vec<u8> {
        let mut layout = (inputs.len() as u32).to_le_bytes().to_vec();
        let mut slots = 0u32;
        for (name, v) in inputs {
            layout.extend((name.len() as u32 + 1).to_le_bytes());
            layout.extend(name.as_bytes());
            layout.push(0);
            for x in v {
                layout.extend(x.to_le_bytes());
            }
            slots |= 1 << v[2];
        }
        let mut b = vec![0u8; 0x4c];
        let mut put = |at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put(0, 0x1234_5678);
        put(4, 0x1000);
        put(0x0c, slots);
        put(0x1c, u32::MAX);
        put(
            0x24,
            u32::from(depth) | (formats.len() as u32) << 8 | u32::from(topology) << 16,
        );
        put(0x28, 1);
        put(0x38, layout.len() as u32);
        put(0x3c, 12);
        for (t, f) in formats.iter().enumerate() {
            b[0x10 + t] = *f;
        }
        b.extend(layout);
        b
    }

    fn plain() -> (Vec<u8>, Vec<u8>) {
        (
            registers_block(false, &[]),
            description_block(
                &[28],
                0,
                3,
                &[
                    ("POSITION", [0, 6, 0, 0, 0, 0]),
                    ("TEXCOORD", [0, 16, 1, 0, 0, 0]),
                ],
            ),
        )
    }

    #[test]
    fn a_plain_pipeline() {
        let (a, b) = plain();
        let d = decode(&a, &b).unwrap();
        assert!(!d.mesh && !d.depth_enable && !d.stencil_enable);
        assert_eq!(
            (d.cull_mode, d.front_ccw, d.fill_mode, d.depth_clip),
            (3, true, 3, true)
        );
        assert!(!d.conservative_raster && !d.independent_blend);
        assert!(d.targets_blend.iter().all(|t| *t == DEFAULT_BLEND));
        assert_eq!(
            (d.num_targets, d.target_formats[0], d.depth_format),
            (1, 28, 0)
        );
        assert_eq!(
            (
                d.topology,
                d.strip_cut,
                d.sample_count,
                d.sample_quality,
                d.sample_mask
            ),
            (3, 0, 1, 0, u32::MAX)
        );
        let names: Vec<(&str, u32, u32)> = d
            .inputs
            .iter()
            .map(|e| (e.name.as_str(), e.format, e.slot))
            .collect();
        assert_eq!(names, [("POSITION", 6, 0), ("TEXCOORD", 16, 1)]);
        assert!(consistent(&a, &d));
    }

    #[test]
    fn blending_with_source_alpha_and_per_target_masks() {
        // Colour: SRC_ALPHA (hw 4) + INV_SRC_ALPHA (hw 5), add; alpha:
        // ONE (hw 1), INV_SRC_ALPHA, add.
        let blend = 1 << 30 | 4 | 5 << 8 | 1 << 16 | 5 << 24;
        let a = registers_block(
            false,
            &[(at::CB_BLEND0_CONTROL, blend), (at::CB_TARGET_MASK, 0x3ff)],
        );
        let b = description_block(&[87, 10, 10], 20, 3, &[]);
        let d = decode(&a, &b).unwrap();
        let t = d.targets_blend[0];
        assert!(t.blend && !t.logic_op_enable);
        assert_eq!(
            (t.src, t.dst, t.op, t.src_alpha, t.dst_alpha, t.op_alpha),
            (5, 6, 1, 2, 6, 1)
        );
        let masks: Vec<u8> = d.targets_blend.iter().map(|t| t.write_mask).collect();
        assert_eq!(&masks[..4], &[0xf, 0xf, 0x3, 0]);
        assert!(!d.targets_blend[1].blend);
        assert!(d.independent_blend);
        assert_eq!((d.num_targets, d.depth_format), (3, 20));
        // Colour factors read as alpha ones in the alpha fields (SRC_COLOR
        // there is SRC_ALPHA), and the second source's factors.
        assert_eq!(blend_factor(2, true), Ok(5));
        assert_eq!(blend_factor(15, false), Ok(16));
        assert_eq!(blend_factor(17, true), Ok(18));
        assert_eq!(blend_op(4), Ok(3));
        assert_eq!(blend_op(2), Ok(4));
    }

    #[test]
    fn depth_and_stencil() {
        // Stencil and depth on, write, LESS_EQUAL (hw 3, D3D12 4); front
        // EQUAL (hw 2), back faces with their own function (hw 5,
        // NOT_EQUAL); front pass replace (hw 3), back fail
        // increment-clamp (hw 5).
        let dc = 1 | 1 << 1 | 1 << 2 | 3 << 4 | 2 << 8 | 1 << 7 | 5 << 20;
        let sc = 3 << 4 | 5 << 12;
        let mask = 0x0012_3400;
        let a = registers_block(
            false,
            &[
                (at::DB_DEPTH_CONTROL, dc),
                (at::DB_STENCIL_CONTROL, sc),
                (at::DB_STENCILREFMASK, mask),
                (at::DB_STENCILREFMASK_BF, mask | 0x77),
            ],
        );
        let b = description_block(&[], 55, 3, &[]);
        let d = decode(&a, &b).unwrap();
        assert!(d.depth_enable && d.stencil_enable && !d.depth_bounds);
        assert_eq!((d.depth_write, d.depth_func), (1, 4));
        assert_eq!((d.stencil_read_mask, d.stencil_write_mask), (0x34, 0x12));
        assert_eq!(
            d.front,
            StencilFace {
                fail: 1,
                depth_fail: 1,
                pass: 3,
                func: 3
            }
        );
        assert_eq!(
            d.back,
            StencilFace {
                fail: 4,
                depth_fail: 1,
                pass: 1,
                func: 6
            }
        );
        assert_eq!((d.num_targets, d.depth_format), (0, 55));
        assert_eq!(stencil_op(4), Ok(3));
    }

    #[test]
    fn the_rasteriser() {
        // Front faces culled, clockwise, depth clip off, overestimation,
        // bias 5, slope 40/16, clamp 0.5.
        let a = registers_block(
            false,
            &[
                (at::PA_SU_SC_MODE_CNTL, 1 | 1 << 2),
                (at::PA_CL_CLIP_CNTL, 3 << 26),
                (at::CONSERVATIVE, CONSERVATIVE_ON),
                (at::POLY_OFFSET_OFFSET, 5.0f32.to_bits()),
                (at::POLY_OFFSET_SCALE, 40.0f32.to_bits()),
                (at::POLY_OFFSET_CLAMP, 0.5f32.to_bits()),
            ],
        );
        let (_, b) = plain();
        let d = decode(&a, &b).unwrap();
        assert_eq!(
            (
                d.cull_mode,
                d.front_ccw,
                d.depth_clip,
                d.conservative_raster
            ),
            (2, false, false, true)
        );
        assert_eq!(
            (d.depth_bias, d.slope_scaled_depth_bias, d.depth_bias_clamp),
            (5, 2.5, 0.5)
        );
        let none = registers_block(false, &[(at::PA_SU_SC_MODE_CNTL, 0)]);
        assert_eq!(decode(&none, &b).unwrap().cull_mode, 1);
    }

    #[test]
    fn a_mesh_pipeline_and_a_rectangle_list() {
        let a = registers_block(true, &[]);
        let b = description_block(&[49], 0, RECTANGLES as u8, &[]);
        let d = decode(&a, &b).unwrap();
        assert!(d.mesh && d.inputs.is_empty());
        assert_eq!((d.topology, d.target_formats[0]), (RECTANGLES, 49));
        // A mesh pipeline has no input layout.
        let (_, with_inputs) = plain();
        assert_eq!(
            decode(&a, &with_inputs),
            Err(Error::Desc("an input layout for a mesh pipeline"))
        );
        // The console's own dwords after the topology are one sample.
        let mut b2 = b.clone();
        for (i, v) in SAMPLE_DWORDS[1].iter().enumerate() {
            b2[0x28 + 4 * i..0x2c + 4 * i].copy_from_slice(&v.to_le_bytes());
        }
        assert_eq!(decode(&a, &b2).unwrap().sample_count, 1);
    }

    #[test]
    fn broken_blocks_are_refused() {
        let (a, b) = plain();
        // No anchor, or a block too short to hold the fixed part.
        assert_eq!(decode(&a[..0x60], &b), Err(Error::Registers));
        let mut moved = a.clone();
        moved[4] = 5;
        assert_eq!(decode(&moved, &b), Err(Error::Registers));
        assert_eq!(decode(&a, &b[..0x40]), Err(Error::Desc("too short")));
        // A layout size that disagrees, or input slots that do.
        let mut b2 = b.clone();
        b2[0x38] ^= 1;
        assert_eq!(decode(&a, &b2), Err(Error::Desc("input layout size")));
        let mut b2 = b.clone();
        b2[0x0c] = 7;
        assert_eq!(decode(&a, &b2), Err(Error::Desc("input slots")));
        // A stream output.
        let mut b2 = b.clone();
        b2[0x40] = 1;
        assert_eq!(decode(&a, &b2), Err(Error::Desc("a stream output")));
        // Nine targets.
        let mut b2 = b.clone();
        b2[0x25] = 9;
        assert_eq!(
            decode(&a, &b2),
            Err(Error::Desc("more than eight render targets"))
        );
        // A blend factor without a D3D12 value (11, BOTH_SRC_ALPHA).
        let bad = registers_block(false, &[(at::CB_BLEND0_CONTROL, 1 << 30 | 11)]);
        assert_eq!(decode(&bad, &b), Err(Error::Value("blend factor", 11)));
    }

    /// Values no pipeline of the title confirms the decoding of: the
    /// record is refused, and the title creates the pipeline from its
    /// stream.
    #[test]
    fn unconfirmed_values_are_refused() {
        let (_, b) = plain();
        let with = |i: usize, v: u32| decode(&registers_block(false, &[(i, v)]), &b);
        assert_eq!(
            with(at::CB_COLOR_CONTROL, 0x66 << 16),
            Err(Error::Unverified("logic operation (ROP3)", 0x66))
        );
        assert_eq!(
            with(at::PA_SU_SC_MODE_CNTL, 2 | 1 << 3),
            Err(Error::Unverified("polygon mode", 1))
        );
        assert_eq!(
            with(at::PA_SU_SC_MODE_CNTL, 3),
            Err(Error::Value("cull mode", 3))
        );
        assert!(matches!(
            with(at::DB_DEPTH_CONTROL, 8),
            Err(Error::Unverified("depth bounds test", _))
        ));
        assert_eq!(
            with(at::CONSERVATIVE, 0x0010_0021),
            Err(Error::Unverified("conservative rasterisation", 0x0010_0021))
        );
        assert_eq!(
            with(at::PA_CL_CLIP_CNTL, 1 << 26),
            Err(Error::Value("depth clip (one plane only)", 1))
        );
        assert!(matches!(
            with(at::DB_STENCILREFMASK_BF, 0x00ff_fe00),
            Err(Error::Value("back-face stencil masks", _))
        ));
        assert!(matches!(
            with(at::POLY_OFFSET_OFFSET, 1.5f32.to_bits()),
            Err(Error::Value("depth bias", _))
        ));
        let a = registers_block(false, &[]);
        for byte in [2u8, 0x23] {
            let mut b2 = b.clone();
            b2[0x26] = byte;
            assert_eq!(
                decode(&a, &b2),
                Err(Error::Unverified("topology type", u32::from(byte)))
            );
        }
        let mut b2 = b.clone();
        b2[0x27] = 1;
        assert_eq!(
            decode(&a, &b2),
            Err(Error::Unverified("byte after the topology type", 1))
        );
        let mut b2 = b.clone();
        b2[0x28] = 4;
        assert_eq!(decode(&a, &b2), Err(Error::Unverified("sample count", 4)));
    }

    #[test]
    fn the_downconvert_register_checks_the_formats() {
        let b = description_block(&[24, 41], 0, 3, &[]);
        let good = registers_block(false, &[(at::SX_PS_DOWNCONVERT, 4 | 1 << 4)]);
        assert!(consistent(&good, &decode(&good, &b).unwrap()));
        let bad = registers_block(false, &[(at::SX_PS_DOWNCONVERT, 4 | 2 << 4)]);
        assert!(!consistent(&bad, &decode(&bad, &b).unwrap()));
    }
}
