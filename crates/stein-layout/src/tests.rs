//! Layout is a pure transform, so every test here is a table in and a table out.
//! No platform, no font, no allocator, no frame loop.

use super::*;

/// Declare columns on the stack and a table over them. A macro because the table
/// borrows storage that has to outlive it.
macro_rules! table {
    ($t:ident, $n:expr) => {
        let mut key = [0u64; $n];
        let mut parent = [0u32; $n];
        let mut subtree_end = [0u32; $n];
        let mut size = [[SizeSpec::px(0.0); 2]; $n];
        let mut axis = [Axis::X; $n];
        let mut measured = [[0.0f32; 2]; $n];
        let mut pad = [Insets::ZERO; $n];
        let mut opts = [Options::default(); $n];
        let mut computed = [[0.0f32; 2]; $n];
        let mut content = [[0.0f32; 2]; $n];
        let mut rect = [Rect::default(); $n];
        let mut clip = [Rect::default(); $n];
        let mut stack = [0u32; $n];
        let storage = LayoutStorage {
            key: &mut key,
            parent: &mut parent,
            subtree_end: &mut subtree_end,
            size: &mut size,
            axis: &mut axis,
            measured: &mut measured,
            pad: &mut pad,
            opts: &mut opts,
            computed: &mut computed,
            content: &mut content,
            rect: &mut rect,
            clip: &mut clip,
            stack: &mut stack,
        };
        #[allow(unused_mut)]
        let mut $t = BoxTable::new(storage).expect("columns agree");
    };
}

fn approx(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.01
}

macro_rules! assert_rect {
    ($t:expr, $i:expr, $x0:expr, $y0:expr, $x1:expr, $y1:expr) => {{
        let r = $t.rect($i).expect("live box");
        assert!(
            approx(r.min[X], $x0)
                && approx(r.min[Y], $y0)
                && approx(r.max[X], $x1)
                && approx(r.max[Y], $y1),
            "box {} was {:?}, expected [{}, {}] to [{}, {}]",
            $i,
            r,
            $x0,
            $y0,
            $x1,
            $y1
        );
    }};
}

#[test]
fn fixed_children_stack_along_the_layout_axis() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(300.0))
            .h(SizeSpec::px(100.0)),
    )
    .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf()
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(100.0)),
    )
    .expect("fits");
    t.leaf(
        2,
        BoxSpec::leaf().w(SizeSpec::px(50.0)).h(SizeSpec::px(100.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([300.0, 100.0]).expect("closed");

    assert_rect!(t, 0, 0.0, 0.0, 300.0, 100.0);
    assert_rect!(t, 1, 0.0, 0.0, 100.0, 100.0);
    assert_rect!(t, 2, 100.0, 0.0, 150.0, 100.0);
}

#[test]
fn cross_axis_children_do_not_accumulate() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::Y)
            .w(SizeSpec::px(200.0))
            .h(SizeSpec::px(300.0)),
    )
    .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf().w(SizeSpec::px(50.0)).h(SizeSpec::px(100.0)),
    )
    .expect("fits");
    t.leaf(
        2,
        BoxSpec::leaf().w(SizeSpec::px(50.0)).h(SizeSpec::px(100.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([200.0, 300.0]).expect("closed");

    // Stacked on Y, both pinned to x = 0 rather than side by side.
    assert_rect!(t, 1, 0.0, 0.0, 50.0, 100.0);
    assert_rect!(t, 2, 0.0, 100.0, 50.0, 200.0);
}

#[test]
fn children_sum_sizes_the_parent() {
    table!(t, 8);
    t.open(0, BoxSpec::container(Axis::X)).expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf().w(SizeSpec::px(60.0)).h(SizeSpec::px(20.0)),
    )
    .expect("fits");
    t.leaf(
        2,
        BoxSpec::leaf().w(SizeSpec::px(40.0)).h(SizeSpec::px(30.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([1000.0, 1000.0]).expect("closed");

    // Sum along the layout axis, largest across it.
    assert_rect!(t, 0, 0.0, 0.0, 100.0, 30.0);
}

#[test]
fn parent_fraction_resolves_against_the_parent() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(400.0))
            .h(SizeSpec::px(100.0)),
    )
    .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf()
            .w(SizeSpec::fraction(0.25))
            .h(SizeSpec::px(100.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([400.0, 100.0]).expect("closed");

    assert_rect!(t, 1, 0.0, 0.0, 100.0, 100.0);
}

#[test]
fn root_fraction_resolves_against_the_viewport() {
    table!(t, 4);
    t.leaf(
        0,
        BoxSpec::leaf()
            .w(SizeSpec::fraction(0.5))
            .h(SizeSpec::fraction(0.25)),
    )
    .expect("fits");
    t.solve([800.0, 400.0]).expect("closed");

    assert_rect!(t, 0, 0.0, 0.0, 400.0, 100.0);
}

#[test]
fn grow_splits_the_remainder_evenly() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(500.0))
            .h(SizeSpec::px(100.0)),
    )
    .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf().w(SizeSpec::px(100.0)).h(SizeSpec::grow()),
    )
    .expect("fits");
    t.leaf(2, BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::grow()))
        .expect("fits");
    t.leaf(3, BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::grow()))
        .expect("fits");
    t.close().expect("balanced");
    t.solve([500.0, 100.0]).expect("closed");

    assert_rect!(t, 1, 0.0, 0.0, 100.0, 100.0);
    assert_rect!(t, 2, 100.0, 0.0, 300.0, 100.0);
    assert_rect!(t, 3, 300.0, 0.0, 500.0, 100.0);
}

#[test]
fn grow_with_nothing_left_is_zero_not_negative() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf().w(SizeSpec::px(200.0)).h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.leaf(2, BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::px(10.0)))
        .expect("fits");
    t.close().expect("balanced");
    t.solve([100.0, 10.0]).expect("closed");

    let grown = t.rect(2).expect("live");
    assert!(grown.width() >= 0.0, "width went negative: {grown:?}");
}

#[test]
fn overflow_shrinks_the_least_strict_child() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    // Both want 100, only 100 is available, so 100 has to come off somewhere.
    t.leaf(
        1,
        BoxSpec::leaf()
            .w(SizeSpec::px(100.0).strict())
            .h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.leaf(
        2,
        BoxSpec::leaf()
            .w(SizeSpec::px(100.0).soft())
            .h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([100.0, 10.0]).expect("closed");

    assert!(
        approx(t.rect(1).expect("live").width(), 100.0),
        "strict child kept its size"
    );
    assert!(
        approx(t.rect(2).expect("live").width(), 0.0),
        "soft child gave way entirely"
    );
}

#[test]
fn overflow_is_shared_when_both_children_give() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf()
            .w(SizeSpec::px(100.0).soft())
            .h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.leaf(
        2,
        BoxSpec::leaf()
            .w(SizeSpec::px(100.0).soft())
            .h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([100.0, 10.0]).expect("closed");

    // Equal size and equal give, so equal loss.
    assert!(approx(t.rect(1).expect("live").width(), 50.0));
    assert!(approx(t.rect(2).expect("live").width(), 50.0));
}

#[test]
fn all_strict_children_overflow_rather_than_clip() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf()
            .w(SizeSpec::px(80.0).strict())
            .h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.leaf(
        2,
        BoxSpec::leaf()
            .w(SizeSpec::px(80.0).strict())
            .h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([100.0, 10.0]).expect("closed");

    // Visibly wrong beats silently wrong. Nothing was clipped or squeezed.
    assert!(approx(t.rect(1).expect("live").width(), 80.0));
    assert!(approx(t.rect(2).expect("live").width(), 80.0));
    assert!(t.rect(2).expect("live").max[X] > t.rect(0).expect("live").max[X]);
}

#[test]
fn measured_extents_come_from_the_caller() {
    table!(t, 8);
    t.open(0, BoxSpec::container(Axis::X)).expect("fits");
    t.leaf(1, BoxSpec::leaf().measured(73.5, 17.0))
        .expect("fits");
    t.close().expect("balanced");
    t.solve([1000.0, 1000.0]).expect("closed");

    assert!(approx(t.rect(1).expect("live").width(), 73.5));
    assert!(approx(t.rect(1).expect("live").height(), 17.0));
    assert!(
        approx(t.rect(0).expect("live").width(), 73.5),
        "parent sized to the text"
    );
}

#[test]
fn fraction_inside_a_sum_parent_is_excluded_then_resolved() {
    table!(t, 8);
    t.open(0, BoxSpec::container(Axis::X).h(SizeSpec::px(50.0)))
        .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf().w(SizeSpec::px(200.0)).h(SizeSpec::px(50.0)),
    )
    .expect("fits");
    t.leaf(
        2,
        BoxSpec::leaf()
            .w(SizeSpec::fraction(0.5))
            .h(SizeSpec::px(50.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([1000.0, 50.0]).expect("closed");

    // Parent sized from the child that claimed space, ignoring the fraction child.
    assert!(approx(t.rect(0).expect("live").width(), 200.0));
    // Fraction child then resolved against that, and allowed to overflow rather
    // than being squeezed for space it never asked the parent to reserve.
    assert!(approx(t.rect(2).expect("live").width(), 100.0));
}

#[test]
fn padding_insets_children() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(200.0))
            .h(SizeSpec::px(100.0))
            .pad(10.0),
    )
    .expect("fits");
    t.leaf(1, BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::grow()))
        .expect("fits");
    t.close().expect("balanced");
    t.solve([200.0, 100.0]).expect("closed");

    // Grow fills the box minus its padding, on both axes.
    assert_rect!(t, 1, 10.0, 10.0, 190.0, 90.0);
}

#[test]
fn gap_separates_siblings_and_is_not_charged_after_the_last() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(220.0))
            .h(SizeSpec::px(50.0))
            .gap(10.0),
    )
    .expect("fits");
    for k in 1..=3u64 {
        t.leaf(k, BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::grow()))
            .expect("fits");
    }
    t.close().expect("balanced");
    t.solve([220.0, 50.0]).expect("closed");

    // 220 minus two gaps of 10 leaves 200 over three children.
    assert_rect!(t, 1, 0.0, 0.0, 66.67, 50.0);
    assert_rect!(t, 2, 76.67, 0.0, 143.33, 50.0);
    assert_rect!(t, 3, 153.33, 0.0, 220.0, 50.0);
}

#[test]
fn padding_and_gap_grow_a_sum_sized_parent() {
    table!(t, 8);
    t.open(0, BoxSpec::container(Axis::X).pad(5.0).gap(8.0))
        .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf().w(SizeSpec::px(30.0)).h(SizeSpec::px(20.0)),
    )
    .expect("fits");
    t.leaf(
        2,
        BoxSpec::leaf().w(SizeSpec::px(30.0)).h(SizeSpec::px(20.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([1000.0, 1000.0]).expect("closed");

    // 30 + 8 + 30, plus 5 of padding on each side.
    assert!(approx(t.rect(0).expect("live").width(), 78.0));
    // Cross axis takes the tallest child plus padding, and no gap.
    assert!(approx(t.rect(0).expect("live").height(), 30.0));
}

#[test]
fn gap_does_not_apply_across_the_cross_axis() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::Y)
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(100.0))
            .gap(20.0),
    )
    .expect("fits");
    t.leaf(1, BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::px(10.0)))
        .expect("fits");
    t.leaf(2, BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::px(10.0)))
        .expect("fits");
    t.close().expect("balanced");
    t.solve([100.0, 100.0]).expect("closed");

    // Layout axis is Y, so X is untouched by the gap and both fill the width.
    assert!(approx(t.rect(1).expect("live").width(), 100.0));
    assert!(approx(t.rect(2).expect("live").width(), 100.0));
    assert!(
        approx(t.rect(2).expect("live").min[Y], 30.0),
        "10 tall plus a 20 gap"
    );
}

#[test]
fn deep_nesting_positions_absolutely() {
    table!(t, 16);
    t.open(
        0,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(400.0))
            .h(SizeSpec::px(400.0)),
    )
    .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf().w(SizeSpec::px(50.0)).h(SizeSpec::px(400.0)),
    )
    .expect("fits");
    t.open(
        2,
        BoxSpec::container(Axis::Y)
            .w(SizeSpec::px(350.0))
            .h(SizeSpec::px(400.0)),
    )
    .expect("fits");
    t.leaf(
        3,
        BoxSpec::leaf().w(SizeSpec::px(350.0)).h(SizeSpec::px(30.0)),
    )
    .expect("fits");
    t.open(
        4,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(350.0))
            .h(SizeSpec::px(40.0)),
    )
    .expect("fits");
    t.leaf(
        5,
        BoxSpec::leaf().w(SizeSpec::px(10.0)).h(SizeSpec::px(40.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.close().expect("balanced");
    t.close().expect("balanced");
    t.solve([400.0, 400.0]).expect("closed");

    assert_rect!(t, 2, 50.0, 0.0, 400.0, 400.0);
    assert_rect!(t, 3, 50.0, 0.0, 400.0, 30.0);
    assert_rect!(t, 4, 50.0, 30.0, 400.0, 70.0);
    // Three levels down, absolute position is the sum of the chain.
    assert_rect!(t, 5, 50.0, 30.0, 60.0, 70.0);
}

#[test]
fn subtree_end_bounds_each_subtree() {
    table!(t, 16);
    t.open(0, BoxSpec::container(Axis::X)).expect("fits");
    t.open(1, BoxSpec::container(Axis::Y)).expect("fits");
    t.leaf(2, BoxSpec::leaf()).expect("fits");
    t.leaf(3, BoxSpec::leaf()).expect("fits");
    t.close().expect("balanced");
    t.leaf(4, BoxSpec::leaf()).expect("fits");
    t.close().expect("balanced");

    assert_eq!(t.len(), 5);
    assert_eq!(t.s.subtree_end[0], 5, "root spans everything");
    assert_eq!(
        t.s.subtree_end[1], 4,
        "inner container spans its two children"
    );
    assert_eq!(t.s.subtree_end[2], 3);
    assert_eq!(t.s.subtree_end[4], 5);
}

#[test]
fn hit_testing_prefers_the_last_painted() {
    table!(t, 8);
    t.open(
        10,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(100.0)),
    )
    .expect("fits");
    t.leaf(
        20,
        BoxSpec::leaf()
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(100.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([100.0, 100.0]).expect("closed");

    // Child is painted after the parent, so it wins an overlapping point.
    assert_eq!(t.hit([50.0, 50.0]), Some(20));
    assert_eq!(t.hit([150.0, 50.0]), None);
}

#[test]
fn a_passive_box_lets_the_pointer_through_to_the_one_under_it() {
    table!(t, 8);
    let square = |spec: BoxSpec| spec.w(SizeSpec::px(100.0)).h(SizeSpec::px(100.0));
    t.open(10, square(BoxSpec::container(Axis::X))).expect("fits");
    t.leaf(20, square(BoxSpec::leaf()).passive()).expect("fits");
    t.close().expect("balanced");
    t.leaf(30, square(BoxSpec::leaf().floating([0.0, 0.0])).passive()).expect("fits");
    t.solve([100.0, 100.0]).expect("closed");
    assert_eq!(t.hit([50.0, 50.0]), Some(10), "both passive boxes are skipped");
}

#[test]
fn rects_come_out_in_painter_order() {
    table!(t, 8);
    t.open(
        10,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.leaf(
        20,
        BoxSpec::leaf().w(SizeSpec::px(50.0)).h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.leaf(
        30,
        BoxSpec::leaf().w(SizeSpec::px(50.0)).h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([100.0, 10.0]).expect("closed");

    let keys: std::vec::Vec<u64> = t.rects().map(|(k, _)| k).collect();
    assert_eq!(
        keys,
        std::vec![10, 20, 30],
        "parent before children, siblings in order"
    );
}

#[test]
fn reset_reuses_the_table() {
    table!(t, 8);
    t.leaf(
        1,
        BoxSpec::leaf().w(SizeSpec::px(10.0)).h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.solve([100.0, 100.0]).expect("closed");
    assert_eq!(t.len(), 1);

    t.reset();
    assert_eq!(t.len(), 0);
    t.leaf(
        2,
        BoxSpec::leaf().w(SizeSpec::px(20.0)).h(SizeSpec::px(20.0)),
    )
    .expect("fits");
    t.solve([100.0, 100.0]).expect("closed");
    assert_eq!(t.key(0), Some(2), "previous frame left nothing behind");
    assert!(approx(t.rect(0).expect("live").width(), 20.0));
}

#[test]
fn capacity_is_a_hard_ceiling() {
    table!(t, 2);
    assert!(t.leaf(1, BoxSpec::leaf()).is_ok());
    assert!(t.leaf(2, BoxSpec::leaf()).is_ok());
    assert_eq!(t.leaf(3, BoxSpec::leaf()), Err(LayoutError::TooManyBoxes));
}

#[test]
fn unbalanced_close_is_refused() {
    table!(t, 4);
    assert_eq!(t.close(), Err(LayoutError::Unbalanced));
}

#[test]
fn solving_with_boxes_open_is_refused() {
    table!(t, 4);
    t.open(1, BoxSpec::container(Axis::X)).expect("fits");
    assert_eq!(t.solve([10.0, 10.0]), Err(LayoutError::StillOpen));
}

#[test]
fn mismatched_columns_are_refused() {
    let mut key = [0u64; 4];
    let mut parent = [0u32; 3];
    let mut subtree_end = [0u32; 4];
    let mut size = [[SizeSpec::px(0.0); 2]; 4];
    let mut axis = [Axis::X; 4];
    let mut measured = [[0.0f32; 2]; 4];
    let mut pad = [Insets::ZERO; 4];
    let mut opts = [Options::default(); 4];
    let mut computed = [[0.0f32; 2]; 4];
    let mut content = [[0.0f32; 2]; 4];
    let mut rect = [Rect::default(); 4];
    let mut clip = [Rect::default(); 4];
    let mut stack = [0u32; 4];
    let storage = LayoutStorage {
        key: &mut key,
        parent: &mut parent,
        subtree_end: &mut subtree_end,
        size: &mut size,
        axis: &mut axis,
        measured: &mut measured,
        pad: &mut pad,
        opts: &mut opts,
        computed: &mut computed,
        content: &mut content,
        rect: &mut rect,
        clip: &mut clip,
        stack: &mut stack,
    };
    assert_eq!(
        BoxTable::new(storage).err(),
        Some(LayoutError::LengthMismatch)
    );
}

#[test]
fn cross_axis_alignment_centres_and_ends() {
    for (align, want_y) in [
        (Align::Start, 0.0),
        (Align::Center, 40.0),
        (Align::End, 80.0),
    ] {
        table!(t, 8);
        t.open(
            0,
            BoxSpec::container(Axis::X)
                .w(SizeSpec::px(200.0))
                .h(SizeSpec::px(100.0))
                .align(align),
        )
        .expect("fits");
        t.leaf(
            1,
            BoxSpec::leaf().w(SizeSpec::px(50.0)).h(SizeSpec::px(20.0)),
        )
        .expect("fits");
        t.close().expect("balanced");
        t.solve([200.0, 100.0]).expect("closed");

        let r = t.rect(1).expect("live");
        assert!(approx(r.min[Y], want_y), "{align:?} put it at {}", r.min[Y]);
        assert!(
            approx(r.min[X], 0.0),
            "alignment must not touch the layout axis"
        );
    }
}

#[test]
fn justify_distributes_leftover_on_the_layout_axis() {
    for (justify, want_x) in [
        (Align::Start, 0.0),
        (Align::Center, 75.0),
        (Align::End, 150.0),
    ] {
        table!(t, 8);
        t.open(
            0,
            BoxSpec::container(Axis::X)
                .w(SizeSpec::px(200.0))
                .h(SizeSpec::px(50.0))
                .justify(justify),
        )
        .expect("fits");
        t.leaf(
            1,
            BoxSpec::leaf().w(SizeSpec::px(50.0)).h(SizeSpec::px(50.0)),
        )
        .expect("fits");
        t.close().expect("balanced");
        t.solve([200.0, 50.0]).expect("closed");

        let r = t.rect(1).expect("live");
        assert!(
            approx(r.min[X], want_x),
            "{justify:?} put it at {}",
            r.min[X]
        );
    }
}

#[test]
fn justify_is_a_noop_when_a_child_grows() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(200.0))
            .h(SizeSpec::px(50.0))
            .justify(Align::Center),
    )
    .expect("fits");
    t.leaf(1, BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::px(50.0)))
        .expect("fits");
    t.close().expect("balanced");
    t.solve([200.0, 50.0]).expect("closed");

    // Growing consumed the slack, so there is nothing left to distribute.
    assert_rect!(t, 1, 0.0, 0.0, 200.0, 50.0);
}

#[test]
fn min_wins_over_the_overflow_solver() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(50.0))
            .h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf()
            .w(SizeSpec::grow().at_least(100.0))
            .h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([50.0, 10.0]).expect("closed");

    // Only 50 available, floor is 100, so it overflows rather than being crushed.
    assert!(approx(t.rect(1).expect("live").width(), 100.0));
}

#[test]
fn max_caps_a_growing_child() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(500.0))
            .h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf()
            .w(SizeSpec::grow().at_most(120.0))
            .h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([500.0, 10.0]).expect("closed");

    assert!(approx(t.rect(1).expect("live").width(), 120.0));
}

#[test]
fn clip_confines_the_subtree_without_moving_it() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::Y)
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(50.0))
            .clip(),
    )
    .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf()
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(200.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([400.0, 400.0]).expect("closed");

    // The rect is untouched; only the visible region shrinks. A renderer needs
    // both, since the geometry is what scrolls and the clip is what scissors.
    assert!(approx(t.rect(1).expect("live").height(), 200.0));
    let c = t.clip(1).expect("live");
    assert_eq!(
        c,
        Rect {
            min: [0.0, 0.0],
            max: [100.0, 50.0]
        }
    );
}

#[test]
fn hit_testing_ignores_clipped_away_geometry() {
    table!(t, 8);
    t.open(
        10,
        BoxSpec::container(Axis::Y)
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(50.0))
            .clip(),
    )
    .expect("fits");
    t.leaf(
        20,
        BoxSpec::leaf()
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(200.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([400.0, 400.0]).expect("closed");

    assert_eq!(t.hit([50.0, 25.0]), Some(20), "inside the visible part");
    // Inside the child's rect but scrolled out of view. Without the clip check
    // this row would swallow clicks it cannot be seen to receive.
    assert_eq!(t.hit([50.0, 120.0]), None);
}

#[test]
fn scroll_shifts_children_and_content_reports_the_overflow() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::Y)
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(50.0))
            .clip()
            .scroll([0.0, 30.0]),
    )
    .expect("fits");
    for k in 1..=3u64 {
        t.leaf(k, BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::px(20.0)))
            .expect("fits");
    }
    t.close().expect("balanced");
    t.solve([400.0, 400.0]).expect("closed");

    assert!(approx(t.rect(1).expect("live").min[Y], -30.0));
    assert!(approx(t.rect(2).expect("live").min[Y], -10.0));
    assert!(approx(t.rect(3).expect("live").min[Y], 10.0));

    // 3 rows of 20 want 60, the box is 50, so there are 10 to scroll through.
    let content = t.content(0).expect("live");
    assert!(approx(content[Y], 60.0), "content was {}", content[Y]);
    assert!(approx(content[Y] - t.rect(0).expect("live").height(), 10.0));
}

#[test]
fn floating_leaves_the_flow_alone() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(200.0))
            .h(SizeSpec::px(100.0)),
    )
    .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf().w(SizeSpec::px(50.0)).h(SizeSpec::px(100.0)),
    )
    .expect("fits");
    t.leaf(
        2,
        BoxSpec::leaf()
            .w(SizeSpec::px(30.0))
            .h(SizeSpec::px(30.0))
            .floating([10.0, 20.0]),
    )
    .expect("fits");
    t.leaf(
        3,
        BoxSpec::leaf().w(SizeSpec::px(50.0)).h(SizeSpec::px(100.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([200.0, 100.0]).expect("closed");

    // The floating box neither advances the cursor nor is counted.
    assert_rect!(t, 1, 0.0, 0.0, 50.0, 100.0);
    assert_rect!(t, 3, 50.0, 0.0, 100.0, 100.0);
    assert_rect!(t, 2, 10.0, 20.0, 40.0, 50.0);
    assert!(
        approx(t.content(0).expect("live")[X], 100.0),
        "floating not counted"
    );
}

#[test]
fn floating_escapes_an_ancestors_clip() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::Y)
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(50.0))
            .clip(),
    )
    .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf()
            .w(SizeSpec::px(200.0))
            .h(SizeSpec::px(200.0))
            .floating([0.0, 0.0]),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([400.0, 400.0]).expect("closed");

    // A popup its parent could clip would be useless.
    let c = t.clip(1).expect("live");
    assert_eq!(
        c,
        Rect {
            min: [0.0, 0.0],
            max: [400.0, 400.0]
        }
    );
}

#[test]
fn an_absolute_box_is_placed_like_a_floating_one_but_stays_clipped() {
    table!(t, 8);
    let clipper = BoxSpec::container(Axis::Y)
        .w(SizeSpec::px(100.0))
        .h(SizeSpec::children_sum())
        .pad(10.0)
        .clip();
    t.open(0, clipper).expect("fits");
    t.leaf(1, BoxSpec::leaf().w(SizeSpec::px(40.0)).h(SizeSpec::px(30.0))).expect("fits");
    let over = BoxSpec::leaf().w(SizeSpec::px(200.0)).h(SizeSpec::px(200.0));
    t.leaf(2, over.absolute([5.0, 7.0])).expect("fits");
    t.leaf(3, BoxSpec::leaf().w(SizeSpec::px(40.0)).h(SizeSpec::px(30.0))).expect("fits");
    t.close().expect("balanced");
    t.solve([400.0, 400.0]).expect("closed");

    let parent = t.rect(0).expect("live");
    assert_eq!(parent.height(), 80.0, "out of the parent's sum");
    assert_eq!(t.rect(3).expect("live").min[1], 40.0, "the flow carries on past it");
    let placed = t.rect(2).expect("live");
    assert_eq!(placed.min, [15.0, 17.0], "at the offset from the content origin");
    assert_eq!(t.clip(2).expect("live"), parent, "clipped by its ancestor");
}

#[test]
fn nested_clips_intersect() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::Y)
            .w(SizeSpec::px(200.0))
            .h(SizeSpec::px(60.0))
            .clip(),
    )
    .expect("fits");
    t.open(
        1,
        BoxSpec::container(Axis::Y)
            .w(SizeSpec::px(80.0))
            .h(SizeSpec::px(200.0))
            .clip(),
    )
    .expect("fits");
    t.leaf(
        2,
        BoxSpec::leaf().w(SizeSpec::px(80.0)).h(SizeSpec::px(200.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.close().expect("balanced");
    t.solve([400.0, 400.0]).expect("closed");

    // Narrow from the inner box, short from the outer one.
    assert_eq!(
        t.clip(2).expect("live"),
        Rect {
            min: [0.0, 0.0],
            max: [80.0, 60.0]
        }
    );
}

#[test]
fn disjoint_clips_produce_an_empty_region_not_an_inverted_one() {
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::Y)
            .w(SizeSpec::px(50.0))
            .h(SizeSpec::px(50.0))
            .clip(),
    )
    .expect("fits");
    // Pushed entirely below the parent by scrolling, then clipped.
    t.open(
        1,
        BoxSpec::container(Axis::Y)
            .w(SizeSpec::px(50.0))
            .h(SizeSpec::px(50.0))
            .clip()
            .scroll([0.0, -200.0]),
    )
    .expect("fits");
    t.leaf(
        2,
        BoxSpec::leaf().w(SizeSpec::px(50.0)).h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.close().expect("balanced");
    t.solve([400.0, 400.0]).expect("closed");

    let c = t.clip(2).expect("live");
    assert!(
        c.width() >= 0.0 && c.height() >= 0.0,
        "clip inverted: {c:?}"
    );
}

// Property tests. Deterministic PRNG so a failure reproduces from its seed.

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

/// Emit a random tree of soft boxes. Soft so violations are always fully solved,
/// which is the precondition for the containment property below.
fn build_random(t: &mut BoxTable<'_>, rng: &mut Rng, depth: u32, budget: &mut u32) {
    let n = 1 + rng.below(4);
    for i in 0..n {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let key = (depth as u64) << 32 | i;
        let axis = if rng.below(2) == 0 { Axis::X } else { Axis::Y };
        let w = match rng.below(3) {
            0 => SizeSpec::new(Size::Px(rng.below(60) as f32), 0.0),
            1 => SizeSpec::grow(),
            _ => SizeSpec::children_sum(),
        };
        let h = match rng.below(3) {
            0 => SizeSpec::new(Size::Px(rng.below(60) as f32), 0.0),
            1 => SizeSpec::grow(),
            _ => SizeSpec::children_sum(),
        };
        if depth >= 4 || rng.below(3) == 0 {
            if t.leaf(key, BoxSpec::leaf().w(w).h(h)).is_err() {
                return;
            }
        } else {
            let mut spec = BoxSpec::container(axis);
            spec = spec.w(w).h(h);
            if t.open(key, spec).is_err() {
                return;
            }
            build_random(t, rng, depth + 1, budget);
            if t.close().is_err() {
                return;
            }
        }
    }
}

#[test]
fn no_child_escapes_its_parent_when_every_box_gives() {
    let mut total_boxes = 0usize;
    for seed in 1..200u64 {
        table!(t, 128);
        let mut rng = Rng(seed);
        t.open(
            0,
            BoxSpec::container(Axis::X)
                .w(SizeSpec::px(500.0))
                .h(SizeSpec::px(500.0)),
        )
        .expect("fits");
        let mut budget = 100u32;
        build_random(&mut t, &mut rng, 1, &mut budget);
        t.close().expect("balanced");
        t.solve([500.0, 500.0]).expect("closed");
        total_boxes += t.len();

        for i in 1..t.len() as u32 {
            let p = t.s.parent[i as usize];
            let child = t.rect(i).expect("live");
            let parent = t.rect(p).expect("live");
            for a in [X, Y] {
                assert!(
                    child.min[a] >= parent.min[a] - 0.01 && child.max[a] <= parent.max[a] + 0.01,
                    "seed {seed}: box {i} {child:?} escaped parent {p} {parent:?} on axis {a}"
                );
            }
        }
    }
    // Guard against the generator degenerating and the property passing vacuously.
    assert!(
        total_boxes > 1000,
        "generator produced only {total_boxes} boxes over 199 seeds"
    );
}

#[test]
fn siblings_never_overlap_along_the_layout_axis() {
    for seed in 1..200u64 {
        table!(t, 128);
        let mut rng = Rng(seed);
        t.open(
            0,
            BoxSpec::container(Axis::X)
                .w(SizeSpec::px(500.0))
                .h(SizeSpec::px(500.0)),
        )
        .expect("fits");
        let mut budget = 100u32;
        build_random(&mut t, &mut rng, 1, &mut budget);
        t.close().expect("balanced");
        t.solve([500.0, 500.0]).expect("closed");

        for i in 0..t.len() as u32 {
            let a = t.s.axis[i as usize].index();
            let end = t.s.subtree_end[i as usize];
            let mut c = i + 1;
            let mut prev_max = f32::NEG_INFINITY;
            while c < end {
                let r = t.rect(c).expect("live");
                assert!(
                    r.min[a] >= prev_max - 0.01,
                    "seed {seed}: sibling {c} starts at {} before previous ended at {prev_max}",
                    r.min[a]
                );
                prev_max = r.max[a];
                c = t.s.subtree_end[c as usize];
            }
        }
    }
}

#[test]
fn solving_is_deterministic() {
    for seed in 1..50u64 {
        let mut first = std::vec::Vec::new();
        {
            table!(t, 128);
            let mut rng = Rng(seed);
            t.open(
                0,
                BoxSpec::container(Axis::X)
                    .w(SizeSpec::px(500.0))
                    .h(SizeSpec::px(500.0)),
            )
            .expect("fits");
            let mut budget = 100u32;
            build_random(&mut t, &mut rng, 1, &mut budget);
            t.close().expect("balanced");
            t.solve([500.0, 500.0]).expect("closed");
            first.extend(t.rects());
        }
        table!(t, 128);
        let mut rng = Rng(seed);
        t.open(
            0,
            BoxSpec::container(Axis::X)
                .w(SizeSpec::px(500.0))
                .h(SizeSpec::px(500.0)),
        )
        .expect("fits");
        let mut budget = 100u32;
        build_random(&mut t, &mut rng, 1, &mut budget);
        t.close().expect("balanced");
        t.solve([500.0, 500.0]).expect("closed");
        let second: std::vec::Vec<(u64, Rect)> = t.rects().collect();

        assert_eq!(
            first, second,
            "seed {seed}: same input gave different rects"
        );
    }
}

#[test]
fn solving_twice_is_idempotent() {
    table!(t, 32);
    t.open(
        0,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(300.0))
            .h(SizeSpec::px(100.0)),
    )
    .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf()
            .w(SizeSpec::px(100.0).soft())
            .h(SizeSpec::grow()),
    )
    .expect("fits");
    t.leaf(2, BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::grow()))
        .expect("fits");
    t.close().expect("balanced");

    t.solve([300.0, 100.0]).expect("closed");
    let once: std::vec::Vec<(u64, Rect)> = t.rects().collect();
    t.solve([300.0, 100.0]).expect("closed");
    let twice: std::vec::Vec<(u64, Rect)> = t.rects().collect();

    // Passes must not accumulate into the columns they read. If they did, a second
    // solve on an unchanged table would drift.
    assert_eq!(once, twice);
}

#[test]
fn a_fraction_inside_a_growing_parent_resolves() {
    // The scrollbar thumb case. Pass 2 cannot resolve this, because a `Grow`
    // parent does not know its own size until pass 4, so the child used to
    // collapse to nothing and the thumb silently vanished.
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::Y)
            .w(SizeSpec::px(300.0))
            .h(SizeSpec::px(400.0)),
    )
    .expect("fits");
    t.open(
        1,
        BoxSpec::container(Axis::Y)
            .w(SizeSpec::px(12.0))
            .h(SizeSpec::grow()),
    )
    .expect("fits");
    t.leaf(
        2,
        BoxSpec::leaf()
            .w(SizeSpec::px(12.0))
            .h(SizeSpec::fraction(0.25)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.close().expect("balanced");
    t.solve([300.0, 400.0]).expect("closed");

    assert!(
        approx(t.rect(1).expect("live").height(), 400.0),
        "track grew"
    );
    assert!(
        approx(t.rect(2).expect("live").height(), 100.0),
        "thumb was {} of a 400px track, expected 100",
        t.rect(2).expect("live").height()
    );
}

#[test]
fn a_fraction_of_a_grow_parent_uses_the_content_box() {
    // Padding is not available to children, so a fraction is of what is left.
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::Y)
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(200.0)),
    )
    .expect("fits");
    t.open(
        1,
        BoxSpec::container(Axis::Y)
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::grow())
            .pad_xy(0.0, 20.0),
    )
    .expect("fits");
    t.leaf(
        2,
        BoxSpec::leaf()
            .w(SizeSpec::px(10.0))
            .h(SizeSpec::fraction(0.5)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.close().expect("balanced");
    t.solve([100.0, 200.0]).expect("closed");

    // 200 tall, 20 padding each end, so 160 of content and half is 80.
    assert!(
        approx(t.rect(2).expect("live").height(), 80.0),
        "got {}",
        t.rect(2).expect("live").height()
    );
}

#[test]
fn a_fraction_sibling_of_a_grow_child_still_shares_correctly() {
    // The fraction takes real space, so what Grow gets is what is left after it.
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::X)
            .w(SizeSpec::px(400.0))
            .h(SizeSpec::px(50.0)),
    )
    .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf()
            .w(SizeSpec::fraction(0.25))
            .h(SizeSpec::grow()),
    )
    .expect("fits");
    t.leaf(2, BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::grow()))
        .expect("fits");
    t.close().expect("balanced");
    t.solve([400.0, 50.0]).expect("closed");

    assert!(approx(t.rect(1).expect("live").width(), 100.0));
    assert!(
        approx(t.rect(2).expect("live").width(), 300.0),
        "grow got {} of the 300 left over",
        t.rect(2).expect("live").width()
    );
}

#[test]
fn a_fraction_uses_the_content_box_whichever_pass_resolves_it() {
    // Two parents of equal extent and padding, sized differently so each
    // fraction child is resolved by a different code path: `Px` is known by
    // pass 2 and refined in pass 4, `Grow` does not know its own size until
    // pass 4. Both must agree, and both must use the content box.
    let mut got = [0.0f32; 2];
    for (n, parent_w) in [SizeSpec::px(200.0), SizeSpec::grow()]
        .into_iter()
        .enumerate()
    {
        table!(t, 8);
        t.open(
            0,
            BoxSpec::container(Axis::X)
                .w(SizeSpec::px(200.0))
                .h(SizeSpec::px(80.0)),
        )
        .expect("fits");
        t.open(
            1,
            BoxSpec::container(Axis::X)
                .w(parent_w)
                .h(SizeSpec::px(80.0))
                .pad_xy(20.0, 0.0),
        )
        .expect("fits");
        // Small enough that the fraction is not squeezed by the overflow pass.
        t.leaf(
            2,
            BoxSpec::leaf().w(SizeSpec::px(40.0)).h(SizeSpec::px(10.0)),
        )
        .expect("fits");
        t.leaf(
            3,
            BoxSpec::leaf()
                .w(SizeSpec::fraction(0.5))
                .h(SizeSpec::px(10.0)),
        )
        .expect("fits");
        t.close().expect("balanced");
        t.close().expect("balanced");
        t.solve([200.0, 80.0]).expect("closed");
        got[n] = t.rect(3).expect("live").width();
    }

    // 200 wide, 20 padding each side, so 160 of content and half is 80.
    for (n, w) in got.iter().enumerate() {
        assert!(
            approx(*w, 80.0),
            "parent kind {n} gave {w}, expected 80 from a 160px content box"
        );
    }
}

#[test]
fn a_fraction_of_a_padded_sum_parent_also_uses_the_content_box() {
    // The `ChildrenSum` path resolves in pass 3b rather than pass 4, and used
    // to divide the parent's full extent. It cannot be compared directly
    // against the case above: a fraction child is excluded from the sum, so a
    // sum-sized parent plus a fraction child always overflows by construction.
    table!(t, 8);
    t.open(
        0,
        BoxSpec::container(Axis::X)
            .pad_xy(20.0, 0.0)
            .h(SizeSpec::px(40.0)),
    )
    .expect("fits");
    t.leaf(
        1,
        BoxSpec::leaf().w(SizeSpec::px(100.0)).h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.leaf(
        2,
        BoxSpec::leaf()
            .w(SizeSpec::fraction(0.5))
            .h(SizeSpec::px(10.0)),
    )
    .expect("fits");
    t.close().expect("balanced");
    t.solve([1000.0, 40.0]).expect("closed");

    // Parent is the 100px child plus 40 of padding, so 100 of content box.
    assert!(approx(t.rect(0).expect("live").width(), 140.0));
    assert!(
        approx(t.rect(2).expect("live").width(), 50.0),
        "got {}, expected half of the 100px content box",
        t.rect(2).expect("live").width()
    );
}
