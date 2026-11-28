use super::*;

#[test]
fn a_bar_stretches_across_and_reserves_its_height() {
    let b = bar(24, false, "panel");
    assert_eq!(check(&b), Ok(()));
    assert!(b.anchor.has(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT));
    assert!(!b.anchor.has(Anchor::BOTTOM));
    assert_eq!(b.exclusive, 24);
    // The compositor fills in the width, the height is ours.
    assert_eq!(configured(&b, 2560, 0), [2560, 24]);
    assert_eq!(configured(&b, 2560, 24), [2560, 24]);
}

#[test]
fn a_bottom_bar_anchors_the_bottom_edge() {
    let b = bar(30, true, "panel");
    assert!(b.anchor.has(Anchor::BOTTOM));
    assert!(!b.anchor.has(Anchor::TOP));
}

#[test]
fn a_zero_axis_without_both_edges_is_refused() {
    let mut l = Layer {
        size: [0, 20],
        anchor: Anchor::TOP | Anchor::LEFT,
        ..Layer::default()
    };
    assert_eq!(check(&l), Err(LayerError::ZeroWidth));
    l.size = [100, 0];
    assert_eq!(check(&l), Err(LayerError::ZeroHeight));
    l.anchor = Anchor::TOP | Anchor::BOTTOM;
    assert_eq!(check(&l), Ok(()));
}

#[test]
fn stray_anchor_bits_are_refused() {
    let l = Layer {
        size: [10, 10],
        anchor: Anchor(0x10),
        ..Layer::default()
    };
    assert_eq!(check(&l), Err(LayerError::Anchor(0x10)));
}

#[test]
fn an_unanchored_surface_keeps_its_own_size() {
    let l = Layer {
        size: [640, 400],
        ..Layer::default()
    };
    assert_eq!(check(&l), Ok(()));
    assert_eq!(configured(&l, 0, 0), [640, 400]);
}

#[test]
fn the_default_spec_is_itself_invalid() {
    // Zero size with nothing anchored, so a scene has to say what it wants.
    assert_eq!(check(&Layer::default()), Err(LayerError::ZeroWidth));
}

#[test]
fn on_demand_falls_back_before_version_four() {
    assert_eq!(keys_for(Keys::OnDemand, 3), Keys::Exclusive);
    assert_eq!(keys_for(Keys::OnDemand, 4), Keys::OnDemand);
    assert_eq!(keys_for(Keys::None, 1), Keys::None);
    assert_eq!(keys_for(Keys::Exclusive, 5), Keys::Exclusive);
}

#[test]
fn configured_never_returns_zero() {
    // Only reachable with a spec that skipped check, still no 0x0 buffer.
    assert_eq!(configured(&Layer::default(), 0, 0), [1, 1]);
}
