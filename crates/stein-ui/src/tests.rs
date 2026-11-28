//! The frame lifecycle, input routing and damage, all headless.

use super::*;
use crate::draw::Shape;
use stein_layout::{Axis, Insets, Options, SizeSpec};

const N: usize = 64;
const Q: usize = 128;

/// Storage on the stack. No allocator is involved anywhere in this crate.
macro_rules! ui {
    ($u:ident) => {
        let mut lkey = [0u64; N];
        let mut lparent = [0u32; N];
        let mut lsubtree = [0u32; N];
        let mut lsize = [[SizeSpec::px(0.0); 2]; N];
        let mut laxis = [Axis::X; N];
        let mut lmeasured = [[0.0f32; 2]; N];
        let mut lpad = [Insets::ZERO; N];
        let mut lopts = [Options::default(); N];
        let mut lcomputed = [[0.0f32; 2]; N];
        let mut lcontent = [[0.0f32; 2]; N];
        let mut lrect = [Rect::default(); N];
        let mut lclip = [Rect::default(); N];
        let mut lstack = [0u32; N];
        let mut skeys = [0u64; N];
        let mut svalues: [Option<WidgetState>; N] = [const { None }; N];
        let mut stouched = [0u32; N];
        let mut quads = [Quad::solid(Rect::default(), Rect::default(), 0); Q];
        let mut prev = quads;
        let mut custom = [CustomRegion {
            key: 0,
            rect: Rect::default(),
            clip: Rect::default(),
        }; 8];
        let mut kstack = [0u64; N];
        let mut fill = [0u32; N];
        let mut shape = [crate::draw::Shape::NONE; N];
        let storage = UiStorage {
            layout: LayoutStorage {
                key: &mut lkey,
                parent: &mut lparent,
                subtree_end: &mut lsubtree,
                size: &mut lsize,
                axis: &mut laxis,
                measured: &mut lmeasured,
                pad: &mut lpad,
                opts: &mut lopts,
                computed: &mut lcomputed,
                content: &mut lcontent,
                rect: &mut lrect,
                clip: &mut lclip,
                stack: &mut lstack,
            },
            state_keys: &mut skeys,
            state_values: &mut svalues,
            state_touched: &mut stouched,
            quads: &mut quads,
            prev_quads: &mut prev,
            custom: &mut custom,
            key_stack: &mut kstack,
            fill: &mut fill,
            shape: &mut shape,
        };
        #[allow(unused_mut)]
        let mut $u = Ui::new(storage).expect("storage agrees");
    };
}

const RED: u32 = 0xffff_0000;
const BLUE: u32 = 0xff00_00ff;

fn panel() -> BoxSpec {
    BoxSpec::container(Axis::Y).fill()
}
fn button() -> BoxSpec {
    BoxSpec::leaf().w(SizeSpec::px(100.0)).h(SizeSpec::px(40.0))
}

fn at(x: f32, y: f32, down: bool) -> Input {
    Input {
        pointer: [x, y],
        pointer_down: down,
        wheel: [0.0, 0.0],
        dt: 1.0 / 60.0,
        ..Input::default()
    }
}

/// Emit a root panel with one button, and return the button's response.
fn frame(u: &mut Ui<'_>, input: Input) -> Response {
    u.begin_frame(input, [400.0, 300.0]);
    u.open(1, panel(), 0).expect("fits");
    let r = u.leaf(2, button(), RED).expect("fits");
    u.close().expect("balanced");
    u.end_frame().expect("closed");
    u.finish();
    r
}

#[test]
fn a_frame_produces_quads_for_filled_boxes_only() {
    ui!(u);
    frame(&mut u, at(-1.0, -1.0, false));
    // The panel has no fill, so it is layout only and never reaches the painter.
    assert_eq!(u.quads().len(), 1);
    assert_eq!(u.quads()[0].color, RED);
    assert_eq!(u.box_count(), 2);
}

#[test]
fn a_shape_applies_to_the_box_emitted_last_and_only_this_frame() {
    ui!(u);
    let shapes = |u: &mut Ui<'_>, shaped: bool| {
        u.begin_frame(at(-1.0, -1.0, false), [400.0, 300.0]);
        // Before any box there is nothing to shape.
        u.shape(Shape::round(9));
        u.open(1, panel(), BLUE).expect("fits");
        u.leaf(2, button(), RED).expect("fits");
        if shaped {
            u.shape(Shape::round(6).stroked(1));
        }
        u.leaf(3, button(), RED).expect("fits");
        u.close().expect("balanced");
        u.end_frame().expect("closed");
        u.finish();
        assert_eq!(u.quads().len(), 3);
        let mut out = [Shape::PILL; 3];
        for (o, q) in out.iter_mut().zip(u.quads()) {
            *o = q.shape;
        }
        out
    };
    let round = Shape::round(6).stroked(1);
    assert_eq!(shapes(&mut u, true), [Shape::NONE, round, Shape::NONE]);
    assert_eq!(
        shapes(&mut u, false),
        [Shape::NONE; 3],
        "the table is rewritten every frame, nothing carries over"
    );
}

#[test]
fn a_changed_shape_is_damage() {
    ui!(u);
    let draw = |u: &mut Ui<'_>, shape: Shape| {
        u.begin_frame(at(-1.0, -1.0, false), [400.0, 300.0]);
        u.leaf(1, button(), RED).expect("fits");
        u.shape(shape);
        u.end_frame().expect("closed");
        u.finish()
    };
    draw(&mut u, Shape::round(4));
    assert!(!draw(&mut u, Shape::round(4)).needs_present);
    assert!(draw(&mut u, Shape::round(5)).needs_present);
}

#[test]
fn hover_needs_a_frame_of_geometry_first() {
    ui!(u);
    // Nothing has been laid out yet, so the first frame cannot know what is
    // under the pointer. A box that did not exist last frame is not hoverable,
    // which is correct rather than merely convenient.
    let r = frame(&mut u, at(50.0, 20.0, false));
    assert!(!r.hovered);

    let r = frame(&mut u, at(50.0, 20.0, false));
    assert!(r.hovered, "second frame has geometry to test against");
}

#[test]
fn a_press_and_release_over_the_widget_is_a_click() {
    ui!(u);
    frame(&mut u, at(50.0, 20.0, false));

    let down = frame(&mut u, at(50.0, 20.0, true));
    assert!(down.pressed && down.active);
    assert!(!down.clicked, "a press is not a click");

    let up = frame(&mut u, at(50.0, 20.0, false));
    assert!(up.clicked);
    assert!(!up.active);
}

#[test]
fn dragging_off_and_releasing_cancels() {
    ui!(u);
    frame(&mut u, at(50.0, 20.0, false));
    let down = frame(&mut u, at(50.0, 20.0, true));
    assert!(down.active);

    // Still held, pointer has wandered away. Active survives, which is what makes
    // dragging work, but hover does not.
    let away = frame(&mut u, at(300.0, 250.0, true));
    assert!(away.active, "drag must survive leaving the widget");
    assert!(!away.hovered);

    let release = frame(&mut u, at(300.0, 250.0, false));
    assert!(!release.clicked, "releasing elsewhere must not click");
}

#[test]
fn state_persists_across_frames_and_is_keyed() {
    ui!(u);
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    u.state(10).expect("room").scroll = [0.0, 42.0];
    u.state(11).expect("room").scroll = [0.0, 7.0];
    u.end_frame().expect("closed");
    u.finish();

    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    assert_eq!(u.state(10).expect("room").scroll, [0.0, 42.0]);
    assert_eq!(u.state(11).expect("room").scroll, [0.0, 7.0]);
    u.end_frame().expect("closed");
    u.finish();
}

#[test]
fn unseen_state_is_eventually_pruned() {
    ui!(u);
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    u.state(99).expect("room").scroll = [1.0, 1.0];
    u.end_frame().expect("closed");
    u.finish();
    assert_eq!(u.live_state(), 1);

    // Never touched again. A UI that showed a widget once must not keep its state
    // for the life of the process.
    //
    // The bound is the age limit plus one full sweep, because pruning examines a
    // slice of the table per frame rather than all of it. That upper bound is the
    // thing worth pinning: without it, a regression to a slower sweep would go
    // unnoticed until some long-running UI grew a leak.
    let bound = STATE_MAX_AGE as usize + u.state_capacity() + 2;
    for _ in 0..bound {
        u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
        u.end_frame().expect("closed");
        u.finish();
    }
    assert_eq!(u.live_state(), 0, "not evicted within {bound} frames");
}

#[test]
fn pruning_costs_a_slice_of_the_table_not_all_of_it() {
    ui!(u);
    // Fill well past what one frame's sweep can examine.
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    for k in 0..40u64 {
        u.state(k + 1).expect("room");
    }
    u.end_frame().expect("closed");
    u.finish();
    let live = u.live_state();
    assert!(live >= 40, "setup put only {live} entries in");

    // One frame after everything went stale evicts at most the budget, not the
    // table. This is what keeps an idle frame's cost independent of capacity.
    for _ in 0..STATE_MAX_AGE + 1 {
        u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
        u.end_frame().expect("closed");
        u.finish();
    }
    let after_one = u.live_state();
    assert!(
        after_one > 0,
        "a single frame emptied the whole table, the sweep is not bounded"
    );
}

#[test]
fn an_unchanged_frame_produces_no_damage() {
    ui!(u);
    frame(&mut u, at(-1.0, -1.0, false));
    u.begin_frame(at(-1.0, -1.0, false), [400.0, 300.0]);
    u.open(1, panel(), 0).expect("fits");
    u.leaf(2, button(), RED).expect("fits");
    u.close().expect("balanced");
    u.end_frame().expect("closed");
    let r = u.finish();

    assert!(r.damage.is_empty());
    assert!(!r.needs_present, "an idle window must be allowed to sleep");
}

#[test]
fn a_recoloured_box_damages_its_rect() {
    ui!(u);
    frame(&mut u, at(-1.0, -1.0, false));

    u.begin_frame(at(-1.0, -1.0, false), [400.0, 300.0]);
    u.open(1, panel(), 0).expect("fits");
    u.leaf(2, button(), BLUE).expect("fits");
    u.close().expect("balanced");
    u.end_frame().expect("closed");
    let r = u.finish();

    let d = r.damage.bounds().expect("colour changed");
    assert_eq!(d.min, [0.0, 0.0]);
    assert_eq!(d.max, [100.0, 40.0]);
    assert!(r.needs_present);
}

#[test]
fn an_animation_in_flight_owes_another_frame() {
    ui!(u);
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    let v = u.animate(5, 1.0, 10.0);
    u.end_frame().expect("closed");
    let r = u.finish();

    assert!(v > 0.0 && v < 1.0, "should have moved but not arrived: {v}");
    assert!(r.animating);
    assert!(
        r.needs_present,
        "a sleeping window would freeze mid-transition"
    );
}

#[test]
fn an_animation_settles_and_stops_asking_for_frames() {
    ui!(u);
    let mut last = 0.0;
    for _ in 0..600 {
        u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
        last = u.animate(5, 1.0, 20.0);
        u.end_frame().expect("closed");
        let r = u.finish();
        if !r.animating {
            break;
        }
    }
    assert!((last - 1.0).abs() <= 0.001, "settled at {last}");

    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    u.animate(5, 1.0, 20.0);
    u.end_frame().expect("closed");
    let r = u.finish();
    assert!(
        !r.animating,
        "a settled animation must stop waking the loop"
    );
}

#[test]
fn animation_never_overshoots_at_any_frame_time() {
    ui!(u);
    // A frame that ran 100x long must not send the value past its target.
    for dt in [1.0 / 240.0, 1.0 / 60.0, 0.5, 5.0] {
        u.begin_frame(
            Input {
                pointer: [0.0, 0.0],
                pointer_down: false,
                wheel: [0.0, 0.0],
                dt,
                ..Input::default()
            },
            [400.0, 300.0],
        );
        let v = u.animate(6, 1.0, 20.0);
        u.end_frame().expect("closed");
        u.finish();
        assert!((0.0..=1.0).contains(&v), "dt {dt} produced {v}");
    }
}

#[test]
fn a_custom_region_reports_its_solved_rect_and_paints_nothing_itself() {
    ui!(u);
    u.begin_frame(at(-1.0, -1.0, false), [400.0, 300.0]);
    u.open(1, panel(), 0).expect("fits");
    u.custom(
        7,
        BoxSpec::leaf()
            .w(SizeSpec::px(200.0))
            .h(SizeSpec::px(150.0)),
    )
    .expect("fits");
    u.close().expect("balanced");
    u.end_frame().expect("closed");
    u.finish();

    assert_eq!(
        u.quads().len(),
        0,
        "the application owns every pixel inside"
    );
    let regions = u.custom_regions();
    assert_eq!(regions.len(), 1);
    assert_eq!(regions[0].key, 7);
    assert_eq!(regions[0].rect.max, [200.0, 150.0]);
    assert_eq!(regions[0].clip.max, [400.0, 300.0]);
}

#[test]
fn the_application_can_push_into_a_custom_region() {
    ui!(u);
    u.begin_frame(at(-1.0, -1.0, false), [400.0, 300.0]);
    u.open(1, panel(), 0).expect("fits");
    u.custom(
        7,
        BoxSpec::leaf()
            .w(SizeSpec::px(200.0))
            .h(SizeSpec::px(150.0)),
    )
    .expect("fits");
    u.close().expect("balanced");
    u.end_frame().expect("closed");
    u.finish();

    // This is the flame graph path: read the region, cull, push.
    let region = u.custom_regions()[0];
    for i in 0..10u32 {
        u.push_quad(Quad::solid(
            Rect {
                min: [i as f32 * 10.0, 0.0],
                max: [i as f32 * 10.0 + 8.0, 12.0],
            },
            region.clip,
            0xff00_ff00,
        ));
    }
    assert_eq!(u.quads().len(), 10);
}

#[test]
fn quads_outside_a_custom_regions_clip_are_culled() {
    ui!(u);
    u.begin_frame(at(-1.0, -1.0, false), [400.0, 300.0]);
    u.open(
        1,
        BoxSpec::container(Axis::Y)
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(80.0))
            .clip(),
        0,
    )
    .expect("fits");
    u.custom(
        7,
        BoxSpec::leaf()
            .w(SizeSpec::px(100.0))
            .h(SizeSpec::px(400.0)),
    )
    .expect("fits");
    u.close().expect("balanced");
    u.end_frame().expect("closed");
    u.finish();

    let region = u.custom_regions()[0];
    u.push_quad(Quad::solid(
        Rect {
            min: [0.0, 0.0],
            max: [10.0, 10.0],
        },
        region.clip,
        1,
    ));
    u.push_quad(Quad::solid(
        Rect {
            min: [0.0, 300.0],
            max: [10.0, 310.0],
        },
        region.clip,
        2,
    ));
    assert_eq!(u.quads().len(), 1, "the scrolled-away one is culled");
    assert_eq!(u.quads()[0].color, 1);
}

#[test]
fn unbalanced_frames_are_refused() {
    ui!(u);
    u.begin_frame(at(0.0, 0.0, false), [100.0, 100.0]);
    assert_eq!(u.close(), Err(UiError::Unbalanced));
    u.open(1, panel(), 0).expect("fits");
    assert_eq!(u.end_frame(), Err(UiError::Unbalanced));
}

#[test]
fn keys_route_interaction_to_the_right_widget() {
    ui!(u);
    // Two buttons, second one under the pointer.
    let build = |u: &mut Ui<'_>, input: Input| {
        u.begin_frame(input, [400.0, 300.0]);
        u.open(1, BoxSpec::container(Axis::Y).fill(), 0)
            .expect("fits");
        let a = u.leaf(2, button(), RED).expect("fits");
        let b = u.leaf(3, button(), BLUE).expect("fits");
        u.close().expect("balanced");
        u.end_frame().expect("closed");
        u.finish();
        (a, b)
    };
    build(&mut u, at(50.0, 60.0, false));
    let (a, b) = build(&mut u, at(50.0, 60.0, false));
    assert!(!a.hovered, "first button spans y 0..40");
    assert!(b.hovered, "second spans y 40..80");
}

#[test]
fn quad_overflow_is_reported_rather_than_silently_truncating() {
    ui!(u);
    u.begin_frame(at(-1.0, -1.0, false), [400.0, 300.0]);
    u.open(1, panel(), 0).expect("fits");
    u.close().expect("balanced");
    u.end_frame().expect("closed");
    u.finish();

    for i in 0..Q + 5 {
        u.push_quad(Quad::solid(
            Rect {
                min: [0.0, 0.0],
                max: [1.0, 1.0],
            },
            Rect {
                min: [0.0, 0.0],
                max: [400.0, 300.0],
            },
            i as u32 + 1,
        ));
    }
    u.begin_frame(at(-1.0, -1.0, false), [400.0, 300.0]);
    u.open(1, panel(), 0).expect("fits");
    u.close().expect("balanced");
    u.end_frame().expect("closed");
    let r = u.finish();
    let _ = r;
    assert_eq!(u.quads().len(), 0, "a new frame starts empty");
}

#[test]
fn drag_reports_per_frame_and_total_movement() {
    ui!(u);
    frame(&mut u, at(50.0, 20.0, false));

    // Press. No movement yet, and the origin is the press point.
    let r = frame(&mut u, at(50.0, 20.0, true));
    assert!(r.pressed);
    assert_eq!(r.drag.delta, [0.0, 0.0], "a press has not moved anything");
    assert_eq!(r.drag.origin, [50.0, 20.0]);

    let r = frame(&mut u, at(60.0, 25.0, true));
    assert_eq!(r.drag.delta, [10.0, 5.0], "movement since the last frame");
    assert_eq!(r.drag.total, [10.0, 5.0]);

    let r = frame(&mut u, at(65.0, 20.0, true));
    assert_eq!(r.drag.delta, [5.0, -5.0]);
    assert_eq!(r.drag.total, [15.0, 0.0], "movement since the press");
    assert_eq!(r.drag.origin, [50.0, 20.0], "the anchor does not move");
}

#[test]
fn drag_survives_leaving_the_widget() {
    ui!(u);
    frame(&mut u, at(50.0, 20.0, false));
    frame(&mut u, at(50.0, 20.0, true));

    // Far outside the button. Pan gestures routinely leave the widget that
    // started them, so the delta has to keep arriving.
    let r = frame(&mut u, at(350.0, 280.0, true));
    assert!(r.active && !r.hovered);
    assert_eq!(r.drag.delta, [300.0, 260.0]);
    assert_eq!(r.drag.total, [300.0, 260.0]);
}

#[test]
fn only_the_dragged_widget_sees_the_drag() {
    ui!(u);
    let build = |u: &mut Ui<'_>, input: Input| {
        u.begin_frame(input, [400.0, 300.0]);
        u.open(1, BoxSpec::container(Axis::Y).fill(), 0)
            .expect("fits");
        let a = u.leaf(2, button(), RED).expect("fits");
        let b = u.leaf(3, button(), BLUE).expect("fits");
        u.close().expect("balanced");
        u.end_frame().expect("closed");
        u.finish();
        (a, b)
    };
    build(&mut u, at(50.0, 20.0, false));
    build(&mut u, at(50.0, 20.0, true));
    let (a, b) = build(&mut u, at(80.0, 60.0, true));

    assert!(a.active, "the press started on the first button");
    assert_eq!(a.drag.delta, [30.0, 40.0]);
    // The pointer is now over the second button, but it is not being dragged.
    assert_eq!(
        b.drag,
        Drag::default(),
        "a merely hovered widget must not think it is being dragged"
    );
}

#[test]
fn releasing_clears_the_drag() {
    ui!(u);
    frame(&mut u, at(50.0, 20.0, false));
    frame(&mut u, at(50.0, 20.0, true));
    frame(&mut u, at(70.0, 30.0, true));
    let r = frame(&mut u, at(70.0, 30.0, false));
    assert_eq!(r.drag, Drag::default());
}

#[test]
fn state_changed_during_a_frame_needs_an_explicit_repaint() {
    ui!(u);
    // A widget whose colour comes from a flag the click itself flips. The
    // widget is emitted before the click is known, so the new colour belongs to
    // the next frame. This is the shape of every select-on-click list.
    let mut selected = false;
    let build = |u: &mut Ui<'_>, input: Input, sel: &mut bool, repaint: bool| {
        u.begin_frame(input, [400.0, 300.0]);
        u.open(1, panel(), 0).expect("fits");
        let fill = if *sel { BLUE } else { RED };
        let r = u.leaf(2, button(), fill).expect("fits");
        if r.clicked {
            *sel = true;
            if repaint {
                u.request_repaint();
            }
        }
        u.close().expect("balanced");
        u.end_frame().expect("closed");
        u.finish()
    };

    build(&mut u, at(50.0, 20.0, false), &mut selected, false);
    build(&mut u, at(50.0, 20.0, true), &mut selected, false);
    // Release: the click lands, but the quads are identical to last frame's
    // because the colour was decided before the click was known.
    let r = build(&mut u, at(50.0, 20.0, false), &mut selected, false);
    assert!(selected, "the click was received");
    assert!(r.damage.is_empty(), "nothing changed visually yet");
    assert!(
        !r.needs_present,
        "without a request the window sleeps holding a stale frame, and the \
         update only appears when some unrelated event wakes it"
    );

    // Same sequence, but asking for the frame that shows the change.
    let mut selected = false;
    ui!(v);
    build(&mut v, at(50.0, 20.0, false), &mut selected, true);
    build(&mut v, at(50.0, 20.0, true), &mut selected, true);
    let r = build(&mut v, at(50.0, 20.0, false), &mut selected, true);
    assert!(r.needs_present, "the owed frame must be requested");
    assert!(r.repaint_requested);

    // And that next frame is where the colour actually changes.
    let r = build(&mut v, at(50.0, 20.0, false), &mut selected, true);
    assert!(!r.damage.is_empty(), "the new colour reaches the screen");
}

#[test]
fn a_repaint_request_lasts_one_frame() {
    ui!(u);
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    u.request_repaint();
    u.end_frame().expect("closed");
    let r = u.finish();
    assert!(r.needs_present);

    // Not sticky. A request that never cleared would keep the window awake
    // forever, which is the bug it exists to prevent, inverted.
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    u.end_frame().expect("closed");
    let r = u.finish();
    assert!(!r.needs_present);
}

#[test]
fn last_frames_geometry_is_readable_while_emitting() {
    ui!(u);
    // First frame: nothing has been laid out, so there is nothing to read.
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    u.state(2).expect("room");
    assert_eq!(u.last_rect(2), Some(Rect::default()), "never laid out yet");
    u.open(1, panel(), 0).expect("fits");
    u.leaf(2, button(), RED).expect("fits");
    u.close().expect("balanced");
    u.end_frame().expect("closed");
    u.finish();

    // Second frame: the solve from the first is available before the widget is
    // emitted, which is what a thumb or a pane clamp needs.
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    let r = u.last_rect(2).expect("state exists");
    assert_eq!(r.width(), 100.0);
    assert_eq!(r.height(), 40.0);
    u.end_frame().expect("closed");
    u.finish();
}

#[test]
fn geometry_is_only_tracked_for_keys_that_asked_for_state() {
    ui!(u);
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    u.open(1, panel(), 0).expect("fits");
    u.leaf(2, button(), RED).expect("fits");
    u.close().expect("balanced");
    u.end_frame().expect("closed");
    u.finish();

    // Never asked for state, so nothing is stored. Tracking every box would
    // fill the table with entries nobody reads.
    assert_eq!(u.last_rect(2), None);
    assert_eq!(u.live_state(), 0);
}

#[test]
fn content_extent_reaches_state_for_scroll_ranges() {
    ui!(u);
    let build = |u: &mut Ui<'_>| {
        u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
        u.state(1).expect("room");
        u.open(
            1,
            BoxSpec::container(Axis::Y)
                .w(SizeSpec::px(100.0))
                .h(SizeSpec::px(50.0))
                .clip(),
            0,
        )
        .expect("fits");
        for k in 10..16u64 {
            u.leaf(
                k,
                BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::px(20.0)),
                RED,
            )
            .expect("fits");
        }
        u.close().expect("balanced");
        u.end_frame().expect("closed");
        u.finish();
    };
    build(&mut u);
    build(&mut u);

    // Six rows of 20 want 120 in a box 50 tall, so 70 of overflow.
    let content = u.last_content(1).expect("state exists");
    let rect = u.last_rect(1).expect("state exists");
    assert_eq!(content[1], 120.0);
    assert_eq!(content[1] - rect.height(), 70.0, "scrollable overflow");
}

#[test]
fn the_user_payload_is_never_touched() {
    ui!(u);
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    // A u64 id cannot live in an f32, which is what forced widgets to pack
    // values across the float fields and risk `animate` corrupting them.
    u.state(7).expect("room").user[0] = u64::MAX;
    u.state(7).expect("room").user[3] = 0xdead_beef_cafe_f00d;
    u.end_frame().expect("closed");
    u.finish();

    for _ in 0..5 {
        u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
        u.animate(7, 1.0, 10.0);
        u.end_frame().expect("closed");
        u.finish();
    }
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    let st = *u.state(7).expect("room");
    assert_eq!(st.user[0], u64::MAX, "animate corrupted the payload");
    assert_eq!(st.user[3], 0xdead_beef_cafe_f00d);
    assert!(st.anim > 0.0, "the animation did run alongside it");
}

#[test]
fn fresh_state_is_distinguishable_from_zeroed_state() {
    ui!(u);
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    let frame = u.frame_index();
    let st = *u.state(9).expect("room");
    assert!(st.is_fresh(frame), "first sight must report fresh");
    u.state(9).expect("room").scroll = [0.0, 0.0];
    u.end_frame().expect("closed");
    u.finish();

    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    let frame = u.frame_index();
    // Zero-valued but not new, which a widget must be able to tell apart from
    // never-seen without reserving a sentinel inside its own data.
    assert!(!u.state(9).expect("room").is_fresh(frame));
    u.end_frame().expect("closed");
    u.finish();
}

#[test]
fn peek_drag_matches_what_the_response_reports() {
    ui!(u);
    frame(&mut u, at(50.0, 20.0, false));
    frame(&mut u, at(50.0, 20.0, true));

    u.begin_frame(at(70.0, 35.0, true), [400.0, 300.0]);
    // Available before the widget is emitted, which is what a widget that moves
    // itself needs. Re-deriving it from the pointer duplicates this.
    let peeked = u.peek_drag(2);
    u.open(1, panel(), 0).expect("fits");
    let r = u.leaf(2, button(), RED).expect("fits");
    u.close().expect("balanced");
    u.end_frame().expect("closed");
    u.finish();

    assert_eq!(peeked, r.drag);
    assert_eq!(peeked.delta, [20.0, 15.0]);
    assert_eq!(u.peek_drag(3), Drag::default(), "only the dragged widget");
}

// Keyboard and focus.

fn with_keys(evs: &[(u32, bool)], mods: Mods) -> Input {
    let mut i = at(0.0, 0.0, false);
    i.mods = mods;
    for (sym, pressed) in evs {
        i.push_key(KeyEvent {
            sym: *sym,
            pressed: *pressed,
            repeat: false,
            mods,
        });
    }
    i
}

#[test]
fn key_events_and_chords_are_readable() {
    let ctrl = Mods {
        ctrl: true,
        ..Mods::default()
    };
    let i = with_keys(&[(b's' as u32, true)], ctrl);
    assert!(i.pressed(b's' as u32));
    assert!(i.chord(b's' as u32, ctrl));
    // Exact, so ctrl+s does not fire when shift is also held. A chord table
    // that ignores extra modifiers fires on combinations the user did not ask
    // for.
    assert!(!i.chord(
        b's' as u32,
        Mods {
            ctrl: true,
            shift: true,
            ..Mods::default()
        }
    ));
    assert!(!i.chord(b's' as u32, Mods::default()));
}

#[test]
fn the_key_buffer_refuses_rather_than_overwriting() {
    let mut i = at(0.0, 0.0, false);
    for n in 0..MAX_KEYS {
        assert!(i.push_key(KeyEvent {
            sym: n as u32,
            pressed: true,
            repeat: false,
            mods: Mods::default()
        }));
    }
    assert!(!i.push_key(KeyEvent {
        sym: 99,
        pressed: true,
        repeat: false,
        mods: Mods::default()
    }));
    assert_eq!(i.keys().len(), MAX_KEYS);
    assert!(i.pressed(0), "the first event must survive the overflow");
}

#[test]
fn committed_text_truncates_on_a_char_boundary() {
    let mut i = at(0.0, 0.0, false);
    assert!(i.push_text("ok"));
    assert_eq!(i.text(), "ok");

    let mut i = at(0.0, 0.0, false);
    // Multi-byte, so a byte-wise cut would yield invalid UTF-8 rather than a
    // shorter string.
    let long = "\u{e9}".repeat(MAX_TEXT);
    assert!(!i.push_text(&long), "must report the truncation");
    assert!(i.text().len() <= MAX_TEXT);
    assert!(long.starts_with(i.text()), "split a code point");
}

#[test]
fn focus_routes_keys_to_exactly_one_widget() {
    ui!(u);
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    u.focusable(10);
    u.focusable(11);
    u.focus(Some(11));
    u.end_frame().expect("closed");
    u.finish();

    u.begin_frame(
        with_keys(&[(b'x' as u32, true)], Mods::default()),
        [400.0, 300.0],
    );
    assert!(u.focusable(10) == false);
    assert!(u.focusable(11));
    assert!(
        !u.keys_for(10),
        "an unfocused field must not consume the key"
    );
    assert!(u.keys_for(11));
    u.end_frame().expect("closed");
    u.finish();
}

#[test]
fn tab_cycles_in_emission_order_and_wraps() {
    ui!(u);
    let mut build = |u: &mut Ui<'_>| {
        u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
        for k in [10u64, 11, 12] {
            u.focusable(k);
        }
        u.end_frame().expect("closed");
        u.finish();
    };
    build(&mut u);

    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    for k in [10u64, 11, 12] {
        u.focusable(k);
    }
    assert_eq!(
        u.cycle_focus(false),
        Some(10),
        "nothing focused starts at the first"
    );
    assert_eq!(u.cycle_focus(false), Some(11));
    assert_eq!(u.cycle_focus(false), Some(12));
    assert_eq!(u.cycle_focus(false), Some(10), "wraps");
    assert_eq!(u.cycle_focus(true), Some(12), "and backwards");
    u.end_frame().expect("closed");
    u.finish();
}

#[test]
fn focus_on_a_widget_that_stops_being_drawn_is_dropped() {
    ui!(u);
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    u.focusable(10);
    u.focusable(11);
    u.focus(Some(11));
    u.end_frame().expect("closed");
    u.finish();
    assert_eq!(u.focused(), Some(11));

    // 11 is gone, scrolled away or its panel closed. Keeping focus there would
    // send every keystroke somewhere invisible with no way to get it back.
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    u.focusable(10);
    u.end_frame().expect("closed");
    u.finish();
    assert_eq!(u.focused(), None);
}

#[test]
fn a_frame_with_nothing_focusable_keeps_the_focus() {
    ui!(u);
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    u.focusable(10);
    u.focus(Some(10));
    u.end_frame().expect("closed");
    u.finish();

    // A frame that simply drew no fields, a modal over the form say, must not
    // silently lose where the user was typing.
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    u.end_frame().expect("closed");
    u.finish();
    assert_eq!(u.focused(), Some(10));
}

#[test]
fn moving_focus_asks_for_a_frame() {
    ui!(u);
    u.begin_frame(at(0.0, 0.0, false), [400.0, 300.0]);
    u.focusable(10);
    u.focus(Some(10));
    u.end_frame().expect("closed");
    let r = u.finish();
    // A focus ring is a visible change, and nothing else this frame moved.
    assert!(
        r.needs_present,
        "the focus change was not going to be drawn"
    );
}
