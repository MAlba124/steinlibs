use super::*;

fn fmt(f: vk::Format, cs: vk::ColorSpaceKHR) -> vk::SurfaceFormatKHR {
    vk::SurfaceFormatKHR {
        format: f,
        color_space: cs,
    }
}

#[test]
fn formats_are_taken_in_the_order_asked_and_only_srgb_nonlinear() {
    let srgb = vk::ColorSpaceKHR::SRGB_NONLINEAR;
    let have = [
        fmt(vk::Format::R8G8B8A8_UNORM, srgb),
        fmt(
            vk::Format::B8G8R8A8_UNORM,
            vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT,
        ),
        fmt(vk::Format::B8G8R8A8_UNORM, srgb),
    ];
    let want = [Format::Bgra8Unorm, Format::Rgba8Unorm];
    assert_eq!(pick_format(&want, &have), Some(have[2]));
    assert_eq!(pick_format(&[Format::Rgba16Float], &have), None);
}

#[test]
fn modes_fall_back_to_fifo() {
    let have = [vk::PresentModeKHR::FIFO, vk::PresentModeKHR::MAILBOX];
    assert_eq!(
        pick_mode(PresentMode::Mailbox, &have),
        vk::PresentModeKHR::MAILBOX
    );
    assert_eq!(
        pick_mode(PresentMode::Immediate, &have),
        vk::PresentModeKHR::FIFO
    );
}

#[test]
fn image_counts_clamp_to_the_surface() {
    assert_eq!(image_count(2, 8, 3), 3);
    assert_eq!(image_count(4, 8, 3), 4);
    assert_eq!(image_count(2, 2, 3), 2);
    assert_eq!(image_count(2, 0, 9), 9, "a max of 0 is unbounded");
}

#[test]
fn a_surface_without_its_own_extent_takes_the_size_clamped() {
    let mut caps = vk::SurfaceCapabilitiesKHR {
        current_extent: vk::Extent2D {
            width: u32::MAX,
            height: u32::MAX,
        },
        min_image_extent: vk::Extent2D {
            width: 1,
            height: 1,
        },
        max_image_extent: vk::Extent2D {
            width: 4096,
            height: 4096,
        },
        ..Default::default()
    };
    assert_eq!(
        extent(&caps, [800, 9000]),
        vk::Extent2D {
            width: 800,
            height: 4096
        }
    );
    caps.current_extent = vk::Extent2D {
        width: 640,
        height: 480,
    };
    assert_eq!(extent(&caps, [800, 600]), caps.current_extent);
}

#[test]
fn opaque_is_preferred_then_whatever_there_is() {
    use vk::CompositeAlphaFlagsKHR as C;
    assert_eq!(composite(C::PRE_MULTIPLIED | C::OPAQUE, false), C::OPAQUE);
    assert_eq!(composite(C::PRE_MULTIPLIED, false), C::PRE_MULTIPLIED);
}

#[test]
fn a_surface_with_alpha_prefers_premultiplied() {
    use vk::CompositeAlphaFlagsKHR as C;
    let all = C::OPAQUE | C::INHERIT | C::PRE_MULTIPLIED | C::POST_MULTIPLIED;
    assert_eq!(composite(all, true), C::PRE_MULTIPLIED);
    assert_eq!(composite(C::INHERIT | C::OPAQUE, true), C::INHERIT);
    assert_eq!(composite(C::OPAQUE, true), C::OPAQUE, "no alpha to have");
}

#[test]
fn damage_is_clipped_to_the_image_and_capped() {
    let mut out = [vk::RectLayerKHR::default(); MAX_DAMAGE];
    let n = damage_rects(
        &[[10, 10, 100, 100], [790, 590, 50, 50], [900, 0, 5, 5]],
        [800, 600],
        &mut out,
    );
    assert_eq!(n, 2, "the off image rect is dropped");
    assert_eq!(
        out[0].extent,
        vk::Extent2D {
            width: 100,
            height: 100
        }
    );
    assert_eq!(
        out[1].extent,
        vk::Extent2D {
            width: 10,
            height: 10
        }
    );
    let many = [[0, 0, 1, 1]; MAX_DAMAGE + 4];
    assert_eq!(damage_rects(&many, [800, 600], &mut out), MAX_DAMAGE);
}
