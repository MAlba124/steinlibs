//! Headless dock tests over stack declared storage.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::vec;
use std::vec::Vec;

use stein_layout::{Axis, BoxSpec, Insets, LayoutStorage, Options, Rect, SizeSpec};
use stein_ui::draw::{CustomRegion, Quad};
use stein_ui::key::{child, hash_str, seeded};
use stein_ui::{Input, Ui, UiError, UiStorage, WidgetState};

use crate::{DockError, DockStorage, DockStyle, DockTree, DropZone, NodeKind, preview_rect};

const N: usize = 256;
const S: usize = 64;
const Q: usize = 512;
const DT: f32 = 1.0 / 60.0;

macro_rules! dock {
    ($t:ident, $n:expr, $p:expr) => {
        let mut nkind = [NodeKind::Free; $n];
        let mut naxis = [Axis::X; $n];
        let mut nratio = [0.0f32; $n];
        let mut na = [0u32; $n];
        let mut nb = [0u32; $n];
        let mut nparent = [0u32; $n];
        let mut nactive = [0u32; $n];
        let mut nrect = [Rect::default(); $n];
        let mut nfree = [0u32; $n];
        let mut pid = [0u32; $p];
        let mut pnode = [0u32; $p];
        let mut porder = [0u16; $p];
        let mut pfree = [0u32; $p];
        let storage = DockStorage {
            node_kind: &mut nkind,
            node_axis: &mut naxis,
            node_ratio: &mut nratio,
            node_a: &mut na,
            node_b: &mut nb,
            node_parent: &mut nparent,
            node_active: &mut nactive,
            node_rect: &mut nrect,
            node_free: &mut nfree,
            panel_id: &mut pid,
            panel_node: &mut pnode,
            panel_order: &mut porder,
            panel_free: &mut pfree,
        };
        #[allow(unused_mut)]
        let mut $t = DockTree::new(storage).expect("storage agrees");
    };
}

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

fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
    Rect {
        min: [x0, y0],
        max: [x1, y1],
    }
}

fn panels(t: &DockTree, leaf: u32) -> Vec<u32> {
    t.panels_of(leaf).collect()
}

fn leaf(t: &DockTree, panel: u32) -> u32 {
    t.leaf_of(panel).expect("panel docked")
}

/// Every structural invariant at once, called after every mutation in the
/// heavier tests.
fn check(t: &DockTree) {
    let mut reachable: Vec<u32> = Vec::new();
    if let Some(root) = t.root() {
        assert_eq!(t.parent_of(root), None, "root has a parent");
        walk(t, root, &mut reachable);
    }
    assert_eq!(reachable.len(), t.node_count(), "orphan nodes exist");
    let splits = reachable.iter().filter(|n| t.is_split(**n)).count();
    let leaves = reachable.iter().filter(|n| t.is_leaf(**n)).count();
    if !reachable.is_empty() {
        assert_eq!(leaves, splits + 1, "tree is not structurally minimal");
    }
    let mut total = 0;
    for l in t.leaves() {
        assert!(reachable.contains(&l), "live leaf {l} unreachable");
        total += t.panels_of(l).count();
    }
    assert_eq!(total, t.panel_count(), "panel table disagrees with leaves");
}

fn walk(t: &DockTree, root: u32, reachable: &mut Vec<u32>) {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        assert!(!reachable.contains(&n), "node {n} reached twice");
        reachable.push(n);
        check_node(t, n, &mut stack);
    }
}

fn check_node(t: &DockTree, n: u32, stack: &mut Vec<u32>) {
    if t.is_split(n) {
        let (a, b) = t.children_of(n).expect("split children");
        assert_eq!(t.parent_of(a), Some(n), "child {a} parent wrong");
        assert_eq!(t.parent_of(b), Some(n), "child {b} parent wrong");
        stack.push(a);
        stack.push(b);
        return;
    }
    assert!(t.is_leaf(n), "reachable node {n} is neither kind");
    let ordered = panels(t, n);
    assert!(!ordered.is_empty(), "reachable empty leaf {n}");
    let raw = usize::from(t.leaf_len(n));
    assert_eq!(ordered.len(), raw, "orders of leaf {n} not dense");
    let active = t.active_panel(n).expect("leaf has an active panel");
    assert!(ordered.contains(&active), "active not in leaf {n}");
}

// Construction and basic ops.

#[test]
fn mismatched_storage_is_refused() {
    let mut nkind = [NodeKind::Free; 4];
    let mut naxis = [Axis::X; 4];
    let mut nratio = [0.0f32; 3];
    let mut na = [0u32; 4];
    let mut nb = [0u32; 4];
    let mut nparent = [0u32; 4];
    let mut nactive = [0u32; 4];
    let mut nrect = [Rect::default(); 4];
    let mut nfree = [0u32; 4];
    let mut pid = [0u32; 4];
    let mut pnode = [0u32; 4];
    let mut porder = [0u16; 4];
    let mut pfree = [0u32; 4];
    let bad = DockStorage {
        node_kind: &mut nkind,
        node_axis: &mut naxis,
        node_ratio: &mut nratio,
        node_a: &mut na,
        node_b: &mut nb,
        node_parent: &mut nparent,
        node_active: &mut nactive,
        node_rect: &mut nrect,
        node_free: &mut nfree,
        panel_id: &mut pid,
        panel_node: &mut pnode,
        panel_order: &mut porder,
        panel_free: &mut pfree,
    };
    assert!(matches!(
        DockTree::new(bad),
        Err(DockError::StorageMismatch)
    ));
}

#[test]
fn insert_root_builds_a_single_leaf() {
    dock!(t, 8, 8);
    t.insert_root(10).expect("room");
    let root = t.root().expect("root exists");
    assert!(t.is_leaf(root));
    assert_eq!(panels(&t, root), [10]);
    assert_eq!(t.active_panel(root), Some(10));
    assert_eq!(t.node_count(), 1);
    assert_eq!(t.panel_count(), 1);
    check(&t);
}

#[test]
fn insert_root_joins_the_root_tab_group() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    t.insert_root(2).expect("room");
    let root = t.root().expect("root");
    assert_eq!(panels(&t, root), [1, 2]);
    assert_eq!(t.active_panel(root), Some(2), "the new tab is selected");
    assert_eq!(t.node_count(), 1);
    check(&t);
}

#[test]
fn insert_root_refuses_a_docked_panel_and_a_split_root() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    assert_eq!(t.insert_root(1), Err(DockError::PanelDocked));
    t.dock(2, leaf(&t, 1), DropZone::Right).expect("dock");
    assert_eq!(t.insert_root(3), Err(DockError::NotALeaf));
    check(&t);
}

#[test]
fn docking_each_side_splits_with_the_right_axis_and_order() {
    let cases = [
        (DropZone::Left, Axis::X, true),
        (DropZone::Right, Axis::X, false),
        (DropZone::Top, Axis::Y, true),
        (DropZone::Bottom, Axis::Y, false),
    ];
    for (zone, axis, leads) in cases {
        dock!(t, 8, 8);
        t.insert_root(1).expect("room");
        let l1 = leaf(&t, 1);
        t.dock(2, l1, zone).expect("dock");
        let root = t.root().expect("root");
        assert!(t.is_split(root), "{zone:?} did not split");
        assert_eq!(t.split_axis(root), Some(axis));
        assert_eq!(t.ratio_of(root), Some(0.5));
        let (a, b) = t.children_of(root).expect("children");
        let l2 = leaf(&t, 2);
        if leads {
            assert_eq!((a, b), (l2, l1), "{zone:?} puts the new panel first");
        } else {
            assert_eq!((a, b), (l1, l2), "{zone:?} puts the new panel last");
        }
        assert_eq!(t.parent_of(l1), Some(root));
        assert_eq!(t.parent_of(l2), Some(root));
        check(&t);
    }
}

#[test]
fn dock_center_joins_and_activates() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    t.dock(2, leaf(&t, 1), DropZone::Center).expect("dock");
    let l = leaf(&t, 1);
    assert_eq!(panels(&t, l), [1, 2]);
    assert_eq!(t.active_panel(l), Some(2));
    assert_eq!(t.node_count(), 1);
    check(&t);
}

#[test]
fn dock_refuses_bad_targets_and_docked_panels() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    t.dock(2, leaf(&t, 1), DropZone::Right).expect("dock");
    let split = t.root().expect("root");
    assert_eq!(
        t.dock(1, leaf(&t, 2), DropZone::Center),
        Err(DockError::PanelDocked)
    );
    assert_eq!(t.dock(3, split, DropZone::Center), Err(DockError::NotALeaf));
    assert_eq!(t.dock(3, 99, DropZone::Center), Err(DockError::NoSuchNode));
    check(&t);
}

#[test]
fn dock_to_root_wraps_the_whole_workspace() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    t.dock(2, leaf(&t, 1), DropZone::Right).expect("dock");
    let old_root = t.root().expect("root");
    t.dock_to_root(3, DropZone::Top).expect("dock to root");
    let root = t.root().expect("root");
    assert_ne!(root, old_root);
    assert_eq!(t.split_axis(root), Some(Axis::Y));
    let (a, b) = t.children_of(root).expect("children");
    assert_eq!(a, leaf(&t, 3), "top drop leads");
    assert_eq!(b, old_root);
    assert_eq!(t.parent_of(old_root), Some(root));
    check(&t);
}

#[test]
fn dock_to_root_center_joins_a_leaf_root_and_refuses_a_split() {
    dock!(t, 8, 8);
    t.dock_to_root(1, DropZone::Center)
        .expect("empty tree seeds");
    t.dock_to_root(2, DropZone::Center)
        .expect("leaf root joins");
    assert_eq!(panels(&t, t.root().expect("root")), [1, 2]);
    t.dock_to_root(3, DropZone::Right).expect("split");
    assert_eq!(
        t.dock_to_root(4, DropZone::Center),
        Err(DockError::NotALeaf)
    );
    check(&t);
}

// Removal and collapse.

#[test]
fn removing_the_active_tab_moves_the_selection_sensibly() {
    dock!(t, 8, 8);
    for p in [1, 2, 3] {
        t.insert_root(p).expect("room");
    }
    let l = leaf(&t, 1);
    t.remove(3).expect("remove last, active");
    assert_eq!(panels(&t, l), [1, 2]);
    assert_eq!(t.active_panel(l), Some(2), "fell back to the new last tab");
    check(&t);
}

#[test]
fn removing_a_middle_active_tab_selects_its_successor() {
    dock!(t, 8, 8);
    for p in [1, 2, 3] {
        t.insert_root(p).expect("room");
    }
    t.activate(2).expect("activate");
    let l = leaf(&t, 1);
    t.remove(2).expect("remove middle");
    assert_eq!(panels(&t, l), [1, 3], "orders compacted");
    assert_eq!(t.active_panel(l), Some(3));
    check(&t);
}

#[test]
fn removing_an_inactive_tab_keeps_the_selection() {
    dock!(t, 8, 8);
    for p in [1, 2, 3] {
        t.insert_root(p).expect("room");
    }
    let l = leaf(&t, 1);
    t.remove(1).expect("remove inactive");
    assert_eq!(panels(&t, l), [2, 3]);
    assert_eq!(t.active_panel(l), Some(3));
    check(&t);
}

#[test]
fn emptying_a_leaf_collapses_it_and_its_parent_split() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    let l1 = leaf(&t, 1);
    t.dock(2, l1, DropZone::Right).expect("dock");
    assert_eq!(t.node_count(), 3);
    t.remove(2).expect("remove");
    assert_eq!(t.root(), Some(l1), "sibling promoted to root");
    assert_eq!(t.parent_of(l1), None);
    assert_eq!(t.node_count(), 1);
    check(&t);
}

#[test]
fn collapse_promotes_a_whole_sibling_subtree() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    t.dock(2, leaf(&t, 1), DropZone::Right).expect("dock");
    t.dock(3, leaf(&t, 2), DropZone::Bottom).expect("dock");
    let inner = t.parent_of(leaf(&t, 2)).expect("inner split");
    t.remove(1).expect("remove");
    assert_eq!(t.root(), Some(inner), "the subtree took the root");
    assert_eq!(t.parent_of(inner), None);
    assert_eq!(t.node_count(), 3);
    check(&t);
}

#[test]
fn removing_the_last_panel_empties_the_tree() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    t.remove(1).expect("remove");
    assert_eq!(t.root(), None);
    assert_eq!(t.node_count(), 0);
    assert_eq!(t.panel_count(), 0);
    check(&t);
    t.insert_root(2)
        .expect("an emptied tree accepts a new root");
    check(&t);
}

#[test]
fn removing_a_missing_panel_is_refused() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    assert_eq!(t.remove(9), Err(DockError::NoSuchPanel));
    check(&t);
}

fn build_five(t: &mut DockTree) {
    t.insert_root(1).expect("room");
    t.dock(2, leaf(t, 1), DropZone::Right).expect("dock 2");
    t.dock(3, leaf(t, 2), DropZone::Bottom).expect("dock 3");
    t.dock(4, leaf(t, 1), DropZone::Center).expect("dock 4");
    t.dock(5, leaf(t, 1), DropZone::Top).expect("dock 5");
}

fn perms(v: &mut Vec<u32>, k: usize, out: &mut Vec<Vec<u32>>) {
    if k <= 1 {
        out.push(v.clone());
        return;
    }
    for i in 0..k {
        perms(v, k - 1, out);
        let from = if k.is_multiple_of(2) { i } else { 0 };
        v.swap(from, k - 1);
    }
}

#[test]
fn every_removal_order_leaves_a_minimal_tree() {
    let mut ids = vec![1u32, 2, 3, 4, 5];
    let mut orders = Vec::new();
    perms(&mut ids, 5, &mut orders);
    assert_eq!(orders.len(), 120, "generator is vacuous");
    for order in &orders {
        dock!(t, 16, 8);
        build_five(&mut t);
        check(&t);
        for p in order {
            t.remove(*p).expect("remove");
            check(&t);
        }
        assert_eq!(t.node_count(), 0, "order {order:?} left nodes");
        assert_eq!(t.root(), None);
    }
}

// move_panel.

#[test]
fn moving_a_lone_panel_onto_its_own_leaf_is_a_noop() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    t.dock(2, leaf(&t, 1), DropZone::Right).expect("dock");
    let l2 = leaf(&t, 2);
    for zone in [
        DropZone::Center,
        DropZone::Left,
        DropZone::Right,
        DropZone::Top,
        DropZone::Bottom,
    ] {
        t.move_panel(2, l2, zone).expect("noop");
        assert_eq!(t.node_count(), 3, "{zone:?} mutated the tree");
        assert_eq!(leaf(&t, 2), l2, "{zone:?} moved the panel");
        check(&t);
    }
}

#[test]
fn moving_a_panel_between_leaves_collapses_the_source() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    t.dock(2, leaf(&t, 1), DropZone::Right).expect("dock");
    let l1 = leaf(&t, 1);
    t.move_panel(2, l1, DropZone::Center).expect("move");
    assert_eq!(t.root(), Some(l1), "source leaf and split collapsed");
    assert_eq!(panels(&t, l1), [1, 2]);
    assert_eq!(t.active_panel(l1), Some(2));
    assert_eq!(t.node_count(), 1);
    check(&t);
}

#[test]
fn moving_within_a_leaf_reorders_to_the_end() {
    dock!(t, 8, 8);
    for p in [1, 2, 3] {
        t.insert_root(p).expect("room");
    }
    let l = leaf(&t, 1);
    t.move_panel(1, l, DropZone::Center).expect("move");
    assert_eq!(panels(&t, l), [2, 3, 1]);
    assert_eq!(t.active_panel(l), Some(1));
    check(&t);
}

#[test]
fn moving_to_a_side_splits_the_target() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    t.insert_root(2).expect("room");
    t.dock(3, leaf(&t, 1), DropZone::Right).expect("dock");
    let l3 = leaf(&t, 3);
    t.move_panel(1, l3, DropZone::Bottom).expect("move");
    let l1 = leaf(&t, 1);
    let p = t.parent_of(l1).expect("split above 1");
    assert_eq!(t.split_axis(p), Some(Axis::Y));
    let (a, b) = t.children_of(p).expect("children");
    assert_eq!((a, b), (l3, l1));
    check(&t);
}

#[test]
fn a_side_move_without_node_room_refuses_before_removing() {
    dock!(t, 4, 8);
    t.insert_root(1).expect("room");
    t.insert_root(2).expect("room");
    t.dock(3, leaf(&t, 1), DropZone::Right).expect("dock");
    // 3 nodes live, 1 free: splitting needs 2 and the source will not collapse.
    let l3 = leaf(&t, 3);
    assert_eq!(
        t.move_panel(1, l3, DropZone::Bottom),
        Err(DockError::NodesFull)
    );
    assert_eq!(leaf(&t, 1), leaf(&t, 2), "the panel must not be lost");
    check(&t);
}

#[test]
fn a_side_move_that_frees_its_own_split_succeeds_at_capacity() {
    dock!(t, 3, 8);
    t.insert_root(1).expect("room");
    t.move_panel(1, leaf(&t, 1), DropZone::Center)
        .expect("noop");
    t.insert_root(2).expect("room");
    t.move_panel(2, leaf(&t, 1), DropZone::Right)
        .expect("split at cap");
    assert_eq!(t.node_count(), 3);
    // Zero free nodes, but moving 2 collapses its own split first.
    let l1 = leaf(&t, 1);
    t.move_panel(2, l1, DropZone::Bottom)
        .expect("reuses freed nodes");
    assert_eq!(t.node_count(), 3);
    check(&t);
}

// activate and set_ratio.

#[test]
fn activate_switches_within_the_group() {
    dock!(t, 8, 8);
    for p in [1, 2, 3] {
        t.insert_root(p).expect("room");
    }
    t.activate(1).expect("activate");
    assert_eq!(t.active_panel(leaf(&t, 1)), Some(1));
    assert_eq!(t.activate(9), Err(DockError::NoSuchPanel));
}

#[test]
fn set_ratio_validates_and_clamps() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    t.dock(2, leaf(&t, 1), DropZone::Right).expect("dock");
    let split = t.root().expect("root");
    assert_eq!(t.set_ratio(leaf(&t, 1), 0.5), Err(DockError::NotASplit));
    assert_eq!(
        t.set_ratio(split, f32::NAN),
        Err(DockError::RatioOutOfRange)
    );
    t.set_ratio(split, 0.0).expect("clamps");
    assert_eq!(t.ratio_of(split), Some(crate::RATIO_MIN));
    t.set_ratio(split, 0.7).expect("in range");
    assert_eq!(t.ratio_of(split), Some(0.7));
}

// Geometry.

#[test]
fn a_lone_leaf_takes_the_root_rect_and_its_content_sits_below_the_tab_bar() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    let root = rect(0.0, 0.0, 800.0, 600.0);
    t.solve(root, 6.0, 26.0);
    let l = leaf(&t, 1);
    assert_eq!(t.rect_of(l), Some(root));
    assert_eq!(t.content_rect_of(l), Some(rect(0.0, 26.0, 800.0, 600.0)));
    assert_eq!(t.rect_of(99), None);
}

#[test]
fn split_children_share_the_region_exactly_at_the_ratio() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    t.dock(2, leaf(&t, 1), DropZone::Right).expect("dock");
    let split = t.root().expect("root");
    t.set_ratio(split, 0.25).expect("ratio");
    t.solve(rect(0.0, 0.0, 800.0, 600.0), 6.0, 26.0);
    assert_eq!(t.rect_of(leaf(&t, 1)), Some(rect(0.0, 0.0, 200.0, 600.0)));
    assert_eq!(t.rect_of(leaf(&t, 2)), Some(rect(200.0, 0.0, 800.0, 600.0)));
    let d: Vec<_> = t.dividers().collect();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].split, split);
    assert_eq!(d[0].axis, Axis::X);
    assert_eq!(
        d[0].grab,
        rect(197.0, 0.0, 203.0, 600.0),
        "straddles the cut"
    );
    assert_eq!(d[0].region, rect(0.0, 0.0, 800.0, 600.0));
}

#[test]
fn nested_splits_tile_the_root_with_no_gaps() {
    dock!(t, 16, 8);
    build_five(&mut t);
    let splits: Vec<u32> = (0..t.node_capacity() as u32)
        .filter(|n| t.is_split(*n))
        .collect();
    for (i, s) in splits.iter().enumerate() {
        t.set_ratio(*s, 0.2 + 0.15 * i as f32).expect("ratio");
    }
    let root = rect(0.0, 0.0, 800.0, 600.0);
    t.solve(root, 6.0, 26.0);
    let mut area = 0.0f32;
    for (l, r) in t.regions() {
        assert!(t.is_leaf(l));
        assert!(r.min[0] >= -0.01 && r.max[0] <= 800.01, "leaf {l} escapes");
        assert!(r.min[1] >= -0.01 && r.max[1] <= 600.01, "leaf {l} escapes");
        area += r.width() * r.height();
    }
    assert!(
        (area - 800.0 * 600.0).abs() < 1.0,
        "leaves cover {area}, want 480000"
    );
    let dividers = t.dividers().count();
    assert_eq!(dividers, splits.len());
    assert_eq!(t.regions().count(), t.leaf_count());
}

#[test]
fn drop_zones_hit_the_centre_the_edges_and_the_corners() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    // Power-of-two extent so the corner ties are exact in f32 rather than
    // decided by rounding noise, and no tab bar, so the leaf is all content.
    t.solve(rect(0.0, 0.0, 128.0, 128.0), 6.0, 0.0);
    let l = leaf(&t, 1);
    let zone = |x, y| t.drop_zone_at([x, y]);
    assert_eq!(zone(64.0, 64.0), Some((l, DropZone::Center)));
    assert_eq!(zone(40.0, 80.0), Some((l, DropZone::Center)), "centre band");
    assert_eq!(zone(8.0, 64.0), Some((l, DropZone::Left)));
    assert_eq!(zone(120.0, 64.0), Some((l, DropZone::Right)));
    assert_eq!(zone(64.0, 8.0), Some((l, DropZone::Top)));
    assert_eq!(zone(64.0, 120.0), Some((l, DropZone::Bottom)));
    // Corners tie two edges exactly; the horizontal edge wins by rule.
    assert_eq!(zone(8.0, 8.0), Some((l, DropZone::Left)));
    assert_eq!(zone(120.0, 8.0), Some((l, DropZone::Right)));
    assert_eq!(zone(8.0, 120.0), Some((l, DropZone::Left)));
    assert_eq!(zone(120.0, 120.0), Some((l, DropZone::Right)));
    // Off the tie, the nearer edge wins over the horizontal preference.
    assert_eq!(zone(16.0, 8.0), Some((l, DropZone::Top)));
    assert_eq!(zone(200.0, 64.0), None);
}

#[test]
fn the_tab_bar_joins_and_edges_start_below_it() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    t.solve(rect(0.0, 0.0, 128.0, 154.0), 6.0, 26.0);
    let l = leaf(&t, 1);
    let zone = |x, y| t.drop_zone_at([x, y]);
    assert_eq!(
        zone(4.0, 4.0),
        Some((l, DropZone::Center)),
        "left end of the bar"
    );
    assert_eq!(
        zone(124.0, 25.0),
        Some((l, DropZone::Center)),
        "right end of the bar"
    );
    assert_eq!(
        zone(64.0, 30.0),
        Some((l, DropZone::Top)),
        "just below the bar"
    );
    assert_eq!(
        zone(64.0, 90.0),
        Some((l, DropZone::Center)),
        "middle of the content"
    );
    assert_eq!(zone(64.0, 150.0), Some((l, DropZone::Bottom)));
}

#[test]
fn drop_zones_resolve_to_the_right_leaf_in_a_split() {
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    t.dock(2, leaf(&t, 1), DropZone::Right).expect("dock");
    t.solve(rect(0.0, 0.0, 800.0, 600.0), 6.0, 26.0);
    assert_eq!(
        t.drop_zone_at([200.0, 300.0]),
        Some((leaf(&t, 1), DropZone::Center))
    );
    assert_eq!(
        t.drop_zone_at([600.0, 300.0]),
        Some((leaf(&t, 2), DropZone::Center))
    );
}

#[test]
fn preview_rects_cover_the_would_be_region() {
    let r = rect(0.0, 0.0, 100.0, 60.0);
    assert_eq!(preview_rect(r, DropZone::Center), r);
    assert_eq!(preview_rect(r, DropZone::Left), rect(0.0, 0.0, 50.0, 60.0));
    assert_eq!(
        preview_rect(r, DropZone::Right),
        rect(50.0, 0.0, 100.0, 60.0)
    );
    assert_eq!(preview_rect(r, DropZone::Top), rect(0.0, 0.0, 100.0, 30.0));
    assert_eq!(
        preview_rect(r, DropZone::Bottom),
        rect(0.0, 30.0, 100.0, 60.0)
    );
}

// Capacity refusal.

#[test]
fn node_capacity_refuses_cleanly_and_the_tree_stays_consistent() {
    dock!(t, 3, 8);
    t.insert_root(1).expect("room");
    t.dock(2, leaf(&t, 1), DropZone::Right)
        .expect("uses all 3 nodes");
    assert_eq!(
        t.dock(3, leaf(&t, 1), DropZone::Right),
        Err(DockError::NodesFull)
    );
    assert_eq!(t.node_count(), 3);
    assert_eq!(t.panel_count(), 2, "no panel leaked from the refusal");
    check(&t);
    t.dock(3, leaf(&t, 1), DropZone::Center)
        .expect("tabs still fit");
    check(&t);
}

#[test]
fn panel_capacity_refuses_cleanly_and_the_tree_stays_consistent() {
    dock!(t, 8, 2);
    t.insert_root(1).expect("room");
    t.insert_root(2).expect("room");
    assert_eq!(t.insert_root(3), Err(DockError::PanelsFull));
    let l = leaf(&t, 1);
    assert_eq!(t.dock(3, l, DropZone::Center), Err(DockError::PanelsFull));
    assert_eq!(t.dock(3, l, DropZone::Right), Err(DockError::PanelsFull));
    assert_eq!(
        t.node_count(),
        1,
        "the side dock must not leave stray nodes"
    );
    assert_eq!(t.panel_count(), 2);
    check(&t);
}

// Persistence.

fn header(has_root: u8) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"SDCK");
    v.extend_from_slice(&1u16.to_le_bytes());
    v.push(has_root);
    v
}

fn leaf_bytes(ids: &[u32], active: u16) -> Vec<u8> {
    let mut v = vec![0u8];
    v.extend_from_slice(&(ids.len() as u16).to_le_bytes());
    for id in ids {
        v.extend_from_slice(&id.to_le_bytes());
    }
    v.extend_from_slice(&active.to_le_bytes());
    v
}

fn split_bytes(axis: u8, ratio: f32, a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut v = vec![1u8, axis];
    v.extend_from_slice(&ratio.to_le_bytes());
    v.extend_from_slice(a);
    v.extend_from_slice(b);
    v
}

fn serialize(t: &DockTree) -> Vec<u8> {
    let mut buf = [0u8; 512];
    let n = t.to_bytes(&mut buf).expect("512 is plenty");
    buf[..n].to_vec()
}

#[test]
fn a_built_tree_round_trips_bit_exact() {
    dock!(t, 16, 8);
    build_five(&mut t);
    let splits: Vec<u32> = (0..t.node_capacity() as u32)
        .filter(|n| t.is_split(*n))
        .collect();
    t.set_ratio(splits[0], 0.3125).expect("ratio");
    t.activate(1).expect("activate");
    let bytes = serialize(&t);
    dock!(t2, 16, 8);
    t2.from_bytes(&bytes).expect("restore");
    check(&t2);
    assert_eq!(serialize(&t2), bytes, "round trip must be exact");
    assert_eq!(t2.panel_count(), 5);
    assert_eq!(t2.node_count(), t.node_count());
    let l1 = t2.leaf_of(1).expect("panel 1 restored");
    assert_eq!(t2.active_panel(l1), Some(1), "active tab survives");
}

#[test]
fn an_empty_tree_round_trips_and_clears_the_target() {
    dock!(t, 8, 8);
    let bytes = serialize(&t);
    dock!(t2, 8, 8);
    t2.insert_root(9).expect("room");
    t2.from_bytes(&bytes).expect("restore empty");
    assert_eq!(t2.root(), None);
    assert_eq!(t2.panel_count(), 0);
    assert_eq!(serialize(&t2), bytes);
}

#[test]
fn truncated_buffers_are_refused_and_leave_the_tree_alone() {
    dock!(t, 16, 8);
    build_five(&mut t);
    let bytes = serialize(&t);
    dock!(t2, 16, 8);
    t2.insert_root(9).expect("room");
    let before = serialize(&t2);
    for cut in 0..bytes.len() {
        assert!(
            t2.from_bytes(&bytes[..cut]).is_err(),
            "prefix of {cut} bytes was accepted"
        );
        assert_eq!(
            serialize(&t2),
            before,
            "a refusal at {cut} mutated the tree"
        );
    }
    t2.from_bytes(&bytes).expect("the full buffer is fine");
    check(&t2);
}

#[test]
fn corrupt_buffers_are_refused() {
    dock!(t, 16, 16);
    let ok = leaf_bytes(&[7], 0);
    let mut good = header(1);
    good.extend_from_slice(&ok);
    t.from_bytes(&good).expect("baseline is valid");

    let mut bad_magic = good.clone();
    bad_magic[0] = b'X';
    assert_eq!(t.from_bytes(&bad_magic), Err(DockError::BadMagic));

    let mut bad_version = good.clone();
    bad_version[4] = 9;
    assert_eq!(t.from_bytes(&bad_version), Err(DockError::BadVersion));

    let mut bad_root_flag = header(7);
    bad_root_flag.extend_from_slice(&ok);
    assert_eq!(t.from_bytes(&bad_root_flag), Err(DockError::Corrupt));

    let mut bad_tag = header(1);
    bad_tag.push(9);
    assert_eq!(t.from_bytes(&bad_tag), Err(DockError::Corrupt));

    let mut dup_across = header(1);
    dup_across.extend_from_slice(&split_bytes(
        0,
        0.5,
        &leaf_bytes(&[7], 0),
        &leaf_bytes(&[7], 0),
    ));
    assert_eq!(t.from_bytes(&dup_across), Err(DockError::Corrupt));

    let mut dup_within = header(1);
    dup_within.extend_from_slice(&leaf_bytes(&[7, 7], 0));
    assert_eq!(t.from_bytes(&dup_within), Err(DockError::Corrupt));

    let mut bad_active = header(1);
    bad_active.extend_from_slice(&leaf_bytes(&[1, 2], 2));
    assert_eq!(t.from_bytes(&bad_active), Err(DockError::Corrupt));

    let mut bad_ratio = header(1);
    bad_ratio.extend_from_slice(&split_bytes(
        0,
        f32::NAN,
        &leaf_bytes(&[1], 0),
        &leaf_bytes(&[2], 0),
    ));
    assert_eq!(t.from_bytes(&bad_ratio), Err(DockError::Corrupt));

    let mut bad_axis = header(1);
    bad_axis.extend_from_slice(&split_bytes(
        2,
        0.5,
        &leaf_bytes(&[1], 0),
        &leaf_bytes(&[2], 0),
    ));
    assert_eq!(t.from_bytes(&bad_axis), Err(DockError::Corrupt));

    let mut empty_leaf = header(1);
    empty_leaf.extend_from_slice(&leaf_bytes(&[], 0));
    assert_eq!(t.from_bytes(&empty_leaf), Err(DockError::Corrupt));

    let mut trailing = good.clone();
    trailing.push(0);
    assert_eq!(t.from_bytes(&trailing), Err(DockError::Corrupt));

    assert_eq!(t.from_bytes(&[]), Err(DockError::Truncated));

    // Every refusal left the last good restore intact.
    assert_eq!(serialize(&t), good);
    check(&t);
}

#[test]
fn restores_that_exceed_capacity_are_refused_before_mutation() {
    dock!(big, 16, 8);
    build_five(&mut big);
    let bytes = serialize(&big);
    dock!(tiny, 2, 8);
    tiny.insert_root(9).expect("room");
    let before = serialize(&tiny);
    assert_eq!(tiny.from_bytes(&bytes), Err(DockError::NodesFull));
    assert_eq!(serialize(&tiny), before);

    dock!(narrow, 16, 2);
    assert_eq!(narrow.from_bytes(&bytes), Err(DockError::PanelsFull));
    assert_eq!(narrow.root(), None);
}

#[test]
fn to_bytes_refuses_a_short_buffer_and_fits_an_exact_one() {
    dock!(t, 16, 8);
    build_five(&mut t);
    let bytes = serialize(&t);
    let mut short = vec![0u8; bytes.len() - 1];
    assert_eq!(t.to_bytes(&mut short), Err(DockError::OutputTooSmall));
    let mut exact = vec![0u8; bytes.len()];
    assert_eq!(t.to_bytes(&mut exact), Ok(bytes.len()));
    assert_eq!(exact, bytes);
}

// Deterministic pseudo-random sequences.

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const ZONES: [DropZone; 5] = [
    DropZone::Center,
    DropZone::Left,
    DropZone::Right,
    DropZone::Top,
    DropZone::Bottom,
];

/// One random operation. Returns whether it applied. Capacity refusals and
/// structural no-ops are the only tolerated errors.
fn random_op(t: &mut DockTree, rng: &mut Rng, next_id: &mut u32) -> bool {
    let leaves: Vec<u32> = t.leaves().collect();
    let ids: Vec<u32> = leaves.iter().flat_map(|l| t.panels_of(*l)).collect();
    let splits: Vec<u32> = (0..t.node_capacity() as u32)
        .filter(|n| t.is_split(*n))
        .collect();
    if leaves.is_empty() {
        let r = t.insert_root(*next_id);
        *next_id += 1;
        return allowed(r);
    }
    match rng.pick(8) {
        0..=2 => {
            let target = leaves[rng.pick(leaves.len())];
            let r = t.dock(*next_id, target, ZONES[rng.pick(5)]);
            *next_id += 1;
            allowed(r)
        }
        3 => {
            let r = t.dock_to_root(*next_id, ZONES[rng.pick(5)]);
            *next_id += 1;
            allowed(r)
        }
        4 => allowed(t.remove(ids[rng.pick(ids.len())])),
        5 => {
            let p = ids[rng.pick(ids.len())];
            let target = leaves[rng.pick(leaves.len())];
            allowed(t.move_panel(p, target, ZONES[rng.pick(5)]))
        }
        6 => allowed(t.activate(ids[rng.pick(ids.len())])),
        _ => {
            if splits.is_empty() {
                return false;
            }
            let s = splits[rng.pick(splits.len())];
            let ratio = (rng.next() % 1000) as f32 / 1000.0;
            allowed(t.set_ratio(s, ratio))
        }
    }
}

fn allowed(r: Result<(), DockError>) -> bool {
    match r {
        Ok(()) => true,
        Err(DockError::NodesFull | DockError::PanelsFull | DockError::NotALeaf) => false,
        Err(e) => panic!("unexpected error {e:?}"),
    }
}

#[test]
fn random_operation_sequences_hold_every_invariant() {
    let mut applied = 0usize;
    let mut nodes_seen = 0usize;
    let mut peak = 0usize;
    for seed in 1..=40u64 {
        dock!(t, 32, 16);
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut next_id = 1u32;
        for _ in 0..60 {
            applied += usize::from(random_op(&mut t, &mut rng, &mut next_id));
            check(&t);
            nodes_seen += t.node_count();
            peak = peak.max(t.node_count());
        }
        t.solve(rect(0.0, 0.0, 800.0, 600.0), 6.0, 26.0);
        let mut area = 0.0f32;
        for (_, r) in t.regions() {
            area += r.width() * r.height();
        }
        if t.root().is_some() {
            assert!(
                (area - 800.0 * 600.0).abs() < 2.0,
                "seed {seed}: leaves cover {area}"
            );
        }
    }
    // Vacuity guard: a broken generator must fail loudly, not pass trivially.
    assert!(applied > 400, "only {applied} ops applied");
    assert!(nodes_seen > 2000, "trees stayed trivial: {nodes_seen}");
    assert!(peak >= 9, "no tree ever grew past {peak} nodes");
}

#[test]
fn a_randomly_built_tree_round_trips_bit_exact() {
    dock!(t, 32, 16);
    let mut rng = Rng(0xC0FF_EE00_DEAD_BEEF);
    let mut next_id = 1u32;
    for _ in 0..200 {
        random_op(&mut t, &mut rng, &mut next_id);
    }
    check(&t);
    assert!(t.panel_count() >= 3, "generator is vacuous");
    let bytes = serialize(&t);
    dock!(t2, 32, 16);
    t2.from_bytes(&bytes).expect("restore");
    check(&t2);
    assert_eq!(serialize(&t2), bytes);
}

// Emitting into a Ui.

const DKEY: u64 = 0xD0C4;

fn dstyle() -> DockStyle {
    DockStyle {
        tab_w: 80.0,
        ..DockStyle::default()
    }
}

fn dock_frame(u: &mut Ui, t: &mut DockTree, input: Input) -> (crate::DockFrame, usize) {
    u.begin_frame(input, [800.0, 600.0]);
    let f = t
        .emit(
            u,
            DKEY,
            [SizeSpec::px(800.0), SizeSpec::px(600.0)],
            dstyle(),
        )
        .expect("emit");
    u.end_frame().expect("balanced");
    let boxes = u.box_count();
    u.finish();
    (f, boxes)
}

/// Split with leaf [1, 2] on the left and leaf [3] on the right.
fn two_leaf_tree(t: &mut DockTree) {
    t.insert_root(1).expect("room");
    t.insert_root(2).expect("room");
    t.dock(3, leaf(t, 1), DropZone::Right).expect("dock");
}

#[test]
fn emit_stays_balanced_and_the_box_count_matches_the_tree() {
    ui!(u);
    dock!(t, 16, 8);
    two_leaf_tree(&mut t);
    let idle = at(-1.0, -1.0, false);
    dock_frame(&mut u, &mut t, idle);
    let (f, boxes) = dock_frame(&mut u, &mut t, idle);
    // ws + split + divider + 2 leaves x (leaf + bar + content) + 3 tabs.
    assert_eq!(boxes, 1 + 1 + 1 + 2 * 3 + 3, "boxes {boxes}");
    assert_eq!(f.panels().len(), 2);
    assert_eq!(f.tabs().len(), 3);
    assert!(f.dragging.is_none());
    assert!(f.hover.is_none());
}

#[test]
fn emit_of_an_empty_tree_is_one_box_and_no_panels() {
    ui!(u);
    dock!(t, 8, 8);
    let (f, boxes) = dock_frame(&mut u, &mut t, at(-1.0, -1.0, false));
    assert_eq!(boxes, 1);
    assert!(f.panels().is_empty());
    assert!(f.tabs().is_empty());
}

#[test]
fn emit_reports_content_rects_that_match_the_solved_layout() {
    ui!(u);
    dock!(t, 16, 8);
    two_leaf_tree(&mut t);
    let idle = at(-1.0, -1.0, false);
    dock_frame(&mut u, &mut t, idle);
    u.begin_frame(idle, [800.0, 600.0]);
    let f = t
        .emit(
            &mut u,
            DKEY,
            [SizeSpec::px(800.0), SizeSpec::px(600.0)],
            dstyle(),
        )
        .expect("emit");
    u.end_frame().expect("balanced");
    for p in f.panels() {
        let rect = t.content_rect_of(p.leaf).expect("leaf");
        assert_eq!(p.rect, rect);
        let (solved, _) = u.geometry_of(p.box_index).expect("solved");
        assert!(
            (solved.min[0] - rect.min[0]).abs() < 0.01
                && (solved.min[1] - rect.min[1]).abs() < 0.01
                && (solved.max[0] - rect.max[0]).abs() < 0.01
                && (solved.max[1] - rect.max[1]).abs() < 0.01,
            "panel {} solved {solved:?} want {rect:?}",
            p.panel
        );
    }
    u.finish();
}

#[test]
fn emit_refuses_a_bad_style_without_unbalancing_the_frame() {
    ui!(u);
    dock!(t, 8, 8);
    t.insert_root(1).expect("room");
    u.begin_frame(at(-1.0, -1.0, false), [800.0, 600.0]);
    let size = [SizeSpec::px(800.0), SizeSpec::px(600.0)];
    let bad_div = DockStyle {
        divider: 0.0,
        ..dstyle()
    };
    assert!(matches!(
        t.emit(&mut u, DKEY, size, bad_div),
        Err(DockError::DividerNotPositive)
    ));
    let bad_tab = DockStyle {
        tab_h: 0.0,
        ..dstyle()
    };
    assert!(matches!(
        t.emit(&mut u, DKEY, size, bad_tab),
        Err(DockError::TabHeightNotPositive)
    ));
    u.end_frame().expect("a refused emit left boxes behind");
    u.finish();
}

#[test]
fn dragging_a_divider_changes_the_ratio_by_the_expected_amount() {
    ui!(u);
    dock!(t, 16, 8);
    two_leaf_tree(&mut t);
    let split = t.root().expect("root");
    let idle = at(-1.0, -1.0, false);
    dock_frame(&mut u, &mut t, idle);
    dock_frame(&mut u, &mut t, idle);
    // Divider straddles x = 400. Press there, then pull 80px right.
    dock_frame(&mut u, &mut t, at(400.0, 300.0, true));
    assert_eq!(
        t.ratio_of(split),
        Some(0.5),
        "a press alone must not move it"
    );
    dock_frame(&mut u, &mut t, at(480.0, 300.0, true));
    let r = t.ratio_of(split).expect("split");
    assert!((r - 0.6).abs() < 1.0e-4, "80px over 800 is 0.1, got {r}");
    dock_frame(&mut u, &mut t, at(480.0, 300.0, false));
    dock_frame(&mut u, &mut t, idle);
    let r = t.ratio_of(split).expect("split");
    assert!((r - 0.6).abs() < 1.0e-4, "the ratio drifted to {r}");
}

#[test]
fn clicking_a_tab_activates_its_panel() {
    ui!(u);
    dock!(t, 16, 8);
    two_leaf_tree(&mut t);
    let l = leaf(&t, 1);
    assert_eq!(t.active_panel(l), Some(2), "setup: last insert is active");
    let idle = at(-1.0, -1.0, false);
    dock_frame(&mut u, &mut t, idle);
    let (f, _) = dock_frame(&mut u, &mut t, idle);
    let tab1 = f.tabs().iter().find(|x| x.panel == 1).expect("tab 1");
    assert_eq!(tab1.rect, rect(0.0, 0.0, 80.0, 26.0));
    // Press and release inside tab 1 without crossing the drag threshold.
    dock_frame(&mut u, &mut t, at(40.0, 13.0, true));
    dock_frame(&mut u, &mut t, at(40.0, 13.0, false));
    assert_eq!(t.active_panel(l), Some(1), "the click did not activate");
    check(&t);
}

#[test]
fn a_tab_drag_past_the_threshold_reports_hover_and_previews_the_region() {
    ui!(u);
    dock!(t, 16, 8);
    two_leaf_tree(&mut t);
    let idle = at(-1.0, -1.0, false);
    dock_frame(&mut u, &mut t, idle);
    let (_, boxes_idle) = dock_frame(&mut u, &mut t, idle);
    dock_frame(&mut u, &mut t, at(40.0, 13.0, true));
    let (f, boxes_drag) = dock_frame(&mut u, &mut t, at(600.0, 300.0, true));
    assert_eq!(f.dragging, Some(1));
    assert_eq!(f.hover, Some((leaf(&t, 3), DropZone::Center)));
    assert_eq!(boxes_drag, boxes_idle + 1, "the preview quad is missing");
    // Nothing moved yet: the drop applies on release.
    assert_eq!(leaf(&t, 1), leaf(&t, 2));
}

#[test]
fn releasing_a_tab_drag_applies_the_move() {
    ui!(u);
    dock!(t, 16, 8);
    two_leaf_tree(&mut t);
    let l3 = leaf(&t, 3);
    let idle = at(-1.0, -1.0, false);
    dock_frame(&mut u, &mut t, idle);
    dock_frame(&mut u, &mut t, idle);
    dock_frame(&mut u, &mut t, at(40.0, 13.0, true));
    dock_frame(&mut u, &mut t, at(600.0, 300.0, true));
    let (f, _) = dock_frame(&mut u, &mut t, at(600.0, 300.0, false));
    assert!(f.dragging.is_none(), "the drag must end on release");
    assert_eq!(leaf(&t, 1), l3, "the panel did not land in the target");
    assert_eq!(panels(&t, l3), [3, 1]);
    assert_eq!(t.active_panel(l3), Some(1));
    check(&t);
}

#[test]
fn releasing_a_tab_drag_outside_any_region_cancels() {
    ui!(u);
    dock!(t, 16, 8);
    two_leaf_tree(&mut t);
    let l1 = leaf(&t, 1);
    let idle = at(-1.0, -1.0, false);
    dock_frame(&mut u, &mut t, idle);
    dock_frame(&mut u, &mut t, idle);
    dock_frame(&mut u, &mut t, at(40.0, 13.0, true));
    let (f, _) = dock_frame(&mut u, &mut t, at(900.0, 300.0, true));
    assert_eq!(f.dragging, Some(1));
    assert!(f.hover.is_none(), "outside the workspace there is no zone");
    dock_frame(&mut u, &mut t, at(900.0, 300.0, false));
    assert_eq!(leaf(&t, 1), l1, "a cancelled drag must not move anything");
    let (f, _) = dock_frame(&mut u, &mut t, idle);
    assert!(f.dragging.is_none());
    check(&t);
}

#[test]
fn a_side_drop_from_a_drag_splits_the_target_leaf() {
    ui!(u);
    dock!(t, 16, 8);
    two_leaf_tree(&mut t);
    let l3 = leaf(&t, 3);
    let idle = at(-1.0, -1.0, false);
    dock_frame(&mut u, &mut t, idle);
    dock_frame(&mut u, &mut t, idle);
    dock_frame(&mut u, &mut t, at(40.0, 13.0, true));
    // Right leaf spans 400..800; y past 70% of 600 is its Bottom zone.
    dock_frame(&mut u, &mut t, at(600.0, 550.0, true));
    let (f, _) = dock_frame(&mut u, &mut t, at(600.0, 550.0, false));
    assert!(f.dragging.is_none());
    let l1 = leaf(&t, 1);
    let p = t.parent_of(l1).expect("split above the dropped panel");
    assert_eq!(t.split_axis(p), Some(Axis::Y));
    assert_eq!(t.children_of(p), Some((l3, l1)));
    check(&t);
}

// Tab bar scrolling.

fn wheel_at(x: f32, y: f32, w: f32) -> Input {
    Input {
        pointer: [x, y],
        pointer_down: false,
        // `w` in notches.
        wheel: [0.0, w * stein_ui::WHEEL_NOTCH],
        dt: DT,
        ..Input::default()
    }
}

fn tab_frame_rect(f: &crate::DockFrame, pid: u32) -> Rect {
    f.tabs()
        .iter()
        .find(|x| x.panel == pid)
        .expect("tab reported")
        .rect
}

fn tab_x(f: &crate::DockFrame, pid: u32) -> f32 {
    tab_frame_rect(f, pid).min[0]
}

/// One 800px leaf whose 12 tabs need 960px, so the bar must scroll.
fn twelve_tab_tree(t: &mut DockTree) {
    for p in 1..=12 {
        t.insert_root(p).expect("room");
    }
}

/// Wheel forward until `pid`'s tab lies fully inside the 800px bar, and
/// return its on-screen rect from the frame that shows it.
fn wheel_until_inside(u: &mut Ui, t: &mut DockTree, pid: u32) -> Rect {
    for _ in 0..40 {
        let (f, _) = dock_frame(u, t, at(400.0, 13.0, false));
        let r = tab_frame_rect(&f, pid);
        if r.min[0] >= -0.01 && r.max[0] <= 800.01 {
            return r;
        }
        dock_frame(u, t, wheel_at(400.0, 13.0, -0.5));
    }
    panic!("tab {pid} never came into view");
}

#[test]
fn every_overflowing_tab_is_reachable_by_wheel_and_click() {
    ui!(u);
    dock!(t, 8, 16);
    twelve_tab_tree(&mut t);
    let root = leaf(&t, 1);
    let idle = at(-1.0, -1.0, false);
    dock_frame(&mut u, &mut t, idle);
    let (f, _) = dock_frame(&mut u, &mut t, idle);
    assert!(tab_x(&f, 1) < -0.01, "no overflow scroll, test is vacuous");
    for pid in 1..=12u32 {
        // Back to the left edge first, so each tab is hunted from scratch.
        dock_frame(&mut u, &mut t, wheel_at(400.0, 13.0, 10.0));
        let r = wheel_until_inside(&mut u, &mut t, pid);
        let cx = (r.min[0] + r.max[0]) * 0.5;
        dock_frame(&mut u, &mut t, at(cx, 13.0, true));
        dock_frame(&mut u, &mut t, at(cx, 13.0, false));
        assert_eq!(
            t.active_panel(root),
            Some(pid),
            "tab {pid} was visible but the click missed"
        );
    }
    check(&t);
}

#[test]
fn wheeling_past_either_end_clamps() {
    ui!(u);
    dock!(t, 8, 16);
    twelve_tab_tree(&mut t);
    let idle = at(-1.0, -1.0, false);
    dock_frame(&mut u, &mut t, idle);
    // Settling reveals the active last tab, which needs the full 160.
    let (f, _) = dock_frame(&mut u, &mut t, idle);
    assert!((tab_x(&f, 1) + 160.0).abs() < 0.01, "reveal missed the end");
    assert_eq!(tab_frame_rect(&f, 12), rect(720.0, 0.0, 800.0, 26.0));
    let (f, _) = dock_frame(&mut u, &mut t, wheel_at(400.0, 13.0, -10.0));
    assert!((tab_x(&f, 1) + 160.0).abs() < 0.01, "wheeled past the end");
    let (f, _) = dock_frame(&mut u, &mut t, wheel_at(400.0, 13.0, 10.0));
    assert!(tab_x(&f, 1).abs() < 0.01, "wheeled past the start");
    let (f, _) = dock_frame(&mut u, &mut t, wheel_at(400.0, 13.0, 10.0));
    assert!(tab_x(&f, 1).abs() < 0.01, "the start clamp did not hold");
}

#[test]
fn a_reveal_fires_once_and_the_wheel_is_not_undone() {
    ui!(u);
    dock!(t, 8, 16);
    twelve_tab_tree(&mut t);
    let idle = at(-1.0, -1.0, false);
    dock_frame(&mut u, &mut t, idle);
    dock_frame(&mut u, &mut t, idle);
    t.activate(1).expect("activate");
    let (f, _) = dock_frame(&mut u, &mut t, idle);
    assert!(
        tab_x(&f, 1).abs() < 0.01,
        "activating tab 1 must scroll home"
    );
    t.activate(12).expect("activate");
    let (f, _) = dock_frame(&mut u, &mut t, idle);
    assert_eq!(
        tab_frame_rect(&f, 12),
        rect(720.0, 0.0, 800.0, 26.0),
        "activating an off screen tab must bring it in"
    );
    let (f, _) = dock_frame(&mut u, &mut t, wheel_at(400.0, 13.0, 1.0));
    assert!(
        (tab_x(&f, 1) + 100.0).abs() < 0.01,
        "the wheel did not move"
    );
    let (f, _) = dock_frame(&mut u, &mut t, idle);
    assert!(
        (tab_x(&f, 1) + 100.0).abs() < 0.01,
        "the reveal re-fired and pinned the bar to the selection"
    );
    let (f, _) = dock_frame(&mut u, &mut t, idle);
    assert!((tab_x(&f, 1) + 100.0).abs() < 0.01);
}

/// Left leaf with tabs 1..=6 and right leaf with 7..=12, bars 400px each.
fn two_overflowing_leaves(t: &mut DockTree) {
    for p in 1..=6 {
        t.insert_root(p).expect("room");
    }
    t.dock(7, leaf(t, 1), DropZone::Right).expect("dock");
    for p in 8..=12 {
        t.dock(p, leaf(t, 7), DropZone::Center).expect("dock");
    }
}

#[test]
fn two_overflowing_bars_scroll_independently() {
    ui!(u);
    dock!(t, 8, 16);
    two_overflowing_leaves(&mut t);
    let idle = at(-1.0, -1.0, false);
    dock_frame(&mut u, &mut t, idle);
    // Each bar reveals its own active last tab: 480px of tabs in 400px.
    let (f, _) = dock_frame(&mut u, &mut t, idle);
    assert!((tab_x(&f, 1) + 80.0).abs() < 0.01, "left bar not revealed");
    assert!(
        (tab_x(&f, 7) - 320.0).abs() < 0.01,
        "right bar not revealed"
    );
    // Wheel over the left bar only.
    let (f, _) = dock_frame(&mut u, &mut t, wheel_at(200.0, 13.0, 10.0));
    assert!(tab_x(&f, 1).abs() < 0.01, "left bar did not scroll");
    assert!((tab_x(&f, 7) - 320.0).abs() < 0.01, "right bar moved too");
    // Now the right bar only.
    let (f, _) = dock_frame(&mut u, &mut t, wheel_at(600.0, 13.0, 10.0));
    assert!(
        (tab_x(&f, 7) - 400.0).abs() < 0.01,
        "right bar did not scroll"
    );
    assert!(tab_x(&f, 1).abs() < 0.01, "left bar moved too");
}

#[test]
fn a_bar_wide_enough_for_its_tabs_never_scrolls() {
    ui!(u);
    dock!(t, 16, 8);
    two_leaf_tree(&mut t);
    let idle = at(-1.0, -1.0, false);
    dock_frame(&mut u, &mut t, idle);
    dock_frame(&mut u, &mut t, idle);
    // Two 80px tabs in a 400px bar.
    let (f, _) = dock_frame(&mut u, &mut t, wheel_at(200.0, 13.0, -10.0));
    assert!(tab_x(&f, 1).abs() < 0.01, "a fitting bar scrolled");
    let (f, _) = dock_frame(&mut u, &mut t, wheel_at(200.0, 13.0, 10.0));
    assert!(tab_x(&f, 1).abs() < 0.01);
}

#[test]
fn a_drag_still_starts_from_a_scrolled_tab() {
    ui!(u);
    dock!(t, 8, 16);
    two_overflowing_leaves(&mut t);
    let right = leaf(&t, 7);
    let idle = at(-1.0, -1.0, false);
    dock_frame(&mut u, &mut t, idle);
    let (f, _) = dock_frame(&mut u, &mut t, idle);
    // Left bar sits at offset 80, so tab 6 shows at 320..400.
    assert_eq!(tab_frame_rect(&f, 6), rect(320.0, 0.0, 400.0, 26.0));
    dock_frame(&mut u, &mut t, at(360.0, 13.0, true));
    let (f, _) = dock_frame(&mut u, &mut t, at(600.0, 300.0, true));
    assert_eq!(f.dragging, Some(6), "the drag missed the scrolled tab");
    assert_eq!(f.hover, Some((right, DropZone::Center)));
    dock_frame(&mut u, &mut t, at(600.0, 300.0, false));
    assert_eq!(leaf(&t, 6), right, "the drop was not applied");
    assert_eq!(t.active_panel(right), Some(6));
    check(&t);
}

// emit_with: caller drawn panel content.

fn dock_frame_with<E, F>(
    u: &mut Ui,
    t: &mut DockTree,
    input: Input,
    f: F,
) -> (crate::DockFrame, usize)
where
    F: FnMut(&mut Ui, u32) -> Result<(), E>,
{
    u.begin_frame(input, [800.0, 600.0]);
    let frame = t
        .emit_with(
            u,
            DKEY,
            [SizeSpec::px(800.0), SizeSpec::px(600.0)],
            dstyle(),
            f,
        )
        .expect("emit_with");
    u.end_frame().expect("balanced");
    let boxes = u.box_count();
    u.finish();
    (frame, boxes)
}

/// Baseline box count for `two_leaf_tree` after the geometry has settled.
fn settled_base(u: &mut Ui, t: &mut DockTree) -> usize {
    let idle = at(-1.0, -1.0, false);
    dock_frame(u, t, idle);
    let (_, base) = dock_frame(u, t, idle);
    base
}

#[test]
fn emit_with_counts_the_callback_boxes_and_stays_balanced() {
    ui!(u);
    dock!(t, 16, 8);
    two_leaf_tree(&mut t);
    let base = settled_base(&mut u, &mut t);
    let content = |u: &mut Ui, pid: u32| {
        for i in 0..3u64 {
            u.leaf(
                seeded(0xCB, hash_str("row"), u64::from(pid) * 8 + i),
                BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::px(20.0)),
                0xff22_2230,
            )?;
        }
        Ok::<(), UiError>(())
    };
    let (f, boxes) = dock_frame_with(&mut u, &mut t, at(-1.0, -1.0, false), content);
    assert_eq!(boxes, base + 2 * 3, "two visible panels, three boxes each");
    assert_eq!(f.panel_errors, 0);
    assert_eq!(f.panels().len(), 2);
}

#[test]
fn emit_with_counts_a_failing_callback_and_still_calls_the_rest() {
    ui!(u);
    dock!(t, 16, 8);
    two_leaf_tree(&mut t);
    let base = settled_base(&mut u, &mut t);
    let mut calls: Vec<u32> = Vec::new();
    let (f, boxes) = dock_frame_with(&mut u, &mut t, at(-1.0, -1.0, false), |_u, pid| {
        calls.push(pid);
        if pid == 2 { Err(()) } else { Ok(()) }
    });
    assert_eq!(calls, [2, 3], "the error must not skip the second panel");
    assert_eq!(f.panel_errors, 1);
    assert_eq!(boxes, base, "a failing callback emitted nothing");
}

#[test]
fn emit_with_survives_a_callback_that_leaves_a_box_open() {
    ui!(u);
    dock!(t, 16, 8);
    two_leaf_tree(&mut t);
    let base = settled_base(&mut u, &mut t);
    let mut calls: Vec<u32> = Vec::new();
    let (f, boxes) = dock_frame_with(&mut u, &mut t, at(-1.0, -1.0, false), |u, pid| {
        calls.push(pid);
        if pid == 2 {
            u.open(
                child(0xCB, hash_str("stray")),
                BoxSpec::container(Axis::X),
                0,
            )?;
        }
        Ok::<(), UiError>(())
    });
    assert_eq!(calls, [2, 3], "the second panel must still be visited");
    assert_eq!(f.panel_errors, 1);
    assert_eq!(boxes, base + 1, "the stray box was emitted, then closed");
    let (f, boxes) = dock_frame(&mut u, &mut t, at(-1.0, -1.0, false));
    assert_eq!(boxes, base, "the next frame is clean");
    assert_eq!(f.panel_errors, 0);
}

#[test]
fn emit_with_survives_a_callback_that_closes_too_many_times() {
    ui!(u);
    dock!(t, 16, 8);
    two_leaf_tree(&mut t);
    let base = settled_base(&mut u, &mut t);
    let mut calls: Vec<u32> = Vec::new();
    let (f, boxes) = dock_frame_with(&mut u, &mut t, at(-1.0, -1.0, false), |u, pid| {
        calls.push(pid);
        if pid == 2 {
            let _ = u.close();
            let _ = u.close();
        }
        Ok::<(), UiError>(())
    });
    assert_eq!(calls, [2, 3], "the second panel must still be visited");
    assert_eq!(f.panel_errors, 1);
    // Content and leaf container were closed early; two ghosts rebalance.
    assert_eq!(boxes, base + 2);
    let (f, boxes) = dock_frame(&mut u, &mut t, at(-1.0, -1.0, false));
    assert_eq!(boxes, base, "the next frame is clean");
    assert_eq!(f.panel_errors, 0);
}

#[test]
fn emit_with_visits_each_visible_panel_exactly_once() {
    ui!(u);
    dock!(t, 16, 8);
    two_leaf_tree(&mut t);
    settled_base(&mut u, &mut t);
    let mut calls: Vec<u32> = Vec::new();
    dock_frame_with(&mut u, &mut t, at(-1.0, -1.0, false), |_u, pid| {
        calls.push(pid);
        Ok::<(), ()>(())
    });
    // Panel 1 is an inactive tab of the left leaf and must never be called.
    assert_eq!(calls, [2, 3], "active tabs only, one call each");
}

#[test]
fn emit_and_emit_with_noop_agree_exactly() {
    let idle = at(-1.0, -1.0, false);
    let (pa, ta, ba, ea) = {
        ui!(u);
        dock!(t, 16, 8);
        two_leaf_tree(&mut t);
        dock_frame(&mut u, &mut t, idle);
        let (f, b) = dock_frame(&mut u, &mut t, idle);
        (f.panels().to_vec(), f.tabs().to_vec(), b, f.panel_errors)
    };
    let (pb, tb, bb, eb) = {
        ui!(u);
        dock!(t, 16, 8);
        two_leaf_tree(&mut t);
        dock_frame_with(&mut u, &mut t, idle, |_, _| Ok::<(), ()>(()));
        let (f, b) = dock_frame_with(&mut u, &mut t, idle, |_, _| Ok::<(), ()>(()));
        (f.panels().to_vec(), f.tabs().to_vec(), b, f.panel_errors)
    };
    assert_eq!(ba, bb, "box counts must match");
    assert_eq!(pa, pb, "panel reports must match");
    assert_eq!(ta, tb, "tab reports must match");
    assert_eq!(ea, 0);
    assert_eq!(eb, 0);
}

#[test]
fn tabs_shrink_to_fit_down_to_the_floor() {
    use crate::emit::fit_tab_w;
    let style = DockStyle {
        tab_w: 180.0,
        tab_min_w: 90.0,
        bar_reserve: 20.0,
        ..DockStyle::default()
    };
    assert_eq!(fit_tab_w(1000.0, 3, &style), 180.0);
    assert_eq!(fit_tab_w(500.0, 4, &style), 120.0);
    assert_eq!(fit_tab_w(500.0, 20, &style), 90.0);
    let fixed = DockStyle {
        tab_w: 96.0,
        ..DockStyle::default()
    };
    assert_eq!(fit_tab_w(100.0, 9, &fixed), 96.0);
}
