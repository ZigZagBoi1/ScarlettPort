//! The title's precompiled pipelines (`scarlett_game::pso_cache`) as the
//! runtime would name them: a decoded record's state as the
//! [`GraphicsState`] a stream with the same fields gives, so that its
//! `describe()` text, and the title pipeline key made from it and the
//! shaders (`make::Pending::new`), are the ones the runtime gives the
//! pipeline when the title creates it from a stream. The keys depend on
//! the adapter: target formats go through `resource::target_format`,
//! which asks the host D3D12 device whether it renders to some formats
//! (so a device is made).

use std::ffi::CString;

use scarlett_game::pso_state::{GraphicsDesc, StencilFace, TargetBlend};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::PCSTR;

use super::host::GraphicsState;

/// The state of a decoded record, through the same format conversions
/// [`GraphicsState::read`] applies to a stream's fields.
pub(super) fn state_of(d: &GraphicsDesc) -> GraphicsState {
    let mut g = GraphicsState::default();
    let target = |t: &TargetBlend| D3D12_RENDER_TARGET_BLEND_DESC {
        BlendEnable: t.blend.into(),
        LogicOpEnable: t.logic_op_enable.into(),
        SrcBlend: D3D12_BLEND(t.src as i32),
        DestBlend: D3D12_BLEND(t.dst as i32),
        BlendOp: D3D12_BLEND_OP(t.op as i32),
        SrcBlendAlpha: D3D12_BLEND(t.src_alpha as i32),
        DestBlendAlpha: D3D12_BLEND(t.dst_alpha as i32),
        BlendOpAlpha: D3D12_BLEND_OP(t.op_alpha as i32),
        LogicOp: D3D12_LOGIC_OP(t.logic_op as i32),
        RenderTargetWriteMask: t.write_mask,
    };
    g.blend = D3D12_BLEND_DESC {
        AlphaToCoverageEnable: d.alpha_to_coverage.into(),
        IndependentBlendEnable: d.independent_blend.into(),
        RenderTarget: d.targets_blend.each_ref().map(target),
    };
    g.sample_mask = d.sample_mask;
    g.rasterizer = D3D12_RASTERIZER_DESC {
        FillMode: D3D12_FILL_MODE(d.fill_mode as i32),
        CullMode: D3D12_CULL_MODE(d.cull_mode as i32),
        FrontCounterClockwise: d.front_ccw.into(),
        DepthBias: d.depth_bias,
        DepthBiasClamp: d.depth_bias_clamp,
        SlopeScaledDepthBias: d.slope_scaled_depth_bias,
        DepthClipEnable: d.depth_clip.into(),
        ConservativeRaster: if d.conservative_raster {
            D3D12_CONSERVATIVE_RASTERIZATION_MODE_ON
        } else {
            D3D12_CONSERVATIVE_RASTERIZATION_MODE_OFF
        },
        ..Default::default()
    };
    let face = |f: &StencilFace| D3D12_DEPTH_STENCILOP_DESC {
        StencilFailOp: D3D12_STENCIL_OP(f.fail as i32),
        StencilDepthFailOp: D3D12_STENCIL_OP(f.depth_fail as i32),
        StencilPassOp: D3D12_STENCIL_OP(f.pass as i32),
        StencilFunc: D3D12_COMPARISON_FUNC(f.func as i32),
    };
    g.depth = D3D12_DEPTH_STENCIL_DESC {
        DepthEnable: d.depth_enable.into(),
        DepthWriteMask: D3D12_DEPTH_WRITE_MASK(d.depth_write as i32),
        DepthFunc: D3D12_COMPARISON_FUNC(d.depth_func as i32),
        StencilEnable: d.stencil_enable.into(),
        StencilReadMask: d.stencil_read_mask,
        StencilWriteMask: d.stencil_write_mask,
        FrontFace: face(&d.front),
        BackFace: face(&d.back),
    };
    g.depth_bounds = d.depth_bounds;
    g.names = d
        .inputs
        .iter()
        .map(|e| CString::new(e.name.as_str()).unwrap_or_default())
        .collect();
    g.inputs = d
        .inputs
        .iter()
        .zip(&g.names)
        .map(|(e, name)| D3D12_INPUT_ELEMENT_DESC {
            SemanticName: PCSTR(name.as_ptr().cast()),
            SemanticIndex: e.index,
            Format: DXGI_FORMAT(crate::resource::pc_format(e.format) as i32),
            InputSlot: e.slot,
            AlignedByteOffset: e.offset,
            InputSlotClass: D3D12_INPUT_CLASSIFICATION(e.class as i32),
            InstanceDataStepRate: e.step_rate,
        })
        .collect();
    g.strip_cut = D3D12_INDEX_BUFFER_STRIP_CUT_VALUE(d.strip_cut as i32);
    g.topology = D3D12_PRIMITIVE_TOPOLOGY_TYPE(d.topology as i32);
    g.targets = D3D12_RT_FORMAT_ARRAY {
        RTFormats: d.target_formats.map(|f| {
            DXGI_FORMAT(crate::resource::target_format(crate::resource::pc_format(f)) as i32)
        }),
        NumRenderTargets: d.num_targets,
    };
    g.depth_format = DXGI_FORMAT(crate::resource::pc_format(d.depth_format) as i32);
    g.samples = DXGI_SAMPLE_DESC {
        Count: d.sample_count,
        Quality: d.sample_quality,
    };
    g
}

/// A graphics state as text: whole (`describe()`, what the title pipeline
/// key and the library name hash) and field by field (to say where two
/// differ).
#[derive(Debug, Clone, PartialEq)]
pub struct StateText {
    pub describe: String,
    pub fields: Vec<(String, String)>,
    /// What the state does ([`semantic_of`]).
    pub semantic: String,
}

fn text_of(g: &GraphicsState) -> StateText {
    let mut f: Vec<(String, String)> = Vec::new();
    let mut put = |name: String, value: String| f.push((name, value));
    let b = &g.blend;
    put(
        "blend.AlphaToCoverageEnable".into(),
        format!("{:?}", b.AlphaToCoverageEnable),
    );
    put(
        "blend.IndependentBlendEnable".into(),
        format!("{:?}", b.IndependentBlendEnable),
    );
    for (i, t) in b.RenderTarget.iter().enumerate() {
        let mut rt = |what: &str, v: String| put(format!("blend.rt{i}.{what}"), v);
        rt("BlendEnable", format!("{:?}", t.BlendEnable));
        rt("LogicOpEnable", format!("{:?}", t.LogicOpEnable));
        rt("SrcBlend", format!("{:?}", t.SrcBlend));
        rt("DestBlend", format!("{:?}", t.DestBlend));
        rt("BlendOp", format!("{:?}", t.BlendOp));
        rt("SrcBlendAlpha", format!("{:?}", t.SrcBlendAlpha));
        rt("DestBlendAlpha", format!("{:?}", t.DestBlendAlpha));
        rt("BlendOpAlpha", format!("{:?}", t.BlendOpAlpha));
        rt("LogicOp", format!("{:?}", t.LogicOp));
        rt(
            "RenderTargetWriteMask",
            format!("{:?}", t.RenderTargetWriteMask),
        );
    }
    put("sample_mask".into(), format!("{}", g.sample_mask));
    let r = &g.rasterizer;
    put("rasterizer.FillMode".into(), format!("{:?}", r.FillMode));
    put("rasterizer.CullMode".into(), format!("{:?}", r.CullMode));
    put(
        "rasterizer.FrontCounterClockwise".into(),
        format!("{:?}", r.FrontCounterClockwise),
    );
    put("rasterizer.DepthBias".into(), format!("{:?}", r.DepthBias));
    put(
        "rasterizer.DepthBiasClamp".into(),
        format!("{:?}", r.DepthBiasClamp),
    );
    put(
        "rasterizer.SlopeScaledDepthBias".into(),
        format!("{:?}", r.SlopeScaledDepthBias),
    );
    put(
        "rasterizer.DepthClipEnable".into(),
        format!("{:?}", r.DepthClipEnable),
    );
    put(
        "rasterizer.MultisampleEnable".into(),
        format!("{:?}", r.MultisampleEnable),
    );
    put(
        "rasterizer.AntialiasedLineEnable".into(),
        format!("{:?}", r.AntialiasedLineEnable),
    );
    put(
        "rasterizer.ForcedSampleCount".into(),
        format!("{:?}", r.ForcedSampleCount),
    );
    put(
        "rasterizer.ConservativeRaster".into(),
        format!("{:?}", r.ConservativeRaster),
    );
    let d = &g.depth;
    put("depth.DepthEnable".into(), format!("{:?}", d.DepthEnable));
    put(
        "depth.DepthWriteMask".into(),
        format!("{:?}", d.DepthWriteMask),
    );
    put("depth.DepthFunc".into(), format!("{:?}", d.DepthFunc));
    put(
        "depth.StencilEnable".into(),
        format!("{:?}", d.StencilEnable),
    );
    put(
        "depth.StencilReadMask".into(),
        format!("{:?}", d.StencilReadMask),
    );
    put(
        "depth.StencilWriteMask".into(),
        format!("{:?}", d.StencilWriteMask),
    );
    put("depth.FrontFace".into(), format!("{:?}", d.FrontFace));
    put("depth.BackFace".into(), format!("{:?}", d.BackFace));
    put("depth_bounds".into(), format!("{}", g.depth_bounds));
    put("inputs.count".into(), format!("{}", g.inputs.len()));
    for (i, (e, name)) in g.inputs.iter().zip(&g.names).enumerate() {
        let mut el = |what: &str, v: String| put(format!("inputs{i}.{what}"), v);
        el("SemanticName", format!("{name:?}"));
        el("SemanticIndex", format!("{}", e.SemanticIndex));
        el("Format", format!("{}", e.Format.0));
        el("InputSlot", format!("{}", e.InputSlot));
        el("AlignedByteOffset", format!("{}", e.AlignedByteOffset));
        el("InputSlotClass", format!("{}", e.InputSlotClass.0));
        el(
            "InstanceDataStepRate",
            format!("{}", e.InstanceDataStepRate),
        );
    }
    put("strip_cut".into(), format!("{}", g.strip_cut.0));
    put("topology".into(), format!("{}", g.topology.0));
    put(
        "targets.NumRenderTargets".into(),
        format!("{}", g.targets.NumRenderTargets),
    );
    for (i, t) in g.targets.RTFormats.iter().enumerate() {
        put(format!("targets.RTFormats{i}"), format!("{}", t.0));
    }
    put("depth_format".into(), format!("{}", g.depth_format.0));
    put("samples".into(), format!("{:?}", g.samples));
    StateText {
        describe: g.describe(),
        semantic: semantic_of(g),
        fields: f,
    }
}

/// What a state does, leaving out what D3D12 ignores: the blend of
/// targets past the count (and, without independent blending, the
/// descriptions past the first), blend factors with blending off, the
/// logic operation with it off, formats past the count, depth write and
/// function with the test off, stencil masks and faces with it off, and
/// depth and stencil without a depth format. Two states with the same text
/// here make the same pipeline though their `describe()` may differ.
fn semantic_of(g: &GraphicsState) -> String {
    let n = (g.targets.NumRenderTargets as usize).min(8);
    let independent = g.blend.IndependentBlendEnable.as_bool();
    let mut s = format!("a2c {:?} targets {n}", g.blend.AlphaToCoverageEnable);
    for i in 0..n {
        let t = g.blend.RenderTarget[if independent { i } else { 0 }];
        s += &format!(
            " | rt{i} {} mask {}",
            g.targets.RTFormats[i].0, t.RenderTargetWriteMask
        );
        if t.BlendEnable.as_bool() {
            s += &format!(
                " blend {:?} {:?} {:?} {:?} {:?} {:?}",
                t.SrcBlend,
                t.DestBlend,
                t.BlendOp,
                t.SrcBlendAlpha,
                t.DestBlendAlpha,
                t.BlendOpAlpha
            );
        }
        if t.LogicOpEnable.as_bool() {
            s += &format!(" logic {:?}", t.LogicOp);
        }
    }
    s += &format!(
        " | samples {} {:?} | raster {:?}",
        g.sample_mask, g.samples, g.rasterizer
    );
    let d = &g.depth;
    if g.depth_format != DXGI_FORMAT_UNKNOWN {
        s += &format!(" | depth format {}", g.depth_format.0);
        if d.DepthEnable.as_bool() {
            s += &format!(" test {:?} {:?}", d.DepthWriteMask, d.DepthFunc);
        }
        if d.StencilEnable.as_bool() {
            s += &format!(
                " stencil {} {} {:?} {:?}",
                d.StencilReadMask, d.StencilWriteMask, d.FrontFace, d.BackFace
            );
        }
        if g.depth_bounds {
            s += " bounds";
        }
    }
    for (e, name) in g.inputs.iter().zip(&g.names) {
        s += &format!(
            " | {name:?} {} {} {} {} {} {}",
            e.SemanticIndex,
            e.Format.0,
            e.InputSlot,
            e.AlignedByteOffset,
            e.InputSlotClass.0,
            e.InstanceDataStepRate
        );
    }
    s += &format!(" | {} {}", g.strip_cut.0, g.topology.0);
    s
}

/// A decoded record's state as the runtime would describe it.
pub fn state_text(d: &GraphicsDesc) -> StateText {
    let mut g = state_of(d);
    title_convention(&mut g);
    text_of(&g)
}

/// The blend of targets D3D12 ignores, filled as most of the title's
/// streams fill it: the register image holds the driver's copy of target
/// 0's blend in every target and a write mask of 0 for targets past the
/// count; the title's descriptions mostly have D3D12's defaults there
/// (checked against a book of 83,562 pipelines on 2026-11-26: this rule
/// took the book's graphics pipelines with an identical record from 4,618
/// to 15,391, and those of them in the early list from 521 to 923; the rest
/// of the title's streams fill these fields in ways the
/// record does not tell, e.g. masks of 0, or blending on target 0 without
/// targets).
fn title_convention(g: &mut GraphicsState) {
    let default = GraphicsState::default().blend.RenderTarget[0];
    let n = (g.targets.NumRenderTargets as usize).min(8);
    let independent = g.blend.IndependentBlendEnable.as_bool();
    for (i, t) in g.blend.RenderTarget.iter_mut().enumerate() {
        let unused = if independent { i >= n } else { i > 0 };
        if unused {
            *t = default;
        }
    }
}

/// The key the runtime gives a title pipeline (`make::Pending::new`): each
/// shader's stage and logged hash (the first 16 hex digits of its
/// container's BLAKE3), in the order the title gave them, then the
/// graphics state's text (none for compute and plain descriptions).
pub fn title_key(shaders: &[(&str, [u8; 32])], describe: Option<&str>) -> String {
    let mut key = blake3::Hasher::new();
    for (stage, hash) in shaders {
        key.update(stage.as_bytes());
        key.update(blake3::Hash::from_bytes(*hash).to_hex()[..16].as_bytes());
    }
    key.update(describe.unwrap_or_default().as_bytes());
    key.finalize().to_hex()[..32].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use scarlett_game::pso_state::InputElement;

    fn desc() -> GraphicsDesc {
        let blend = TargetBlend {
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
        let face = StencilFace {
            fail: 1,
            depth_fail: 1,
            pass: 1,
            func: 8,
        };
        GraphicsDesc {
            mesh: false,
            alpha_to_coverage: false,
            independent_blend: false,
            targets_blend: [blend; 8],
            sample_mask: u32::MAX,
            fill_mode: 3,
            cull_mode: 3,
            front_ccw: true,
            depth_bias: -2,
            depth_bias_clamp: 0.0,
            slope_scaled_depth_bias: 1.5,
            depth_clip: true,
            conservative_raster: false,
            depth_enable: true,
            depth_write: 1,
            depth_func: 4,
            stencil_enable: false,
            stencil_read_mask: 0xff,
            stencil_write_mask: 0xff,
            front: face,
            back: face,
            depth_bounds: false,
            inputs: vec![InputElement {
                name: "POSITION".into(),
                index: 0,
                format: 6,
                slot: 0,
                offset: 0,
                class: 0,
                step_rate: 0,
            }],
            strip_cut: 0,
            topology: 3,
            num_targets: 1,
            target_formats: [28, 0, 0, 0, 0, 0, 0, 0],
            depth_format: 40,
            sample_count: 1,
            sample_quality: 0,
        }
    }

    #[test]
    fn the_title_key_is_the_runtime_s() {
        let h = *blake3::hash(b"container").as_bytes();
        let mut want = blake3::Hasher::new();
        want.update(b"vertex");
        want.update(blake3::hash(b"container").to_hex()[..16].as_bytes());
        want.update(b"state");
        assert_eq!(
            title_key(&[("vertex", h)], Some("state")),
            want.finalize().to_hex()[..32]
        );
        assert_ne!(
            title_key(&[("vertex", h)], None),
            title_key(&[("pixel", h)], None)
        );
    }

    #[test]
    fn ignored_targets_get_the_default_blend() {
        // The register image's copy: target 0's blend in every target,
        // masks of 0 past the count.
        let mut d = desc();
        for t in &mut d.targets_blend {
            (t.blend, t.src, t.dst) = (true, 5, 6);
        }
        for t in &mut d.targets_blend[1..] {
            t.write_mask = 0;
        }
        d.independent_blend = true;
        d.num_targets = 2;
        let s = state_text(&d);
        let field = |f: &str| {
            s.fields
                .iter()
                .find(|(k, _)| k == f)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(field("blend.rt1.BlendEnable"), "BOOL(1)");
        assert_eq!(field("blend.rt2.BlendEnable"), "BOOL(0)");
        assert_eq!(field("blend.rt2.RenderTargetWriteMask"), "15");
        // What the state does is the same either way.
        assert_eq!(s.semantic, semantic_of(&state_of(&d)));
        d.independent_blend = false;
        let s = state_text(&d);
        let field = |f: &str| {
            s.fields
                .iter()
                .find(|(k, _)| k == f)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(field("blend.rt0.BlendEnable"), "BOOL(1)");
        assert_eq!(field("blend.rt1.BlendEnable"), "BOOL(0)");
    }
}
