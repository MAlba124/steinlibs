use super::*;
use vk::AccessFlags2 as A;
use vk::PipelineStageFlags2 as P;

#[test]
fn a_barrier_publishes_writes_to_reads_and_writes() {
    let b = barrier(
        Stage::TRANSFER,
        Stage::FRAGMENT | Stage::COMPUTE,
        Hazard::NONE,
    );
    assert_eq!(b.src_stage_mask, P::ALL_TRANSFER);
    assert_eq!(b.src_access_mask, A::TRANSFER_WRITE);
    assert_eq!(b.dst_stage_mask, P::FRAGMENT_SHADER | P::COMPUTE_SHADER);
    assert_eq!(b.dst_access_mask, A::SHADER_READ | A::SHADER_WRITE);
}

#[test]
fn draw_args_hazard_waits_for_the_command_processor() {
    let b = barrier(Stage::COMPUTE, Stage::VERTEX, Hazard::DRAW_ARGS);
    assert!(b.dst_stage_mask.contains(P::DRAW_INDIRECT));
    assert!(b.dst_access_mask.contains(A::INDIRECT_COMMAND_READ));
}

#[test]
fn every_stage_bit_maps_to_something_except_present() {
    for bit in 0..9 {
        let s = Stage(1 << bit);
        let (p, _, _) = stages(s);
        assert_eq!(p == P::NONE, s == Stage::PRESENT, "stage bit {bit}");
    }
}

#[test]
fn unsupported_textures_are_refused_before_the_driver() {
    let ok = TextureDesc::d2([4, 4], Format::Rgba8Unorm, Usage::SAMPLED);
    assert!(image_info(&ok).is_ok());
    for bad in [
        TextureDesc {
            size: [0, 4, 1],
            ..ok
        },
        TextureDesc { mips: 0, ..ok },
        TextureDesc { layers: 2, ..ok },
        TextureDesc { samples: 4, ..ok },
        TextureDesc {
            usage: Usage(0),
            ..ok
        },
    ] {
        assert!(image_info(&bad).is_err(), "{bad:?}");
    }
}
