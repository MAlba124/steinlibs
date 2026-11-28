//! Headless widget tests over stack declared storage.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stein_layout::{Align, Axis, BoxSpec, Insets, LayoutStorage, Options, Rect, SizeSpec};
use stein_ui::draw::{CustomRegion, Quad};
use stein_ui::key::{child, hash_str, indexed};
use stein_ui::{Input, KeyEvent, Mods, Response, Ui, UiStorage, WidgetState};

use crate::WidgetError;
use crate::scroll::{ScrollArea, ScrollStyle};
use crate::split::{Splitter, SplitterStyle};
use crate::table::{
    Column, ColumnWidth, MAX_COLUMNS, Sort, Table, TableStyle, chars_that_fit, clamp_sort,
    divider_offset,
};
use crate::tabs::{Tabs, TabsStyle};
use crate::textview::{Caret, LineRange, SelectionRects, TextMetrics, TextView, TextViewStyle};
use crate::vlist::{VirtualList, VirtualRange};

const N: usize = 256;
const S: usize = 64;
const Q: usize = 512;
const DT: f32 = 1.0 / 60.0;

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
        let mut lstack = [0u32; 32];
        let mut skeys = [0u64; S];
        let mut svalues: [Option<WidgetState>; S] = [const { None }; S];
        let mut stouched = [0u32; S];
        let mut quads = [Quad::solid(Rect::default(), Rect::default(), 0); Q];
        let mut prev = quads;
        let mut custom = [CustomRegion {
            key: 0,
            rect: Rect::default(),
            clip: Rect::default(),
        }; 4];
        let mut kstack = [0u64; 32];
        let mut fill = [0u32; N];
        let mut shape = [stein_ui::draw::Shape::NONE; N];
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

fn at(x: f32, y: f32, down: bool) -> Input {
    Input {
        pointer: [x, y],
        pointer_down: down,
        wheel: [0.0, 0.0],
        dt: DT,
        ..Input::default()
    }
}

/// No easing, so an offset lands in the frame that moved it.
fn instant() -> ScrollStyle {
    ScrollStyle {
        smooth: 0.0,
        ..ScrollStyle::default()
    }
}

fn wheeled(x: f32, y: f32, w: f32) -> Input {
    Input {
        pointer: [x, y],
        pointer_down: false,
        // `w` in notches.
        wheel: [0.0, w * stein_ui::WHEEL_NOTCH],
        dt: DT,
        ..Input::default()
    }
}

// A scroll area over `rows` rows of `row_h` px each, virtualized.

const SKEY: u64 = 0x51;
const LKEY: u64 = 0x52;

struct ScrollOut {
    range: VirtualRange,
    boxes: usize,
    offset: f32,
    max_scroll: f32,
    view_len: f32,
    content_len: f32,
    thumb: Option<Rect>,
    track: Option<Rect>,
}

fn scroll_frame_sized(
    u: &mut Ui,
    input: Input,
    rows: u32,
    row_h: f32,
    size: [SizeSpec; 2],
    vp: [f32; 2],
) -> ScrollOut {
    u.begin_frame(input, vp);
    let sa = ScrollArea::begin(u, SKEY, Axis::Y, size, instant()).expect("begin");
    let vl = VirtualList::begin(u, LKEY, sa.offset(), sa.view_len(), rows, row_h).expect("vlist");
    let range = vl.range();
    for i in range.first..range.end {
        vl.row(u, 1_000 + u64::from(i), 0xff33_3344).expect("row");
    }
    let range = vl.end(u).expect("vlist end");
    let sa = sa.end(u).expect("scroll end");
    u.end_frame().expect("closed");
    let boxes = u.box_count();
    let thumb = u.geometry_of(sa.thumb_index()).map(|g| g.0);
    let track = u.geometry_of(sa.track_index()).map(|g| g.0);
    u.finish();
    ScrollOut {
        range,
        boxes,
        offset: sa.offset(),
        max_scroll: sa.max_scroll(),
        view_len: sa.view_len(),
        content_len: sa.content_len(),
        thumb,
        track,
    }
}

fn scroll_frame(u: &mut Ui, input: Input, rows: u32, row_h: f32) -> ScrollOut {
    scroll_frame_sized(
        u,
        input,
        rows,
        row_h,
        [SizeSpec::px(400.0), SizeSpec::px(300.0)],
        [400.0, 300.0],
    )
}

#[test]
fn a_100k_row_list_emits_a_couple_dozen_boxes_and_the_range_is_right() {
    ui!(u);
    let rows = 100_000;
    let idle = at(-1.0, -1.0, false);
    scroll_frame(&mut u, idle, rows, 20.0);

    let top = scroll_frame(&mut u, idle, rows, 20.0);
    assert!(
        top.content_len > top.view_len,
        "no overflow, test is vacuous"
    );
    assert_eq!(top.view_len, 300.0);
    assert_eq!(
        top.content_len, 2_000_000.0,
        "spacers must keep content exact"
    );
    assert_eq!(top.range, VirtualRange { first: 0, end: 15 });
    // Outer, viewport, two spacers, 15 rows, track, lead spacer, thumb.
    assert_eq!(top.boxes, 22, "100k rows must cost only the visible boxes");

    u.state(SKEY).expect("room").scroll[1] = 1_000_000.0;
    let mid = scroll_frame(&mut u, idle, rows, 20.0);
    assert_eq!(
        mid.range,
        VirtualRange {
            first: 50_000,
            end: 50_015
        }
    );
    assert!(mid.boxes <= 30);

    u.state(SKEY).expect("room").scroll[1] = 9.0e9;
    let bot = scroll_frame(&mut u, idle, rows, 20.0);
    assert_eq!(bot.offset, bot.max_scroll, "clamped to the real overflow");
    assert_eq!(
        bot.range,
        VirtualRange {
            first: 99_985,
            end: 100_000
        }
    );
    assert!(bot.boxes <= 30);
}

#[test]
fn scroll_clamps_at_both_ends_and_cannot_pass_the_content() {
    ui!(u);
    let rows = 100;
    let idle = at(200.0, 150.0, false);
    scroll_frame(&mut u, idle, rows, 20.0);
    scroll_frame(&mut u, idle, rows, 20.0);

    let r = scroll_frame(&mut u, wheeled(200.0, 150.0, 50.0), rows, 20.0);
    assert!(r.max_scroll > 0.0, "no overflow, test is vacuous");
    assert_eq!(r.offset, 0.0, "scrolled above the top");

    let mut last = 0.0;
    for _ in 0..5 {
        let r = scroll_frame(&mut u, wheeled(200.0, 150.0, -50.0), rows, 20.0);
        assert!(r.offset <= r.max_scroll, "driven past the content");
        last = r.offset;
    }
    assert_eq!(last, 1_700.0, "pinned exactly at content minus viewport");
    let r = scroll_frame(&mut u, wheeled(200.0, 150.0, -50.0), rows, 20.0);
    assert_eq!(r.offset, 1_700.0);
}

#[test]
fn the_thumb_is_proportional_to_the_visible_fraction() {
    ui!(u);
    let rows = 60;
    let idle = at(-1.0, -1.0, false);
    scroll_frame(&mut u, idle, rows, 20.0);
    scroll_frame(&mut u, idle, rows, 20.0);
    let r = scroll_frame(&mut u, idle, rows, 20.0);

    let thumb = r.thumb.expect("thumb solved");
    let track = r.track.expect("track solved");
    let frac = r.view_len / r.content_len;
    assert!(frac > 0.0 && frac < 1.0, "fraction {frac} is vacuous");
    let want = track.height() * frac;
    assert!(
        (thumb.height() - want).abs() < 0.5,
        "thumb {} for want {}",
        thumb.height(),
        want
    );
}

#[test]
fn dragging_the_thumb_moves_the_content_scaled_and_in_the_same_direction() {
    ui!(u);
    // Content 1200 over a 300 viewport. Thumb 75 in a 300 track, so 225px of
    // travel maps to 900px of scroll, a factor of exactly 4.
    let rows = 60;
    let f = |u: &mut Ui, x: f32, y: f32, down: bool| scroll_frame(u, at(x, y, down), rows, 20.0);
    f(&mut u, -1.0, -1.0, false);
    f(&mut u, -1.0, -1.0, false);
    f(&mut u, 395.0, 40.0, false);

    let r = f(&mut u, 395.0, 40.0, true);
    assert_eq!(r.offset, 0.0, "a press alone must not move anything");

    let r = f(&mut u, 395.0, 90.0, true);
    assert!(
        (r.offset - 200.0).abs() < 0.01,
        "50px of thumb moved {} of content, wanted 200",
        r.offset
    );
    assert!(r.offset > 0.0, "thumb down must scroll down");

    let r = f(&mut u, 395.0, 90.0, false);
    assert!((r.offset - 200.0).abs() < 0.01, "release moved the content");
}

#[test]
fn clicking_the_track_pages_once_per_press() {
    ui!(u);
    let rows = 60;
    let f = |u: &mut Ui, x: f32, y: f32, down: bool| scroll_frame(u, at(x, y, down), rows, 20.0);
    f(&mut u, -1.0, -1.0, false);
    f(&mut u, -1.0, -1.0, false);

    // Below the thumb pages down by one viewport.
    let r = f(&mut u, 395.0, 250.0, true);
    assert_eq!(r.offset, 300.0, "one page is one viewport");
    let r = f(&mut u, 395.0, 250.0, true);
    assert_eq!(r.offset, 300.0, "holding must not keep paging");
    f(&mut u, 395.0, 250.0, false);

    let r = f(&mut u, 395.0, 250.0, true);
    assert_eq!(r.offset, 600.0, "a second press pages again");
    f(&mut u, 395.0, 250.0, false);

    // Above the thumb pages back up. The lead area is covered by the spacer
    // box, which is exactly the case that regressed to a dead zone once.
    let r = f(&mut u, 395.0, 20.0, true);
    assert_eq!(r.offset, 300.0);
}

#[test]
fn a_resize_settles_through_the_damage_chain_alone() {
    ui!(u);
    // No post frame call exists any more, so this is the property that
    // replaced it. The frame after a resize reads corrected geometry because
    // the resized frame was damaged and therefore followed by another build.
    let rows = 60;
    let grow = [SizeSpec::grow(), SizeSpec::grow()];
    let idle = at(-1.0, -1.0, false);
    scroll_frame_sized(&mut u, idle, rows, 20.0, grow, [400.0, 300.0]);
    scroll_frame_sized(&mut u, idle, rows, 20.0, grow, [400.0, 300.0]);

    // The resize frame itself. The visible fraction is a frame stale, but the
    // thumb is a fraction of the track, so it still spans a sane share of the
    // new 600px track instead of keeping its old pixel height.
    let r = scroll_frame_sized(&mut u, idle, rows, 20.0, grow, [400.0, 600.0]);
    assert_eq!(r.range.end, 15, "the emitted range lags the resize by one");
    let thumb = r.thumb.expect("solved");
    assert!(
        (thumb.height() - 150.0).abs() < 0.5,
        "stale fraction of the new track should give 150, got {}",
        thumb.height()
    );

    // The very next build reads the new solve and is fully consistent.
    let r = scroll_frame_sized(&mut u, idle, rows, 20.0, grow, [400.0, 600.0]);
    assert_eq!(r.view_len, 600.0);
    assert_eq!(r.max_scroll, 600.0);
    assert_eq!(r.range, VirtualRange { first: 0, end: 30 });
    let thumb = r.thumb.expect("solved");
    assert!(
        (thumb.height() - 300.0).abs() < 0.5,
        "half visible should give a half thumb, got {}",
        thumb.height()
    );
}

// Plain, non virtual children. The clamp must come from the solve's own
// content extent, since nothing declares one.

#[test]
fn plain_children_scroll_against_solved_content() {
    ui!(u);
    let frame = |u: &mut Ui, input: Input| {
        u.begin_frame(input, [400.0, 300.0]);
        let sa = ScrollArea::begin(
            u,
            SKEY,
            Axis::Y,
            [SizeSpec::px(400.0), SizeSpec::px(300.0)],
            instant(),
        )
        .expect("begin");
        for i in 0..10u64 {
            u.leaf(
                indexed(LKEY, hash_str("item"), i),
                BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::px(50.0)),
                0xff26_2636,
            )
            .expect("item");
        }
        let sa = sa.end(u).expect("end");
        u.end_frame().expect("closed");
        u.finish();
        (sa.offset(), sa.max_scroll())
    };
    frame(&mut u, at(200.0, 150.0, false));
    frame(&mut u, at(200.0, 150.0, false));
    let (_, max) = frame(&mut u, at(200.0, 150.0, false));
    assert_eq!(max, 200.0, "10 rows of 50 in a 300 window overflow by 200");
    let (offset, _) = frame(&mut u, wheeled(200.0, 150.0, -50.0));
    assert_eq!(offset, 200.0, "wheel must pin at the solved overflow");
}

// An 800x600 splitter with a filled leaf in each pane.

const PKEY: u64 = 0x77;

struct SplitOut {
    f: f32,
    a: Option<Rect>,
    b: Option<Rect>,
}

fn split_frame(u: &mut Ui, input: Input, axis: Axis, style: SplitterStyle) -> SplitOut {
    u.begin_frame(input, [800.0, 600.0]);
    let mut sp = Splitter::begin(
        u,
        PKEY,
        axis,
        [SizeSpec::px(800.0), SizeSpec::px(600.0)],
        0.5,
        style,
    )
    .expect("split begin");
    u.leaf(
        child(PKEY, hash_str("a.fill")),
        BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::grow()),
        0xff22_2230,
    )
    .expect("pane a fill");
    sp.separator(u).expect("separator");
    u.leaf(
        child(PKEY, hash_str("b.fill")),
        BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::grow()),
        0xff30_2222,
    )
    .expect("pane b fill");
    let sp = sp.end(u).expect("split end");
    u.end_frame().expect("closed");
    let a = u.geometry_of(sp.pane_a_index()).map(|g| g.0);
    let b = u.geometry_of(sp.pane_b_index()).map(|g| g.0);
    u.finish();
    SplitOut {
        f: sp.fraction(),
        a,
        b,
    }
}

#[test]
fn a_splitter_survives_a_drag_that_would_collapse_a_pane() {
    ui!(u);
    let style = SplitterStyle {
        divider: 8.0,
        min_pane: 100.0,
        ..SplitterStyle::default()
    };
    let idle = at(-1.0, -1.0, false);
    split_frame(&mut u, idle, Axis::X, style);
    split_frame(&mut u, idle, Axis::X, style);

    // Divider sits at 396..404. Drag it to x zero, far past the minimum.
    let usable = 800.0 - 8.0;
    let requested = 0.5 + (0.0 - 400.0) / usable;
    assert!(
        requested < 100.0 / usable,
        "drag would not collapse, vacuous"
    );

    split_frame(&mut u, at(400.0, 300.0, true), Axis::X, style);
    let r = split_frame(&mut u, at(0.0, 300.0, true), Axis::X, style);
    let a = r.a.expect("pane a solved");
    let b = r.b.expect("pane b solved");
    assert!((a.width() - 100.0).abs() < 0.01, "pane a is {}", a.width());
    assert!(b.width() >= 100.0 - 0.01, "pane b crushed to {}", b.width());

    // Release and idle. The clamped fraction is the persistent one.
    split_frame(&mut u, at(0.0, 300.0, false), Axis::X, style);
    let r = split_frame(&mut u, idle, Axis::X, style);
    assert!(
        (r.f - 100.0 / usable).abs() < 1.0e-4,
        "fraction drifted to {}",
        r.f
    );
    assert!((r.a.expect("solved").width() - 100.0).abs() < 0.01);
}

#[test]
fn a_vertical_splitter_clamps_the_far_side_too() {
    ui!(u);
    let style = SplitterStyle {
        divider: 8.0,
        min_pane: 100.0,
        ..SplitterStyle::default()
    };
    let idle = at(-1.0, -1.0, false);
    split_frame(&mut u, idle, Axis::Y, style);
    split_frame(&mut u, idle, Axis::Y, style);

    // Divider sits at 296..304. Drag it to the bottom edge.
    split_frame(&mut u, at(400.0, 300.0, true), Axis::Y, style);
    let r = split_frame(&mut u, at(400.0, 600.0, true), Axis::Y, style);
    let a = r.a.expect("pane a solved");
    let b = r.b.expect("pane b solved");
    assert!(
        (b.height() - 100.0).abs() < 0.01,
        "pane b is {}",
        b.height()
    );
    assert!(
        (a.height() - 492.0).abs() < 0.01,
        "pane a is {}",
        a.height()
    );
}

#[test]
fn a_zero_split_fraction_is_representable_and_persists() {
    ui!(u);
    // A zero fraction used to be the freshness sentinel and silently sprang
    // back to the initial value. `born` makes it a legitimate stored state.
    let style = SplitterStyle {
        divider: 8.0,
        min_pane: 0.0,
        ..SplitterStyle::default()
    };
    let idle = at(-1.0, -1.0, false);
    split_frame(&mut u, idle, Axis::X, style);
    split_frame(&mut u, idle, Axis::X, style);

    split_frame(&mut u, at(400.0, 300.0, true), Axis::X, style);
    let r = split_frame(&mut u, at(0.0, 300.0, true), Axis::X, style);
    assert_eq!(r.f, 0.0, "an unclamped drag to the edge is exactly zero");

    split_frame(&mut u, at(0.0, 300.0, false), Axis::X, style);
    let r = split_frame(&mut u, idle, Axis::X, style);
    assert_eq!(r.f, 0.0, "zero sprang back, the sentinel is still there");
    assert!(r.a.expect("solved").width() < 0.01);
}

// A 400 wide tab bar with 80px tabs.

const TKEY: u64 = 0x99;

fn tstyle() -> TabsStyle {
    TabsStyle {
        height: 28.0,
        pad: 0.0,
        gap: 4.0,
        ..TabsStyle::default()
    }
}

fn tabs_frame_inner(
    u: &mut Ui,
    input: Input,
    ids: &[u64],
    animate_bar_key: bool,
) -> (crate::tabs::TabsResult, [bool; 4]) {
    u.begin_frame(input, [400.0, 100.0]);
    if animate_bar_key {
        // The selected id lives in the state's user words, which animate must
        // never touch. Packing it into the float fields was corruptible here.
        u.animate(TKEY, 1.0, 20.0);
    }
    let mut t = Tabs::begin(u, TKEY, SizeSpec::px(400.0), tstyle()).expect("tabs begin");
    let mut act = [false; 4];
    for (n, id) in ids.iter().enumerate() {
        act[n] = t.item(u, *id, 80.0).expect("tab").active;
    }
    let res = t.end(u).expect("tabs end");
    u.end_frame().expect("closed");
    u.finish();
    (res, act)
}

fn tabs_frame(u: &mut Ui, input: Input, ids: &[u64]) -> (crate::tabs::TabsResult, [bool; 4]) {
    tabs_frame_inner(u, input, ids, false)
}

#[test]
fn tab_selection_persists_across_frames_and_reorders() {
    ui!(u);
    let a = hash_str("alpha");
    let b = hash_str("beta");
    let c = hash_str("gamma");
    let ids = [a, b, c];
    let idle = at(-1.0, -1.0, false);

    let (r, _) = tabs_frame(&mut u, idle, &ids);
    assert_eq!(r.active, Some(a), "first tab wins by default");
    assert!(r.changed);
    let (r, act) = tabs_frame(&mut u, idle, &ids);
    assert!(!r.changed, "an idle frame must not report a change");
    assert!(act[0]);

    // Tab beta spans x 84..164. Press and release on it. On the click frame
    // itself only later items see the new selection, since earlier ones were
    // emitted before the click was processed.
    tabs_frame(&mut u, at(100.0, 14.0, true), &ids);
    let (r, act) = tabs_frame(&mut u, at(100.0, 14.0, false), &ids);
    assert_eq!(r.active, Some(b), "the click did not select");
    assert!(r.changed);
    assert!(act[1]);

    let (r, act) = tabs_frame(&mut u, idle, &ids);
    assert_eq!(r.active, Some(b), "selection lost between frames");
    assert!(!r.changed);
    assert!(act[1] && !act[0] && !act[2]);

    // Reordered, the id keeps the selection even though the position moved.
    let (r, act) = tabs_frame(&mut u, idle, &[c, b, a]);
    assert_eq!(r.active, Some(b));
    assert!(act[1] && !act[0] && !act[2]);
    assert!(!r.changed, "a reorder is not a selection change");
}

#[test]
fn tabs_fall_back_when_the_selected_tab_disappears() {
    ui!(u);
    let a = hash_str("alpha");
    let b = hash_str("beta");
    let c = hash_str("gamma");
    let idle = at(-1.0, -1.0, false);

    tabs_frame(&mut u, idle, &[a, b, c]);
    tabs_frame(&mut u, at(100.0, 14.0, true), &[a, b, c]);
    let (r, _) = tabs_frame(&mut u, at(100.0, 14.0, false), &[a, b, c]);
    assert_eq!(r.active, Some(b), "setup click failed, test is vacuous");

    // The fallback lands in end, after the items were emitted, so the flag
    // shows on the next frame.
    let (r, _) = tabs_frame(&mut u, idle, &[a, c]);
    assert_eq!(r.active, Some(a), "closing the active tab must fall back");
    assert!(r.changed);
    let (r, act) = tabs_frame(&mut u, idle, &[a, c]);
    assert_eq!(r.active, Some(a));
    assert!(!r.changed);
    assert!(act[0] && !act[1]);
}

#[test]
fn animating_the_bar_key_cannot_corrupt_the_selection() {
    ui!(u);
    let a = hash_str("alpha");
    let b = hash_str("beta");
    let ids = [a, b];
    let idle = at(-1.0, -1.0, false);

    tabs_frame(&mut u, idle, &ids);
    tabs_frame(&mut u, at(100.0, 14.0, true), &ids);
    let (r, _) = tabs_frame(&mut u, at(100.0, 14.0, false), &ids);
    assert_eq!(r.active, Some(b), "setup click failed, test is vacuous");

    for _ in 0..3 {
        let (r, _) = tabs_frame_inner(&mut u, idle, &ids, true);
        assert_eq!(r.active, Some(b), "animate clobbered the stored id");
        assert!(!r.changed);
    }
}

// Three 40px rows in the 400x300 scroll area, reordered mid drag.

fn rows_frame(u: &mut Ui, input: Input, order: &[u64; 3]) -> ([Response; 3], [Option<Rect>; 3]) {
    u.begin_frame(input, [400.0, 300.0]);
    let sa = ScrollArea::begin(
        u,
        SKEY,
        Axis::Y,
        [SizeSpec::px(400.0), SizeSpec::px(300.0)],
        instant(),
    )
    .expect("scroll begin");
    let vl = VirtualList::begin(u, LKEY, sa.offset(), sa.view_len(), 3, 40.0).expect("vlist");
    let mut rs = [Response::default(); 3];
    let mut idx = [0u32; 3];
    for (n, id) in order.iter().enumerate() {
        rs[n] = vl.row(u, *id, 0xff44_4455).expect("row");
        idx[n] = u.last_index();
    }
    vl.end(u).expect("vlist end");
    sa.end(u).expect("scroll end");
    u.end_frame().expect("closed");
    let rects = [
        u.geometry_of(idx[0]).map(|g| g.0),
        u.geometry_of(idx[1]).map(|g| g.0),
        u.geometry_of(idx[2]).map(|g| g.0),
    ];
    u.finish();
    (rs, rects)
}

#[test]
fn row_identity_survives_reordering_when_ids_are_stable() {
    ui!(u);
    let fwd = [10u64, 20, 30];
    let rev = [30u64, 20, 10];
    rows_frame(&mut u, at(-1.0, -1.0, false), &fwd);
    rows_frame(&mut u, at(100.0, 20.0, false), &fwd);

    // Press row id 10, sitting at the top.
    let (rs, rects) = rows_frame(&mut u, at(100.0, 20.0, true), &fwd);
    assert!(rs[0].active, "press missed row 10, test is vacuous");
    let top_before = rects[0].expect("solved").min[1];

    // Same press held, list re-sorted. The drag must follow the id to its
    // new position, not stay with whatever now sits under the pointer.
    let (rs, rects) = rows_frame(&mut u, at(100.0, 20.0, true), &rev);
    let id10_rect = rects[2].expect("solved");
    assert!(rs[2].active, "row 10 lost its active state in the re-sort");
    assert!(!rs[0].active, "row 30 stole the drag by taking position 0");
    assert!(
        (id10_rect.min[1] - top_before).abs() > 1.0,
        "row 10 never moved, reorder did not happen and the test is vacuous"
    );
}

#[test]
fn scroll_state_belongs_to_the_list_not_the_ordering() {
    ui!(u);
    let rows = 100;
    let idle = at(-1.0, -1.0, false);
    scroll_frame(&mut u, idle, rows, 20.0);
    scroll_frame(&mut u, idle, rows, 20.0);
    u.state(SKEY).expect("room").scroll[1] = 400.0;
    let r = scroll_frame(&mut u, idle, rows, 20.0);
    assert_eq!(r.offset, 400.0);
    // Rows come and go as the range moves, the offset stays keyed to the area.
    let r = scroll_frame(&mut u, idle, rows, 20.0);
    assert_eq!(r.offset, 400.0);
}

#[test]
fn bad_parameters_are_refused_before_any_box_is_emitted() {
    ui!(u);
    u.begin_frame(at(-1.0, -1.0, false), [100.0, 100.0]);

    let e = VirtualList::begin(&mut u, 1, 0.0, 100.0, 10, 0.0);
    assert!(matches!(e, Err(WidgetError::RowHeightNotPositive)));

    let bad_bar = ScrollStyle {
        bar_size: 0.0,
        ..ScrollStyle::default()
    };
    let e = ScrollArea::begin(
        &mut u,
        2,
        Axis::Y,
        [SizeSpec::grow(), SizeSpec::grow()],
        bad_bar,
    );
    assert!(matches!(e, Err(WidgetError::BarSizeNotPositive)));

    let size = [SizeSpec::grow(), SizeSpec::grow()];
    let bad_div = SplitterStyle {
        divider: 0.0,
        ..SplitterStyle::default()
    };
    let e = Splitter::begin(&mut u, 3, Axis::X, size, 0.5, bad_div);
    assert!(matches!(e, Err(WidgetError::DividerNotPositive)));

    let bad_min = SplitterStyle {
        min_pane: -1.0,
        ..SplitterStyle::default()
    };
    let e = Splitter::begin(&mut u, 4, Axis::X, size, 0.5, bad_min);
    assert!(matches!(e, Err(WidgetError::MinPaneNegative)));

    let e = Splitter::begin(&mut u, 5, Axis::X, size, 1.0, SplitterStyle::default());
    assert!(matches!(e, Err(WidgetError::InitialFractionOutOfRange)));

    let bad_tabs = TabsStyle {
        height: 0.0,
        ..TabsStyle::default()
    };
    let e = Tabs::begin(&mut u, 6, SizeSpec::grow(), bad_tabs);
    assert!(matches!(e, Err(WidgetError::TabHeightNotPositive)));

    // Every refusal above must leave the frame balanced.
    u.end_frame().expect("a refused widget emitted boxes");
    u.finish();
}

// Tables. A four column table over ten rows, driven a frame at a time so the
// one frame geometry lag is visible rather than hidden behind a settle loop.

const TBLKEY: u64 = 0x70;
const TW: f32 = 400.0;
const TH: f32 = 300.0;

fn cols4() -> [Column<'static>; 4] {
    [
        Column::new("name", ColumnWidth::Grow(2.0)),
        Column::new("bytes", ColumnWidth::Grow(1.0)),
        Column::new("count", ColumnWidth::Px(60.0)),
        Column::new("pct", ColumnWidth::Grow(1.0)),
    ]
}

struct TableOut {
    widths: [f32; 4],
    sort: Sort,
    sort_changed: bool,
    heads: [Rect; 4],
    cells: [Rect; 4],
    total: f32,
    boxes: usize,
}

/// The first row's cell indices, so the solved rects can be compared with the
/// header's. A free function because the lint counts the impl and both loops.
fn record_first_row(cidx: &mut [u32; 4], id: u64, col: usize, index: u32) {
    if id == 0 && col < 4 {
        cidx[col] = index;
    }
}

fn table_frame(u: &mut Ui, input: Input, cols: &[Column<'_>], rows: u64) -> TableOut {
    table_frame_sized(u, input, cols, rows, TableStyle::default(), TW)
}

fn table_frame_sized(
    u: &mut Ui,
    input: Input,
    cols: &[Column<'_>],
    rows: u64,
    style: TableStyle,
    w: f32,
) -> TableOut {
    u.begin_frame(input, [w, TH]);
    let mut t = Table::begin(u, TBLKEY, cols, [SizeSpec::px(w), SizeSpec::px(TH)], style)
        .expect("table begin");
    let mut heads = [Rect::default(); 4];
    let mut hidx = [0u32; 4];
    for (c, slot) in hidx.iter_mut().enumerate().take(cols.len().min(4)) {
        *slot = t.head_index(c).expect("in range");
    }
    let mut cidx = [0u32; 4];
    for id in 0..rows {
        t.row(u, id, 0xff11_1118).expect("row");
        for c in 0..cols.len() {
            t.cell(u, id, c, 0).expect("cell");
            record_first_row(&mut cidx, id, c, u.last_index());
        }
        t.end_row(u).expect("end row");
    }
    let widths = {
        let w = t.widths();
        let mut out = [0.0f32; 4];
        out[..w.len().min(4)].copy_from_slice(&w[..w.len().min(4)]);
        out
    };
    let sort = t.sort();
    let sort_changed = t.sort_changed();
    let total = t.total_width();
    let t = t.end(u).expect("table end");
    let _ = t.key();
    u.end_frame().expect("balanced");
    let boxes = u.box_count();
    for c in 0..cols.len().min(4) {
        heads[c] = u.rect_of(hidx[c]).expect("solved");
    }
    let mut cells = [Rect::default(); 4];
    if rows > 0 {
        for c in 0..cols.len().min(4) {
            cells[c] = u.rect_of(cidx[c]).expect("solved");
        }
    }
    u.finish();
    TableOut {
        widths,
        sort,
        sort_changed,
        heads,
        cells,
        total,
        boxes,
    }
}

fn wide(w: f32) -> f32 {
    w
}

#[test]
fn grow_columns_split_what_the_fixed_ones_leave() {
    ui!(u);
    let cols = cols4();
    table_frame(&mut u, at(0.0, 0.0, false), &cols, 3);
    // Second frame: the outer rect from the first solve is now readable.
    let o = table_frame(&mut u, at(0.0, 0.0, false), &cols, 3);
    // 400 total, 60 fixed, 340 split 2:1:1.
    assert!((o.widths[0] - 170.0).abs() < 0.01, "{:?}", o.widths);
    assert!((o.widths[1] - 85.0).abs() < 0.01, "{:?}", o.widths);
    assert!((o.widths[2] - 60.0).abs() < 0.01, "{:?}", o.widths);
    assert!((o.widths[3] - 85.0).abs() < 0.01, "{:?}", o.widths);
    assert!((o.total - wide(TW)).abs() < 0.01, "total {}", o.total);
}

#[test]
fn header_cells_and_body_cells_share_column_edges() {
    ui!(u);
    let cols = cols4();
    table_frame(&mut u, at(0.0, 0.0, false), &cols, 3);
    let o = table_frame(&mut u, at(0.0, 0.0, false), &cols, 3);
    for c in 0..4 {
        assert!(
            (o.heads[c].min[0] - o.cells[c].min[0]).abs() < 0.01,
            "column {c} left edge: head {:?} cell {:?}",
            o.heads[c],
            o.cells[c]
        );
        assert!(
            (o.heads[c].max[0] - o.cells[c].max[0]).abs() < 0.01,
            "column {c} right edge: head {:?} cell {:?}",
            o.heads[c],
            o.cells[c]
        );
    }
    // And they tile: no gaps, no overlap.
    for c in 1..4 {
        assert!((o.heads[c].min[0] - o.heads[c - 1].max[0]).abs() < 0.01);
    }
}

#[test]
fn dividers_float_so_they_cost_no_column_width() {
    ui!(u);
    let cols = cols4();
    table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    let fat = TableStyle {
        divider: 20.0,
        ..TableStyle::default()
    };
    let o = table_frame_sized(&mut u, at(0.0, 0.0, false), &cols, 0, fat, TW);
    // Three 20px dividers would have eaten 60px if they were in flow.
    assert!((o.total - wide(TW)).abs() < 0.01, "total {}", o.total);
    assert!((o.heads[3].max[0] - o.heads[0].min[0] - TW).abs() < 0.01);
}

#[test]
fn a_minimum_floors_a_column_and_the_overflow_stays_visible() {
    ui!(u);
    let cols = [
        Column::new("a", ColumnWidth::Grow(1.0)).min(200.0),
        Column::new("b", ColumnWidth::Grow(1.0)).min(200.0),
        Column::new("c", ColumnWidth::Grow(1.0)).min(200.0),
        Column::new("d", ColumnWidth::Grow(1.0)).min(200.0),
    ];
    table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    let o = table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    for w in o.widths {
        assert!((w - 200.0).abs() < 0.01, "{:?}", o.widths);
    }
    // 800 in a 400 box. Honest, and the header clips it rather than lying.
    assert!((o.total - 800.0).abs() < 0.01, "{}", o.total);
}

#[test]
fn a_table_that_has_not_been_solved_yet_asks_for_another_frame() {
    ui!(u);
    let cols = cols4();
    u.begin_frame(at(0.0, 0.0, false), [TW, TH]);
    let t = Table::begin(
        &mut u,
        TBLKEY,
        &cols,
        [SizeSpec::px(TW), SizeSpec::px(TH)],
        TableStyle::default(),
    )
    .expect("begin");
    t.end(&mut u).expect("end");
    u.end_frame().expect("balanced");
    let r = u.finish();
    assert!(r.repaint_requested, "the first frame sized columns blind");
}

// Dragging column 0's divider 50px right. The divider for column 0 straddles
// x = widths[0], so the press has to land there.

fn drag_divider(u: &mut Ui, cols: &[Column<'_>], col_edge: f32, dx: f32) {
    let y = 12.0;
    table_frame(u, at(col_edge, y, true), cols, 0);
    table_frame(u, at(col_edge + dx, y, true), cols, 0);
    table_frame(u, at(col_edge + dx, y, false), cols, 0);
}

#[test]
fn dragging_a_divider_pins_the_column_to_its_left() {
    ui!(u);
    let cols = cols4();
    table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    let before = table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    assert!((before.widths[0] - 170.0).abs() < 0.01);

    drag_divider(&mut u, &cols, 170.0, 50.0);
    let after = table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    assert!(
        (after.widths[0] - 220.0).abs() < 1.0,
        "widths {:?}",
        after.widths
    );
    // The pin is not a one frame thing.
    let later = table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    assert!((later.widths[0] - after.widths[0]).abs() < 0.01);
    // And the grow columns gave up the space, not the fixed one.
    assert!((later.widths[2] - 60.0).abs() < 0.01, "{:?}", later.widths);
}

#[test]
fn a_pinned_column_keeps_its_pixels_when_the_table_resizes() {
    ui!(u);
    let cols = cols4();
    table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    drag_divider(&mut u, &cols, 170.0, 50.0);

    let style = TableStyle::default();
    table_frame_sized(&mut u, at(0.0, 0.0, false), &cols, 0, style, 600.0);
    let o = table_frame_sized(&mut u, at(0.0, 0.0, false), &cols, 0, style, 600.0);
    assert!((o.widths[0] - 220.0).abs() < 1.0, "{:?}", o.widths);
    assert!((o.widths[2] - 60.0).abs() < 0.01, "{:?}", o.widths);
    // 600 - 220 - 60 = 320, split 1:1 between columns 1 and 3.
    assert!((o.widths[1] - 160.0).abs() < 1.0, "{:?}", o.widths);
    assert!((o.widths[3] - 160.0).abs() < 1.0, "{:?}", o.widths);
}

#[test]
fn a_drag_cannot_take_a_column_below_its_minimum() {
    ui!(u);
    let cols = [
        Column::new("a", ColumnWidth::Grow(1.0)).min(80.0),
        Column::new("b", ColumnWidth::Grow(1.0)),
    ];
    table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    drag_divider(&mut u, &cols, 200.0, -500.0);
    let o = table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    assert!((o.widths[0] - 80.0).abs() < 0.01, "{:?}", o.widths);
}

#[test]
fn reset_widths_puts_the_columns_back_on_their_spec() {
    ui!(u);
    let cols = cols4();
    table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    drag_divider(&mut u, &cols, 170.0, 60.0);
    let pinned = table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    assert!(pinned.widths[0] > 200.0);

    u.begin_frame(at(0.0, 0.0, false), [TW, TH]);
    let t = Table::begin(
        &mut u,
        TBLKEY,
        &cols,
        [SizeSpec::px(TW), SizeSpec::px(TH)],
        TableStyle::default(),
    )
    .expect("begin");
    t.reset_widths(&mut u).expect("reset");
    t.end(&mut u).expect("end");
    u.end_frame().expect("balanced");
    u.finish();

    let o = table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    assert!((o.widths[0] - 170.0).abs() < 0.01, "{:?}", o.widths);
}

// Sorting. Header cell 0 spans x 0..170 at y 0..24 once solved.

fn click_header(u: &mut Ui, cols: &[Column<'_>], x: f32) -> TableOut {
    table_frame(u, at(x, 12.0, true), cols, 0);
    table_frame(u, at(x, 12.0, false), cols, 0)
}

#[test]
fn clicking_a_header_cycles_ascending_descending_off() {
    ui!(u);
    let cols = cols4();
    table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    let o = table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    assert_eq!(o.sort, Sort::default());

    let o = click_header(&mut u, &cols, 80.0);
    assert_eq!(
        o.sort,
        Sort {
            column: Some(0),
            descending: false
        }
    );
    assert!(o.sort_changed);
    // A frame that changed the order owes a repaint, because the rows it just
    // built are still in the old one.
    let o = click_header(&mut u, &cols, 80.0);
    assert_eq!(
        o.sort,
        Sort {
            column: Some(0),
            descending: true
        }
    );
    let o = click_header(&mut u, &cols, 80.0);
    assert_eq!(o.sort, Sort::default(), "third click clears it");
}

#[test]
fn sorting_moves_to_the_column_that_was_clicked() {
    ui!(u);
    let cols = cols4();
    table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    click_header(&mut u, &cols, 80.0);
    // Column 1 spans 170..255.
    let o = click_header(&mut u, &cols, 200.0);
    assert_eq!(
        o.sort,
        Sort {
            column: Some(1),
            descending: false
        },
        "a new column starts ascending rather than inheriting"
    );
}

#[test]
fn a_column_marked_fixed_does_not_sort() {
    ui!(u);
    let cols = [
        Column::new("a", ColumnWidth::Grow(1.0)).fixed(),
        Column::new("b", ColumnWidth::Grow(1.0)),
    ];
    table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    let o = click_header(&mut u, &cols, 50.0);
    assert_eq!(o.sort, Sort::default());
    assert!(!o.sort_changed);
}

#[test]
fn a_sort_set_directly_is_read_back_next_frame() {
    ui!(u);
    let cols = cols4();
    table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    u.begin_frame(at(0.0, 0.0, false), [TW, TH]);
    let mut t = Table::begin(
        &mut u,
        TBLKEY,
        &cols,
        [SizeSpec::px(TW), SizeSpec::px(TH)],
        TableStyle::default(),
    )
    .expect("begin");
    t.set_sort(
        &mut u,
        Sort {
            column: Some(3),
            descending: true,
        },
    )
    .expect("set");
    t.end(&mut u).expect("end");
    u.end_frame().expect("balanced");
    u.finish();

    let o = table_frame(&mut u, at(0.0, 0.0, false), &cols, 0);
    assert_eq!(
        o.sort,
        Sort {
            column: Some(3),
            descending: true
        }
    );
    assert!(!o.sort_changed, "restoring is not a user edit");
}

#[test]
fn a_cell_keeps_its_key_when_the_rows_are_reordered() {
    ui!(u);
    let cols = cols4();
    u.begin_frame(at(0.0, 0.0, false), [TW, TH]);
    let t = Table::begin(
        &mut u,
        TBLKEY,
        &cols,
        [SizeSpec::px(TW), SizeSpec::px(TH)],
        TableStyle::default(),
    )
    .expect("begin");
    // Same row id, different position, has to be the same key. Otherwise a
    // re-sort drops selection and hover on the floor.
    let a = t.cell_key(77, 2);
    let b = t.cell_key(77, 2);
    let other = t.cell_key(78, 2);
    let other_col = t.cell_key(77, 3);
    t.end(&mut u).expect("end");
    u.end_frame().expect("balanced");
    u.finish();
    assert_eq!(a, b);
    assert_ne!(a, other);
    assert_ne!(a, other_col);
}

#[test]
fn a_thousand_row_table_only_emits_what_the_caller_asks_for() {
    ui!(u);
    let cols = cols4();
    // Ten rows of four cells, plus a row container each, plus the outer box,
    // the header row, four header cells and three dividers.
    let o = table_frame(&mut u, at(0.0, 0.0, false), &cols, 10);
    assert_eq!(o.boxes, 1 + 1 + 4 + 3 + 10 * 5, "boxes {}", o.boxes);
}

#[test]
fn cell_content_insets_and_alignment_agree_with_the_column() {
    ui!(u);
    let cols = [
        Column::new("l", ColumnWidth::Grow(1.0)),
        Column::new("c", ColumnWidth::Grow(1.0)).align(Align::Center),
        Column::new("r", ColumnWidth::Grow(1.0)).align(Align::End),
    ];
    u.begin_frame(at(0.0, 0.0, false), [TW, TH]);
    let t = Table::begin(
        &mut u,
        TBLKEY,
        &cols,
        [SizeSpec::px(TW), SizeSpec::px(TH)],
        TableStyle::default(),
    )
    .expect("begin");
    let cell = Rect {
        min: [100.0, 0.0],
        max: [200.0, 20.0],
    };
    let content = t.cell_content(cell);
    assert!((content.min[0] - 106.0).abs() < 0.01);
    assert!((content.max[0] - 194.0).abs() < 0.01);
    // 88px of content box, 40px of text.
    assert!((t.align_x(0, cell, 40.0) - 106.0).abs() < 0.01);
    assert!((t.align_x(1, cell, 40.0) - 130.0).abs() < 0.01);
    assert!((t.align_x(2, cell, 40.0) - 154.0).abs() < 0.01);
    // Text wider than the cell pins to the start rather than going negative.
    assert!((t.align_x(2, cell, 400.0) - 106.0).abs() < 0.01);
    t.end(&mut u).expect("end");
    u.end_frame().expect("balanced");
    u.finish();
}

#[test]
fn a_cell_narrower_than_its_padding_has_a_zero_width_content_box() {
    ui!(u);
    let cols = cols4();
    u.begin_frame(at(0.0, 0.0, false), [TW, TH]);
    let t = Table::begin(
        &mut u,
        TBLKEY,
        &cols,
        [SizeSpec::px(TW), SizeSpec::px(TH)],
        TableStyle::default(),
    )
    .expect("begin");
    let tiny = Rect {
        min: [0.0, 0.0],
        max: [4.0, 20.0],
    };
    let c = t.cell_content(tiny);
    assert!(c.max[0] >= c.min[0], "{c:?}");
    assert!((c.max[0] - c.min[0]).abs() < 0.01);
    t.end(&mut u).expect("end");
    u.end_frame().expect("balanced");
    u.finish();
}

#[test]
fn tables_refuse_bad_input_without_unbalancing_the_frame() {
    ui!(u);
    u.begin_frame(at(0.0, 0.0, false), [TW, TH]);
    let size = [SizeSpec::px(TW), SizeSpec::px(TH)];
    let style = TableStyle::default();
    let cols = cols4();

    let e = Table::begin(&mut u, 1, &[], size, style);
    assert!(matches!(e, Err(WidgetError::ColumnCountOutOfRange)));

    let too_many = [Column::new("x", ColumnWidth::Grow(1.0)); MAX_COLUMNS + 1];
    let e = Table::begin(&mut u, 2, &too_many, size, style);
    assert!(matches!(e, Err(WidgetError::ColumnCountOutOfRange)));

    let bad_h = TableStyle {
        row_h: 0.0,
        ..style
    };
    let e = Table::begin(&mut u, 3, &cols, size, bad_h);
    assert!(matches!(e, Err(WidgetError::RowHeightNotPositive)));

    let bad_div = TableStyle {
        divider: 0.0,
        ..style
    };
    let e = Table::begin(&mut u, 4, &cols, size, bad_div);
    assert!(matches!(e, Err(WidgetError::DividerNotPositive)));

    let bad_min = [Column::new("x", ColumnWidth::Grow(1.0)).min(-1.0)];
    let e = Table::begin(&mut u, 5, &bad_min, size, style);
    assert!(matches!(e, Err(WidgetError::MinPaneNegative)));

    u.end_frame().expect("a refused table emitted boxes");
    u.finish();
}

#[test]
fn a_row_left_open_is_refused_rather_than_unbalancing_the_frame() {
    ui!(u);
    u.begin_frame(at(0.0, 0.0, false), [TW, TH]);
    let cols = cols4();
    let mut t = Table::begin(
        &mut u,
        TBLKEY,
        &cols,
        [SizeSpec::px(TW), SizeSpec::px(TH)],
        TableStyle::default(),
    )
    .expect("begin");
    t.row(&mut u, 0, 0).expect("row");
    assert!(matches!(
        t.row(&mut u, 1, 0),
        Err(WidgetError::RowAlreadyOpen)
    ));
    // Ending the table with a row open is refused, so the row can be closed.
    t.end_row(&mut u).expect("close");
    assert!(matches!(t.end_row(&mut u), Err(WidgetError::NoRowOpen)));
    t.end(&mut u).expect("end");
    u.end_frame().expect("balanced");
    u.finish();
}

#[test]
fn a_cell_past_the_last_column_is_refused() {
    ui!(u);
    u.begin_frame(at(0.0, 0.0, false), [TW, TH]);
    let cols = cols4();
    let mut t = Table::begin(
        &mut u,
        TBLKEY,
        &cols,
        [SizeSpec::px(TW), SizeSpec::px(TH)],
        TableStyle::default(),
    )
    .expect("begin");
    t.row(&mut u, 0, 0).expect("row");
    assert!(matches!(
        t.cell(&mut u, 0, 4, 0),
        Err(WidgetError::ColumnOutOfRange)
    ));
    assert!(t.head_index(4).is_none());
    t.end_row(&mut u).expect("close");
    t.end(&mut u).expect("end");
    u.end_frame().expect("balanced");
    u.finish();
}

#[test]
fn a_sort_pointing_past_the_column_set_is_cleared() {
    let s = Sort {
        column: Some(7),
        descending: true,
    };
    assert_eq!(clamp_sort(s, 4), Sort::default());
    assert_eq!(clamp_sort(s, 8), s);
    assert_eq!(clamp_sort(Sort::default(), 0), Sort::default());
}

#[test]
fn divider_offsets_accumulate_column_widths() {
    let w = [170.0f32, 85.0, 60.0, 85.0];
    assert!((divider_offset(&w, 0) - 170.0).abs() < 0.01);
    assert!((divider_offset(&w, 1) - 255.0).abs() < 0.01);
    assert!((divider_offset(&w, 2) - 315.0).abs() < 0.01);
}

#[test]
fn char_fitting_refuses_nonsense_rather_than_dividing_by_zero() {
    assert_eq!(chars_that_fit(100.0, 0.0), 0);
    assert_eq!(chars_that_fit(0.0, 8.0), 0);
    assert_eq!(chars_that_fit(-5.0, 8.0), 0);
    assert_eq!(chars_that_fit(80.0, 8.0), 10);
    // A huge box does not produce a nonsense count a caller would index with.
    assert!(chars_that_fit(f32::INFINITY, 8.0) <= 4096);
}

// Text views. A monospaced document with known metrics, so every pixel the
// view reports can be checked against arithmetic rather than another solve.

const VKEY: u64 = 0x80;
const LH: f32 = 10.0;
const ADV: f32 = 8.0;
const VW: f32 = 400.0;
const VH: f32 = 200.0;

fn metrics() -> TextMetrics {
    TextMetrics {
        line_h: LH,
        advance: ADV,
    }
}

struct TextOut {
    visible: LineRange,
    first_col: u32,
    offset: [f32; 2],
    caret: Caret,
    selection: Option<(Caret, Caret)>,
    sel_rects: SelectionRects,
    viewport: Rect,
    boxes: usize,
}

fn text_frame(u: &mut Ui, input: Input, lines: u32, cols: u32) -> TextOut {
    text_frame_styled(u, input, lines, cols, TextViewStyle::default())
}

fn text_frame_styled(
    u: &mut Ui,
    input: Input,
    lines: u32,
    cols: u32,
    style: TextViewStyle,
) -> TextOut {
    u.begin_frame(input, [VW, VH]);
    let t = TextView::begin(
        u,
        VKEY,
        [SizeSpec::px(VW), SizeSpec::px(VH)],
        lines,
        cols,
        metrics(),
        style,
    )
    .expect("begin");
    let t = t.end(u).expect("end");
    u.end_frame().expect("balanced");
    let boxes = u.box_count();
    let out = TextOut {
        visible: t.visible_lines(),
        first_col: t.first_column(),
        offset: t.offset(),
        caret: t.caret(),
        selection: t.selection(),
        sel_rects: t.selection_rects(),
        viewport: t.viewport(),
        boxes,
    };
    u.finish();
    out
}

fn settled(u: &mut Ui, lines: u32, cols: u32) -> TextOut {
    let idle = at(-1.0, -1.0, false);
    text_frame(u, idle, lines, cols);
    text_frame(u, idle, lines, cols)
}

#[test]
fn a_hundred_thousand_lines_cost_a_handful_of_boxes() {
    ui!(u);
    let o = settled(&mut u, 100_000, 200);
    // Outer, row, viewport, and a track plus space plus thumb per axis.
    assert_eq!(o.boxes, 3 + 2 * 3, "boxes {}", o.boxes);
    // 190px of text height, because the horizontal bar takes the other ten.
    assert_eq!(o.visible, LineRange { first: 0, end: 19 });
}

#[test]
fn the_visible_range_follows_the_offset_exactly() {
    ui!(u);
    settled(&mut u, 1_000, 80);
    u.state(VKEY).expect("room").scroll[1] = 505.0;
    let o = settled(&mut u, 1_000, 80);
    // 505 / 10 = line 50 partly visible, 190px of view reaches into line 69.
    assert_eq!(o.visible, LineRange { first: 50, end: 70 });
}

#[test]
fn scrolling_stops_at_the_end_of_the_document() {
    ui!(u);
    settled(&mut u, 100, 40);
    u.state(VKEY).expect("room").scroll[1] = 9.0e9;
    let o = settled(&mut u, 100, 40);
    // 40 columns fit, so the horizontal bar hides and 100 lines of 10px in
    // a 200px view leave 800px of travel.
    assert!((o.offset[1] - 800.0).abs() < 0.01, "{:?}", o.offset);
    assert_eq!(
        o.visible,
        LineRange {
            first: 80,
            end: 100
        }
    );
}

#[test]
fn a_document_shorter_than_the_view_does_not_scroll_at_all() {
    ui!(u);
    settled(&mut u, 5, 10);
    u.state(VKEY).expect("room").scroll = [500.0, 500.0];
    let o = settled(&mut u, 5, 10);
    assert_eq!(o.offset, [0.0, 0.0]);
    assert_eq!(o.visible, LineRange { first: 0, end: 5 });
}

#[test]
fn the_wheel_moves_by_whole_lines_and_only_inside_the_view() {
    ui!(u);
    settled(&mut u, 1_000, 40);
    let inside = wheeled(200.0, 100.0, -1.0);
    let o = text_frame(&mut u, inside, 1_000, 40);
    // Three lines per notch.
    assert!((o.offset[1] - 30.0).abs() < 0.01, "{:?}", o.offset);

    let outside = wheeled(-50.0, -50.0, -1.0);
    let o = text_frame(&mut u, outside, 1_000, 40);
    assert!(
        (o.offset[1] - 30.0).abs() < 0.01,
        "a wheel outside scrolled it"
    );
}

#[test]
fn horizontal_extent_comes_from_the_widest_line() {
    ui!(u);
    settled(&mut u, 10, 200);
    u.state(VKEY).expect("room").scroll[0] = 9.0e9;
    let o = settled(&mut u, 10, 200);
    // 200 columns of 8px is 1600 and a 400px view leaves 1200 of travel. Ten
    // lines fit, so the vertical bar is hidden and takes no width.
    let expect = 200.0 * ADV - VW;
    assert!((o.offset[0] - expect).abs() < 0.01, "{:?}", o.offset);
    assert_eq!(o.first_col, (expect / ADV) as u32);
}

#[test]
fn a_gutter_narrows_the_text_area_without_moving_the_viewport() {
    ui!(u);
    let style = TextViewStyle {
        gutter_cols: 5,
        gutter_gap: 1,
        ..TextViewStyle::default()
    };
    let idle = at(-1.0, -1.0, false);
    text_frame_styled(&mut u, idle, 100, 40, style);
    let o = text_frame_styled(&mut u, idle, 100, 40, style);
    let _ = o;

    u.begin_frame(idle, [VW, VH]);
    let t = TextView::begin(
        &mut u,
        VKEY,
        [SizeSpec::px(VW), SizeSpec::px(VH)],
        100,
        40,
        metrics(),
        style,
    )
    .expect("begin");
    let t = t.end(&mut u).expect("end");
    u.end_frame().expect("balanced");
    // Six characters of gutter at 8px each.
    assert!((t.gutter_width() - 48.0).abs() < 0.01);
    let g = t.gutter();
    let ta = t.text_area();
    assert!((g.max[0] - g.min[0] - 48.0).abs() < 0.01);
    assert!(
        (ta.min[0] - g.max[0]).abs() < 0.01,
        "gutter and text overlap"
    );
    assert!((t.viewport().min[0] - g.min[0]).abs() < 0.01);
    // Line rects start after the gutter, gutter rects before it.
    assert!((t.line_rect(0).min[0] - ta.min[0]).abs() < 0.01);
    assert!((t.gutter_rect(0).max[0] - g.max[0]).abs() < 0.01);
    u.finish();
}

#[test]
fn line_geometry_is_the_arithmetic_and_nothing_else() {
    ui!(u);
    settled(&mut u, 1_000, 40);
    u.state(VKEY).expect("room").scroll[1] = 250.0;
    let idle = at(-1.0, -1.0, false);
    text_frame(&mut u, idle, 1_000, 40);

    u.begin_frame(idle, [VW, VH]);
    let t = TextView::begin(
        &mut u,
        VKEY,
        [SizeSpec::px(VW), SizeSpec::px(VH)],
        1_000,
        40,
        metrics(),
        TextViewStyle::default(),
    )
    .expect("begin");
    let t = t.end(&mut u).expect("end");
    u.end_frame().expect("balanced");
    let vp = t.viewport();
    // Line 25 is exactly at the top once 250px have scrolled away.
    let r = t.line_rect(25);
    assert!((r.min[1] - vp.min[1]).abs() < 0.01, "{r:?} vp {vp:?}");
    assert!((r.max[1] - r.min[1] - LH).abs() < 0.01);
    let o = t.line_origin(26);
    assert!((o[1] - vp.min[1] - LH).abs() < 0.01);
    u.finish();
}

#[test]
fn a_click_maps_to_the_cell_it_landed_on() {
    ui!(u);
    settled(&mut u, 100, 40);
    let idle = at(-1.0, -1.0, false);
    u.begin_frame(idle, [VW, VH]);
    let t = TextView::begin(
        &mut u,
        VKEY,
        [SizeSpec::px(VW), SizeSpec::px(VH)],
        100,
        40,
        metrics(),
        TextViewStyle::default(),
    )
    .expect("begin");
    let t = t.end(&mut u).expect("end");
    u.end_frame().expect("balanced");
    let vp = t.viewport();
    // Left edge of column 0, line 0.
    assert_eq!(
        t.caret_at([vp.min[0], vp.min[1]]),
        Caret { line: 0, col: 0 }
    );
    // Third line, and a click in the right half of a cell lands after it.
    let c = t.caret_at([vp.min[0] + 2.0 * ADV + 0.6 * ADV, vp.min[1] + 2.5 * LH]);
    assert_eq!(c, Caret { line: 2, col: 3 });
    let c = t.caret_at([vp.min[0] + 2.0 * ADV + 0.2 * ADV, vp.min[1] + 2.5 * LH]);
    assert_eq!(c, Caret { line: 2, col: 2 });
    // Off the bottom clamps into the document rather than returning nothing.
    let c = t.caret_at([vp.min[0], vp.min[1] + 1.0e6]);
    assert_eq!(c.line, 99);
    let c = t.caret_at([vp.min[0] - 1.0e6, vp.min[1] - 1.0e6]);
    assert_eq!(c, Caret { line: 0, col: 0 });
    u.finish();
}

// Selecting by dragging: press, move, release. The press has to land on the
// viewport leaf, so it uses coordinates inside it.

fn drag_select(u: &mut Ui, from: [f32; 2], to: [f32; 2], lines: u32, cols: u32) -> TextOut {
    text_frame(u, at(from[0], from[1], true), lines, cols);
    text_frame(u, at(to[0], to[1], true), lines, cols);
    text_frame(u, at(to[0], to[1], false), lines, cols)
}

#[test]
fn dragging_across_the_text_selects_from_the_press_to_the_pointer() {
    ui!(u);
    settled(&mut u, 100, 40);
    let o = drag_select(&mut u, [1.0, 5.0], [1.0 + 5.0 * ADV, 25.0], 100, 40);
    let (a, b) = o.selection.expect("a selection");
    assert_eq!(a, Caret { line: 0, col: 0 });
    assert_eq!(b, Caret { line: 2, col: 5 });
}

#[test]
fn a_backwards_drag_reports_the_selection_in_document_order() {
    ui!(u);
    settled(&mut u, 100, 40);
    let o = drag_select(&mut u, [1.0 + 5.0 * ADV, 25.0], [1.0, 5.0], 100, 40);
    let (a, b) = o.selection.expect("a selection");
    assert!(a < b, "{a:?} then {b:?}");
    assert_eq!(a, Caret { line: 0, col: 0 });
    assert_eq!(b, Caret { line: 2, col: 5 });
}

#[test]
fn a_selection_within_one_line_is_a_single_rect() {
    ui!(u);
    settled(&mut u, 100, 40);
    let o = drag_select(&mut u, [1.0, 5.0], [1.0 + 6.0 * ADV, 5.0], 100, 40);
    assert_eq!(o.sel_rects.as_slice().len(), 1);
    let r = o.sel_rects.as_slice()[0];
    assert!((r.max[1] - r.min[1] - LH).abs() < 0.01);
    assert!((r.max[0] - r.min[0] - 6.0 * ADV).abs() < 0.5, "{r:?}");
}

#[test]
fn a_multi_line_selection_is_head_block_and_tail() {
    ui!(u);
    settled(&mut u, 100, 40);
    // Lines 0 through 4, so there is a middle block.
    let o = drag_select(
        &mut u,
        [1.0 + 3.0 * ADV, 5.0],
        [1.0 + 2.0 * ADV, 45.0],
        100,
        40,
    );
    let rects = o.sel_rects.as_slice();
    assert_eq!(rects.len(), 3, "{rects:?}");
    // The block covers exactly the lines between, and nothing overlaps.
    assert!(
        (rects[1].max[1] - rects[1].min[1] - 3.0 * LH).abs() < 0.01,
        "{rects:?}"
    );
    assert!(rects[0].max[1] <= rects[1].min[1] + 0.01);
    assert!(rects[1].max[1] <= rects[2].min[1] + 0.01);
}

#[test]
fn selection_rects_never_leave_the_text_area() {
    ui!(u);
    settled(&mut u, 100, 200);
    let o = drag_select(&mut u, [1.0, 5.0], [1.0, 1.0e5], 100, 200);
    let text = Rect {
        min: o.viewport.min,
        max: o.viewport.max,
    };
    for r in o.sel_rects.as_slice() {
        assert!(r.min[0] >= text.min[0] - 0.01, "{r:?}");
        assert!(r.max[0] <= text.max[0] + 0.01, "{r:?}");
        assert!(r.min[1] >= text.min[1] - 0.01, "{r:?}");
        assert!(r.max[1] <= text.max[1] + 0.01, "{r:?}");
    }
}

#[test]
fn a_click_without_a_drag_leaves_an_empty_selection() {
    ui!(u);
    settled(&mut u, 100, 40);
    let o = drag_select(&mut u, [40.0, 45.0], [40.0, 45.0], 100, 40);
    assert!(o.selection.is_none(), "{:?}", o.selection);
    assert_eq!(o.caret, Caret { line: 4, col: 5 });
    assert_eq!(o.sel_rects.as_slice().len(), 0);
}

#[test]
fn clicking_the_text_takes_focus_so_the_arrow_keys_arrive() {
    ui!(u);
    settled(&mut u, 100, 40);
    text_frame(&mut u, at(40.0, 45.0, true), 100, 40);
    let after = text_frame(&mut u, at(40.0, 45.0, false), 100, 40);
    let _ = after;
    assert_eq!(u.focused(), Some(VKEY));
}

fn key_frame(u: &mut Ui, sym: u32, shift: bool, lines: u32, cols: u32) -> TextOut {
    let mut input = at(-1.0, -1.0, false);
    input.push_key(KeyEvent {
        sym,
        pressed: true,
        repeat: false,
        mods: Mods {
            shift,
            ..Mods::default()
        },
    });
    text_frame(u, input, lines, cols)
}

#[test]
fn arrow_keys_move_the_caret_once_the_view_has_focus() {
    ui!(u);
    settled(&mut u, 100, 40);
    text_frame(&mut u, at(40.0, 45.0, true), 100, 40);
    text_frame(&mut u, at(40.0, 45.0, false), 100, 40);

    let o = key_frame(&mut u, stein_ui::keys::DOWN, false, 100, 40);
    assert_eq!(o.caret, Caret { line: 5, col: 5 });
    let o = key_frame(&mut u, stein_ui::keys::RIGHT, false, 100, 40);
    assert_eq!(o.caret, Caret { line: 5, col: 6 });
    let o = key_frame(&mut u, stein_ui::keys::HOME, false, 100, 40);
    assert_eq!(o.caret, Caret { line: 5, col: 0 });
    let o = key_frame(&mut u, stein_ui::keys::END, false, 100, 40);
    assert_eq!(o.caret, Caret { line: 5, col: 40 });
    // An unshifted move collapses the selection onto the caret.
    assert!(o.selection.is_none(), "{:?}", o.selection);
}

#[test]
fn shift_arrow_extends_the_selection_instead_of_collapsing_it() {
    ui!(u);
    settled(&mut u, 100, 40);
    text_frame(&mut u, at(40.0, 45.0, true), 100, 40);
    text_frame(&mut u, at(40.0, 45.0, false), 100, 40);
    key_frame(&mut u, stein_ui::keys::DOWN, false, 100, 40);

    let o = key_frame(&mut u, stein_ui::keys::DOWN, true, 100, 40);
    let (a, b) = o.selection.expect("extended");
    assert_eq!(a, Caret { line: 5, col: 5 });
    assert_eq!(b, Caret { line: 6, col: 5 });
    let o = key_frame(&mut u, stein_ui::keys::DOWN, true, 100, 40);
    let (a, b) = o.selection.expect("extended further");
    assert_eq!(a, Caret { line: 5, col: 5 });
    assert_eq!(b, Caret { line: 7, col: 5 });
}

#[test]
fn the_caret_cannot_leave_the_document() {
    ui!(u);
    settled(&mut u, 3, 4);
    text_frame(&mut u, at(4.0, 5.0, true), 3, 4);
    text_frame(&mut u, at(4.0, 5.0, false), 3, 4);
    for _ in 0..10 {
        key_frame(&mut u, stein_ui::keys::UP, false, 3, 4);
        key_frame(&mut u, stein_ui::keys::LEFT, false, 3, 4);
    }
    let o = key_frame(&mut u, stein_ui::keys::LEFT, false, 3, 4);
    assert_eq!(o.caret, Caret { line: 0, col: 0 });
    for _ in 0..20 {
        key_frame(&mut u, stein_ui::keys::DOWN, false, 3, 4);
        key_frame(&mut u, stein_ui::keys::RIGHT, false, 3, 4);
    }
    let o = key_frame(&mut u, stein_ui::keys::RIGHT, false, 3, 4);
    assert_eq!(o.caret, Caret { line: 2, col: 4 });
}

#[test]
fn page_down_moves_by_a_viewport_and_drags_the_view_with_it() {
    ui!(u);
    settled(&mut u, 1_000, 40);
    text_frame(&mut u, at(4.0, 5.0, true), 1_000, 40);
    text_frame(&mut u, at(4.0, 5.0, false), 1_000, 40);
    let o = key_frame(&mut u, stein_ui::keys::PAGE_DOWN, false, 1_000, 40);
    // 40 columns fit, so no horizontal bar, and 200px of view at 10px a
    // line is 20 lines.
    assert_eq!(o.caret.line, 20);
    // And the caret stayed visible rather than running off the bottom.
    assert!(
        o.visible.first <= 20 && 20 < o.visible.end,
        "{:?}",
        o.visible
    );
}

#[test]
fn ctrl_a_selects_the_whole_document() {
    ui!(u);
    settled(&mut u, 50, 30);
    text_frame(&mut u, at(4.0, 5.0, true), 50, 30);
    text_frame(&mut u, at(4.0, 5.0, false), 50, 30);
    let mut input = at(-1.0, -1.0, false);
    input.push_key(KeyEvent {
        sym: stein_ui::keys::A,
        pressed: true,
        repeat: false,
        mods: Mods {
            ctrl: true,
            ..Mods::default()
        },
    });
    let o = text_frame(&mut u, input, 50, 30);
    let (a, b) = o.selection.expect("all of it");
    assert_eq!(a, Caret { line: 0, col: 0 });
    assert_eq!(b, Caret { line: 49, col: 30 });
}

#[test]
fn keys_are_ignored_while_something_else_has_focus() {
    ui!(u);
    settled(&mut u, 100, 40);
    u.focus(Some(0xdead));
    let o = key_frame(&mut u, stein_ui::keys::DOWN, false, 100, 40);
    assert_eq!(
        o.caret,
        Caret { line: 0, col: 0 },
        "keys leaked across focus"
    );
}

#[test]
fn scroll_to_line_takes_the_shortest_route_and_keeps_the_margin() {
    ui!(u);
    settled(&mut u, 1_000, 40);
    let idle = at(-1.0, -1.0, false);
    u.begin_frame(idle, [VW, VH]);
    let t = TextView::begin(
        &mut u,
        VKEY,
        [SizeSpec::px(VW), SizeSpec::px(VH)],
        1_000,
        40,
        metrics(),
        TextViewStyle::default(),
    )
    .expect("begin");
    t.scroll_to_line(&mut u, 500).expect("scroll");
    let t = t.end(&mut u).expect("end");
    let _ = t;
    u.end_frame().expect("balanced");
    u.finish();

    let o = text_frame(&mut u, idle, 1_000, 40);
    assert!(
        o.visible.first <= 500 && 500 < o.visible.end,
        "line 500 is not visible: {:?}",
        o.visible
    );
    // Scrolled down from the top, so line 500 sits near the bottom with the
    // margin below it rather than being recentred.
    assert!(o.visible.end >= 503, "no margin kept: {:?}", o.visible);
    assert!(
        o.visible.end <= 506,
        "recentred instead of revealed: {:?}",
        o.visible
    );
}

#[test]
fn set_caret_and_clear_selection_survive_to_the_next_frame() {
    ui!(u);
    settled(&mut u, 100, 40);
    let idle = at(-1.0, -1.0, false);
    u.begin_frame(idle, [VW, VH]);
    let t = TextView::begin(
        &mut u,
        VKEY,
        [SizeSpec::px(VW), SizeSpec::px(VH)],
        100,
        40,
        metrics(),
        TextViewStyle::default(),
    )
    .expect("begin");
    t.set_caret(&mut u, Caret { line: 42, col: 7 })
        .expect("set");
    let t = t.end(&mut u).expect("end");
    let _ = t;
    u.end_frame().expect("balanced");
    u.finish();

    let o = text_frame(&mut u, idle, 100, 40);
    assert_eq!(o.caret, Caret { line: 42, col: 7 });
    assert!(o.selection.is_none(), "a point caret is not a selection");
}

#[test]
fn a_view_over_nothing_reports_nothing_rather_than_a_bogus_range() {
    ui!(u);
    let o = settled(&mut u, 0, 0);
    assert!(o.visible.is_empty());
    assert_eq!(o.visible.len(), 0);
    assert_eq!(o.offset, [0.0, 0.0]);
    assert!(o.selection.is_none());
}

#[test]
fn text_views_refuse_bad_metrics_without_unbalancing_the_frame() {
    ui!(u);
    u.begin_frame(at(0.0, 0.0, false), [VW, VH]);
    let size = [SizeSpec::px(VW), SizeSpec::px(VH)];
    let style = TextViewStyle::default();

    let bad = TextMetrics {
        line_h: 0.0,
        advance: ADV,
    };
    let e = TextView::begin(&mut u, 1, size, 10, 10, bad, style);
    assert!(matches!(e, Err(WidgetError::TextMetricsNotPositive)));

    let bad = TextMetrics {
        line_h: LH,
        advance: -1.0,
    };
    let e = TextView::begin(&mut u, 2, size, 10, 10, bad, style);
    assert!(matches!(e, Err(WidgetError::TextMetricsNotPositive)));

    let bad_bar = TextViewStyle {
        bar: ScrollStyle {
            bar_size: 0.0,
            ..ScrollStyle::default()
        },
        ..style
    };
    let e = TextView::begin(&mut u, 3, size, 10, 10, metrics(), bad_bar);
    assert!(matches!(e, Err(WidgetError::BarSizeNotPositive)));

    u.end_frame().expect("a refused view emitted boxes");
    u.finish();
}

#[test]
fn a_wrapped_view_drops_the_horizontal_bar_and_gets_its_height_back() {
    ui!(u);
    let style = TextViewStyle {
        horizontal: false,
        ..TextViewStyle::default()
    };
    let idle = at(-1.0, -1.0, false);
    text_frame_styled(&mut u, idle, 100_000, 200, style);
    let o = text_frame_styled(&mut u, idle, 100_000, 200, style);
    // Outer, row, viewport, and one bar's three boxes.
    assert_eq!(o.boxes, 3 + 3, "boxes {}", o.boxes);
    // The full 200px is text again.
    assert_eq!(o.visible, LineRange { first: 0, end: 20 });
}

#[test]
fn a_wrapped_view_cannot_be_scrolled_sideways() {
    ui!(u);
    let style = TextViewStyle {
        horizontal: false,
        ..TextViewStyle::default()
    };
    let idle = at(-1.0, -1.0, false);
    text_frame_styled(&mut u, idle, 100, 500, style);
    text_frame_styled(&mut u, idle, 100, 500, style);
    u.state(VKEY).expect("room").scroll[0] = 9.0e9;
    let o = text_frame_styled(&mut u, idle, 100, 500, style);
    assert_eq!(o.offset[0], 0.0, "a wrapped view scrolled sideways");
    assert_eq!(o.first_col, 0);
}

// A tab bar narrower than its tabs. This is the normal case, not the edge
// case, so every tab has to stay reachable.

const TBKEY: u64 = 0x90;

struct ScrollTabsOut {
    active: Option<u64>,
    offset: f32,
    strip_w: f32,
    rects: [Rect; 6],
    count: usize,
}

fn scroll_tabs_frame(u: &mut Ui, input: Input, n: usize, bar_w: f32, tab_w: f32) -> ScrollTabsOut {
    u.begin_frame(input, [bar_w, 60.0]);
    let mut t = Tabs::begin(u, TBKEY, SizeSpec::px(bar_w), TabsStyle::default()).expect("begin");
    let mut idx = [0u32; 6];
    for (i, slot) in idx.iter_mut().enumerate().take(n.min(6)) {
        let r = t.item(u, i as u64, tab_w).expect("item");
        *slot = r.index;
    }
    let offset = t.offset();
    let strip_w = t.strip_width();
    let res = t.end(u).expect("end");
    u.end_frame().expect("balanced");
    let mut rects = [Rect::default(); 6];
    for i in 0..n.min(6) {
        rects[i] = u.rect_of(idx[i]).unwrap_or_default();
    }
    u.finish();
    ScrollTabsOut {
        active: res.active,
        offset,
        strip_w,
        rects,
        count: n.min(6),
    }
}

#[test]
fn a_bar_wide_enough_does_not_scroll() {
    ui!(u);
    scroll_tabs_frame(&mut u, at(-1.0, -1.0, false), 3, 400.0, 80.0);
    let o = scroll_tabs_frame(&mut u, at(-1.0, -1.0, false), 3, 400.0, 80.0);
    assert_eq!(o.offset, 0.0);
    assert!(o.strip_w <= 400.0, "strip {} in a 400px bar", o.strip_w);
}

#[test]
fn a_tab_past_the_edge_is_brought_into_view_when_selected() {
    ui!(u);
    // Six 90px tabs need well over 200px.
    let frame = |u: &mut Ui, i: Input| scroll_tabs_frame(u, i, 6, 200.0, 90.0);
    let idle = at(-1.0, -1.0, false);
    frame(&mut u, idle);
    let o = frame(&mut u, idle);
    assert!(o.strip_w > 200.0, "no overflow, the test is vacuous");
    assert_eq!(o.offset, 0.0, "nothing selected off screen yet");

    // Select the last tab directly, the way a keyboard shortcut would.
    u.begin_frame(idle, [200.0, 60.0]);
    let mut t =
        Tabs::begin(&mut u, TBKEY, SizeSpec::px(200.0), TabsStyle::default()).expect("begin");
    for i in 0..6u64 {
        t.item(&mut u, i, 90.0).expect("item");
    }
    t.end(&mut u).expect("end");
    u.end_frame().expect("balanced");
    u.finish();
    u.state(TBKEY).expect("room").user[0] = 5;
    u.state(TBKEY).expect("room").user[1] = 1;

    let o = frame(&mut u, idle);
    assert_eq!(o.active, Some(5));
    let o = frame(&mut u, idle);
    assert!(o.offset > 0.0, "the bar never scrolled to the selection");
    // And the selected tab is now inside the bar.
    let r = o.rects[5];
    assert!(
        r.min[0] >= -0.5 && r.max[0] <= 200.5,
        "tab 5 is still outside: {r:?}"
    );
}

#[test]
fn the_wheel_scrolls_the_bar_and_stops_at_both_ends() {
    ui!(u);
    let idle = at(-1.0, -1.0, false);
    scroll_tabs_frame(&mut u, idle, 6, 200.0, 90.0);
    scroll_tabs_frame(&mut u, idle, 6, 200.0, 90.0);

    let over = wheeled(100.0, 14.0, -1.0);
    for _ in 0..20 {
        scroll_tabs_frame(&mut u, over, 6, 200.0, 90.0);
    }
    // Read back at begin time, which is the clamped value the next frame uses.
    let o = scroll_tabs_frame(&mut u, idle, 6, 200.0, 90.0);
    assert!(o.offset > 0.0, "the wheel did not scroll the bar");
    assert!(
        o.offset <= o.strip_w - 200.0 + 0.5,
        "scrolled past the end: {} of {}",
        o.offset,
        o.strip_w
    );

    let back = wheeled(100.0, 14.0, 1.0);
    for _ in 0..20 {
        scroll_tabs_frame(&mut u, back, 6, 200.0, 90.0);
    }
    let o = scroll_tabs_frame(&mut u, idle, 6, 200.0, 90.0);
    assert_eq!(o.offset, 0.0, "scrolling back did not stop at zero");
}

#[test]
fn a_wheel_away_from_the_bar_leaves_it_alone() {
    ui!(u);
    let idle = at(-1.0, -1.0, false);
    scroll_tabs_frame(&mut u, idle, 6, 200.0, 90.0);
    scroll_tabs_frame(&mut u, idle, 6, 200.0, 90.0);
    for _ in 0..5 {
        scroll_tabs_frame(&mut u, wheeled(100.0, 200.0, -1.0), 6, 200.0, 90.0);
    }
    let o = scroll_tabs_frame(&mut u, idle, 6, 200.0, 90.0);
    assert_eq!(o.offset, 0.0);
}

#[test]
fn tabs_tile_the_strip_in_order_whatever_the_offset() {
    ui!(u);
    let idle = at(-1.0, -1.0, false);
    scroll_tabs_frame(&mut u, idle, 6, 200.0, 90.0);
    let o = scroll_tabs_frame(&mut u, idle, 6, 200.0, 90.0);
    for i in 1..o.count {
        assert!(
            o.rects[i].min[0] > o.rects[i - 1].min[0],
            "tab {i} is not after tab {}",
            i - 1
        );
    }
}

#[test]
fn scrolling_away_from_the_selected_tab_is_not_undone() {
    // Revealing on every frame would pin the bar to the selection and make
    // every other tab unreachable, which is the bug this guards.
    ui!(u);
    let idle = at(-1.0, -1.0, false);
    scroll_tabs_frame(&mut u, idle, 6, 200.0, 90.0);
    scroll_tabs_frame(&mut u, idle, 6, 200.0, 90.0);
    u.state(TBKEY).expect("room").user[0] = 0;
    u.state(TBKEY).expect("room").user[1] = 1;
    scroll_tabs_frame(&mut u, idle, 6, 200.0, 90.0);

    let over = wheeled(100.0, 14.0, -1.0);
    for _ in 0..4 {
        scroll_tabs_frame(&mut u, over, 6, 200.0, 90.0);
    }
    let o = scroll_tabs_frame(&mut u, idle, 6, 200.0, 90.0);
    assert_eq!(o.active, Some(0), "the selection moved on its own");
    assert!(
        o.offset > 0.0,
        "the bar snapped back to the selected tab, so nothing else is reachable"
    );
    // And it stays there.
    let o = scroll_tabs_frame(&mut u, idle, 6, 200.0, 90.0);
    assert!(o.offset > 0.0);
}

// Smooth scrolling: the wheel moves a target, the offset eases to it.

/// One frame of a 300 px area over 100 rows of 20 px with the default,
/// easing style. The offset, whether another frame is owed, and the
/// thumb's quad.
fn smooth_frame(u: &mut Ui, input: Input) -> (f32, bool, Option<stein_ui::draw::Quad>) {
    u.begin_frame(input, [400.0, 300.0]);
    let size = [SizeSpec::px(400.0), SizeSpec::px(300.0)];
    let sa = ScrollArea::begin(u, SKEY, Axis::Y, size, ScrollStyle::default()).expect("begin");
    let vl = VirtualList::begin(u, LKEY, sa.offset(), sa.view_len(), 100, 20.0).expect("vlist");
    for i in vl.range().first..vl.range().end {
        vl.row(u, 1_000 + u64::from(i), 0xff33_3344).expect("row");
    }
    vl.end(u).expect("vlist end");
    let sa = sa.end(u).expect("scroll end");
    u.end_frame().expect("closed");
    let thumb = u.geometry_of(sa.thumb_index()).map(|g| g.0);
    let quad = u.quads().iter().find(|q| Some(q.rect) == thumb).copied();
    let owed = u.finish().needs_present;
    (sa.offset(), owed, quad)
}

#[test]
fn a_wheel_notch_eases_to_one_step_and_then_lets_the_window_sleep() {
    ui!(u);
    let idle = at(200.0, 150.0, false);
    smooth_frame(&mut u, idle);
    smooth_frame(&mut u, idle);
    let step = ScrollStyle::default().wheel_step;
    let (first, owed, _) = smooth_frame(&mut u, wheeled(200.0, 150.0, -1.0));
    assert!(first > 0.0 && first < step, "the first frame moves part way, {first}");
    assert!(owed, "a frame is owed while it moves");
    let mut last = first;
    let mut frames = 0;
    loop {
        let (offset, owed, _) = smooth_frame(&mut u, idle);
        assert!(offset >= last, "eases one way, {offset} after {last}");
        last = offset;
        frames += 1;
        if !owed {
            break;
        }
        assert!(frames < 120, "never landed");
    }
    assert_eq!(last, step, "lands exactly one notch down");
    assert!(frames < 40, "took {frames} frames");
}

#[test]
fn notches_in_quick_succession_add_up() {
    ui!(u);
    let idle = at(200.0, 150.0, false);
    smooth_frame(&mut u, idle);
    smooth_frame(&mut u, idle);
    for _ in 0..3 {
        smooth_frame(&mut u, wheeled(200.0, 150.0, -1.0));
    }
    let mut last = 0.0;
    for _ in 0..120 {
        last = smooth_frame(&mut u, idle).0;
    }
    assert_eq!(last, 3.0 * ScrollStyle::default().wheel_step);
}

#[test]
fn an_offset_written_from_outside_is_jumped_to_not_eased_away() {
    ui!(u);
    let idle = at(200.0, 150.0, false);
    smooth_frame(&mut u, idle);
    smooth_frame(&mut u, idle);
    u.state(SKEY).expect("room").scroll[1] = 500.0;
    let (offset, _, _) = smooth_frame(&mut u, idle);
    assert_eq!(offset, 500.0);
    let (offset, owed, _) = smooth_frame(&mut u, idle);
    assert_eq!(offset, 500.0, "stays where it was put");
    assert!(!owed);
}

#[test]
fn the_bar_is_rounded() {
    ui!(u);
    let idle = at(-1.0, -1.0, false);
    smooth_frame(&mut u, idle);
    let (_, _, thumb) = smooth_frame(&mut u, idle);
    let thumb = thumb.expect("the thumb painted");
    assert_eq!(thumb.shape, stein_ui::draw::Shape::PILL);
}

// Auto hide: a bar only takes room and paints while there is something to scroll.

/// One frame of a 400x300 area over `rows` rows of 20 px. The viewport and
/// track rects and how many quads the bar painted.
fn hide_frame(u: &mut Ui, rows: u32, style: ScrollStyle) -> (Option<Rect>, Option<Rect>, usize) {
    u.begin_frame(at(-1.0, -1.0, false), [400.0, 300.0]);
    let size = [SizeSpec::px(400.0), SizeSpec::px(300.0)];
    let sa = ScrollArea::begin(u, SKEY, Axis::Y, size, style).expect("begin");
    let vl = VirtualList::begin(u, LKEY, sa.offset(), sa.view_len(), rows, 20.0).expect("vlist");
    for i in vl.range().first..vl.range().end {
        vl.row(u, 1_000 + u64::from(i), 0xff33_3344).expect("row");
    }
    vl.end(u).expect("vlist end");
    let sa = sa.end(u).expect("scroll end");
    u.end_frame().expect("closed");
    let view = u.geometry_of(sa.viewport_index()).map(|g| g.0);
    let track = u.geometry_of(sa.track_index()).map(|g| g.0);
    let thumb = u.geometry_of(sa.thumb_index()).map(|g| g.0);
    let bar_quads = u
        .quads()
        .iter()
        .filter(|q| Some(q.rect) == track || Some(q.rect) == thumb)
        .count();
    u.finish();
    (view, track, bar_quads)
}

fn width(r: Option<Rect>) -> f32 {
    r.map_or(-1.0, |r| r.max[0] - r.min[0])
}

#[test]
fn content_that_fits_hides_the_bar_and_gives_its_width_back() {
    ui!(u);
    for _ in 0..3 {
        hide_frame(&mut u, 5, instant());
    }
    let (view, track, quads) = hide_frame(&mut u, 5, instant());
    assert_eq!(width(view), 400.0);
    assert_eq!(width(track), 0.0);
    assert_eq!(quads, 0, "a hidden bar paints nothing");
}

#[test]
fn content_that_overflows_shows_the_bar() {
    ui!(u);
    for _ in 0..3 {
        hide_frame(&mut u, 100, instant());
    }
    let bar = instant().bar_size;
    let (view, track, quads) = hide_frame(&mut u, 100, instant());
    assert_eq!(width(view), 400.0 - bar);
    assert_eq!(width(track), bar);
    assert_eq!(quads, 2, "track and thumb");
}

#[test]
fn growing_content_brings_the_bar_back_and_shrinking_hides_it() {
    ui!(u);
    let rows = [5, 5, 5, 100, 100, 100, 5, 5, 5];
    let shown = rows.map(|n| width(hide_frame(&mut u, n, instant()).1) > 0.0);
    // Content length is read from the previous solve, so each change lands a frame late.
    assert_eq!(shown, [false, false, false, false, true, true, true, false, false]);
}

#[test]
fn a_bar_that_may_not_hide_stays_with_nothing_to_scroll() {
    ui!(u);
    let style = ScrollStyle {
        auto_hide: false,
        ..instant()
    };
    for _ in 0..3 {
        hide_frame(&mut u, 5, style);
    }
    let (_, track, quads) = hide_frame(&mut u, 5, style);
    assert_eq!(width(track), style.bar_size);
    assert_eq!(quads, 2);
}
