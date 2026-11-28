use super::quad::{GpuQuad, QuadKind, QuadRoot};
use super::*;

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn f32_at(b: &[u8], at: usize) -> f32 {
    f32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn stored<T: GpuLayout>(v: &T) -> Vec<u8> {
    let mut out = vec![0xAA; T::SIZE];
    v.store(&mut out);
    out
}

#[test]
fn handles_pack_index_and_generation() {
    let h = Handle::new(Handle::MAX_INDEX, 255);
    assert_eq!(h.index(), Handle::MAX_INDEX);
    assert_eq!(h.generation(), 255);
    let h = Handle::new(Handle::MAX_INDEX + 5, 3);
    assert_eq!(
        h.index(),
        4,
        "index is masked, not spilled into the generation"
    );
    assert_eq!(h.generation(), 3);
}

// These offsets are the contract with shaders/quad.vert. Checked against
// spirv-dis of the compiled shader when the layout was written.
#[test]
fn gpu_quad_matches_the_shader_layout() {
    let q = GpuQuad {
        rect: [1.0, 2.0, 3.0, 4.0],
        clip: [5.0, 6.0, 7.0, 8.0],
        uv: [0.25, 0.5, 0.125, 0.0625],
        color: 0x80FF_2010,
        slot: TextureSlot(7),
        kind: QuadKind::Coverage,
        radius: [1, 2, 3, 4],
        stroke: 5,
    };
    let b = stored(&q);
    assert_eq!(b.len(), 64);
    for (i, want) in [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0].iter().enumerate() {
        assert_eq!(f32_at(&b, i * 4), *want, "rect/clip lane {i}");
    }
    assert_eq!(f32_at(&b, 32), 0.25);
    assert_eq!(f32_at(&b, 44), 0.0625);
    assert_eq!(u32_at(&b, 48), 0x80FF_2010);
    assert_eq!(u32_at(&b, 52), 7);
    assert_eq!(u32_at(&b, 56), 1 | 5 << 8, "kind, then stroke");
    assert_eq!(u32_at(&b, 60), 0x0403_0201, "radii, top left lowest");
}

#[test]
fn quad_root_matches_the_push_constant_block() {
    let r = QuadRoot {
        quads: GpuPtr(0x1122_3344_5566_7788),
        atlas: GpuPtr(0x99AA_BBCC_DDEE_FF00),
        size: [800.0, 600.0],
        atlas_size: [512, 256],
    };
    let b = stored(&r);
    assert_eq!(b.len(), 32);
    let u64_at = |at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
    assert_eq!(u64_at(0), 0x1122_3344_5566_7788);
    assert_eq!(u64_at(8), 0x99AA_BBCC_DDEE_FF00);
    assert_eq!(f32_at(&b, 16), 800.0);
    assert_eq!(f32_at(&b, 20), 600.0);
    assert_eq!(u32_at(&b, 24), 512);
    assert_eq!(u32_at(&b, 28), 256);
}

#[test]
fn roots_are_copied_in_order_and_resolvable() {
    let mut c = Commands::with_capacity(16, 256, 2);
    c.draw(&7u32, 3, 1);
    c.dispatch(&[1.0f32, 2.0], [4, 1, 1]);
    c.draw(&(), 3, 1);
    let roots: Vec<RootRef> = c
        .ops()
        .iter()
        .filter_map(|op| match op {
            Op::Draw { root, .. } | Op::Dispatch { root, .. } => Some(*root),
            _ => None,
        })
        .collect();
    assert_eq!(roots.len(), 3);
    assert_eq!(c.root(roots[0]), 7u32.to_le_bytes());
    assert_eq!(roots[1].at, 4);
    assert_eq!(f32_at(c.root(roots[1]), 4), 2.0);
    assert_eq!(roots[2].len, 0, "unit root means no push constants");
    assert!(c.root(roots[2]).is_empty());
}

#[test]
fn passes_are_referenced_by_index() {
    let mut c = Commands::default();
    let pass = Pass {
        color: [None; MAX_COLOR],
        depth: None,
        size: [640, 480],
    };
    c.begin_pass(pass);
    c.end_pass();
    c.begin_pass(Pass {
        size: [1, 1],
        ..pass
    });
    assert_eq!(c.ops()[2], Op::BeginPass(1));
    assert_eq!(c.pass(1).map(|p| p.size), Some([1, 1]));
    assert_eq!(c.pass(2), None);
}

#[test]
fn ops_stay_small() {
    assert!(size_of::<Op>() <= 64, "Op is {} bytes", size_of::<Op>());
}

#[test]
fn stage_sets_combine() {
    let s = Stage::TRANSFER | Stage::FRAGMENT;
    assert!(s.contains(Stage::FRAGMENT));
    assert!(!s.contains(Stage::VERTEX));
    assert!(Stage::ALL.contains(s | Stage::PRESENT));
}

#[test]
fn quad_record_skips_empty_work() {
    let pass = quad::QuadPass {
        pipeline: Pipeline(Handle::new(0, 0)),
        format: Format::Bgra8Unorm,
    };
    let slice = |len| Slice::<GpuQuad> {
        heap: Heap(Handle::new(0, 0)),
        at: 0,
        len,
        gpu: GpuPtr(0x1000),
        _t: core::marker::PhantomData,
    };
    let mut c = Commands::default();
    let none = quad::QuadAtlas::NONE;
    pass.record(&mut c, slice(0), [800, 600], none);
    pass.record(&mut c, slice(5), [0, 600], none);
    assert!(c.ops().is_empty());
    pass.record(&mut c, slice(5), [800, 600], none);
    assert!(matches!(
        c.ops().last(),
        Some(Op::Draw {
            vertices: 6,
            instances: 5,
            ..
        })
    ));
}
