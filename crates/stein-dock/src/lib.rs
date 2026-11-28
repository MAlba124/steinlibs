//! A docking workspace: a binary tree of splits whose leaves are tab groups.
//!
//! The tree lives in parallel arrays over caller supplied slices, in the style
//! of the rest of the stack: capacity is a hard ceiling that refuses rather
//! than grows, and free lists recycle slots the way `stein_core::Pool` does.
//!
//! Panels are a flat table, not owned by leaves. Each panel row records its
//! owning leaf and its position in that leaf's tab bar, so "the panels of a
//! leaf" is a filter over a contiguous table rather than a linked structure.
//! With panel counts in the tens the scans are cheaper than any indirection.
//!
//! Geometry is solved into per node rects by [`DockTree::solve`], and
//! [`DockTree::emit`] builds the workspace into a [`stein_ui::Ui`] frame with
//! the same one frame lag every widget in the stack accepts: interaction is
//! resolved from peeks against the previous frame's solve, then boxes are
//! emitted. Dividers straddle the split boundary so they cost no layout width
//! and child extents tile their region exactly.

#![no_std]

#[cfg(test)]
extern crate std;

mod emit;
mod geom;
mod persist;

#[cfg(test)]
mod tests;

pub use emit::{DockFrame, DockStyle, FramePanel, FrameTab, MAX_FRAME_PANELS, MAX_FRAME_TABS};
pub use geom::{DividerInfo, Dividers, Leaves, PanelsOf, Regions, preview_rect};

use stein_layout::{Axis, Rect};
use stein_ui::UiError;

pub(crate) const NONE: u32 = u32::MAX;
pub(crate) const RATIO_MIN: f32 = 0.05;
pub(crate) const RATIO_MAX: f32 = 0.95;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Free,
    Split,
    Leaf,
}

/// Where a dragged panel lands relative to a target leaf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropZone {
    /// Join the target's tab group.
    Center,
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockError {
    Ui(UiError),
    /// The widget state table is full, so persistence would silently vanish.
    StateFull,
    /// Backing slices disagree on capacity, or a capacity is zero.
    StorageMismatch,
    NodesFull,
    PanelsFull,
    NoSuchNode,
    NotALeaf,
    NotASplit,
    NoSuchPanel,
    /// The panel id is already docked somewhere in the tree.
    PanelDocked,
    /// A split ratio that is not a finite number.
    RatioOutOfRange,
    /// More leaves or panels than a [`DockFrame`] can report.
    FrameFull,
    DividerNotPositive,
    TabHeightNotPositive,
    /// `to_bytes` was given a slice smaller than the serialised tree.
    OutputTooSmall,
    BadMagic,
    BadVersion,
    Truncated,
    Corrupt,
}

impl From<UiError> for DockError {
    fn from(e: UiError) -> Self {
        DockError::Ui(e)
    }
}

/// Borrowed columns. Node columns must agree on length, panel columns too.
pub struct DockStorage<'a> {
    pub node_kind: &'a mut [NodeKind],
    pub node_axis: &'a mut [Axis],
    pub node_ratio: &'a mut [f32],
    pub node_a: &'a mut [u32],
    pub node_b: &'a mut [u32],
    pub node_parent: &'a mut [u32],
    /// Panel slot of the active tab, for leaves. `NONE` elsewhere.
    pub node_active: &'a mut [u32],
    pub node_rect: &'a mut [Rect],
    pub node_free: &'a mut [u32],
    pub panel_id: &'a mut [u32],
    /// Owning leaf per panel slot. `NONE` marks a free slot.
    pub panel_node: &'a mut [u32],
    pub panel_order: &'a mut [u16],
    pub panel_free: &'a mut [u32],
}

pub struct DockTree<'a> {
    pub(crate) s: DockStorage<'a>,
    pub(crate) root: u32,
    node_free_len: u32,
    panel_free_len: u32,
    node_live: u32,
    panel_live: u32,
    pub(crate) divider_px: f32,
    pub(crate) tab_h: f32,
}

impl<'a> DockTree<'a> {
    pub fn new(s: DockStorage<'a>) -> Result<Self, DockError> {
        let n = s.node_kind.len();
        let p = s.panel_id.len();
        let nodes_agree = s.node_axis.len() == n
            && s.node_ratio.len() == n
            && s.node_a.len() == n
            && s.node_b.len() == n
            && s.node_parent.len() == n
            && s.node_active.len() == n
            && s.node_rect.len() == n
            && s.node_free.len() == n;
        let panels_agree =
            s.panel_node.len() == p && s.panel_order.len() == p && s.panel_free.len() == p;
        if !nodes_agree || !panels_agree || n == 0 || p == 0 {
            return Err(DockError::StorageMismatch);
        }
        // NONE is a sentinel, so a slot may never carry that index.
        if n >= NONE as usize || p >= NONE as usize {
            return Err(DockError::StorageMismatch);
        }
        let mut t = DockTree {
            s,
            root: NONE,
            node_free_len: 0,
            panel_free_len: 0,
            node_live: 0,
            panel_live: 0,
            divider_px: 0.0,
            tab_h: 0.0,
        };
        t.reset();
        Ok(t)
    }

    /// Empty the tree. The storage may have been used before, and a tree that
    /// could be born holding nodes nobody inserted would be untrustworthy.
    pub(crate) fn reset(&mut self) {
        for k in self.s.node_kind.iter_mut() {
            *k = NodeKind::Free;
        }
        let n = self.s.node_free.len() as u32;
        // Descending, so the first alloc takes slot 0 and slot order tracks
        // insertion order for the common build-up case.
        for (i, f) in self.s.node_free.iter_mut().enumerate() {
            *f = n - 1 - i as u32;
        }
        for v in self.s.panel_node.iter_mut() {
            *v = NONE;
        }
        let p = self.s.panel_free.len() as u32;
        for (i, f) in self.s.panel_free.iter_mut().enumerate() {
            *f = p - 1 - i as u32;
        }
        self.node_free_len = n;
        self.panel_free_len = p;
        self.node_live = 0;
        self.panel_live = 0;
        self.root = NONE;
    }

    pub fn node_capacity(&self) -> usize {
        self.s.node_kind.len()
    }
    pub fn panel_capacity(&self) -> usize {
        self.s.panel_id.len()
    }
    pub fn node_count(&self) -> usize {
        self.node_live as usize
    }
    pub fn panel_count(&self) -> usize {
        self.panel_live as usize
    }
    pub fn root(&self) -> Option<u32> {
        (self.root != NONE).then_some(self.root)
    }
    pub fn leaf_count(&self) -> usize {
        self.s
            .node_kind
            .iter()
            .filter(|k| **k == NodeKind::Leaf)
            .count()
    }

    pub(crate) fn kind(&self, node: u32) -> NodeKind {
        self.s
            .node_kind
            .get(node as usize)
            .copied()
            .unwrap_or(NodeKind::Free)
    }
    pub fn is_leaf(&self, node: u32) -> bool {
        self.kind(node) == NodeKind::Leaf
    }
    pub fn is_split(&self, node: u32) -> bool {
        self.kind(node) == NodeKind::Split
    }
    pub fn parent_of(&self, node: u32) -> Option<u32> {
        if self.kind(node) == NodeKind::Free {
            return None;
        }
        let p = self.s.node_parent[node as usize];
        (p != NONE).then_some(p)
    }
    pub fn children_of(&self, split: u32) -> Option<(u32, u32)> {
        (self.kind(split) == NodeKind::Split)
            .then(|| (self.s.node_a[split as usize], self.s.node_b[split as usize]))
    }
    pub fn split_axis(&self, split: u32) -> Option<Axis> {
        (self.kind(split) == NodeKind::Split).then(|| self.s.node_axis[split as usize])
    }
    pub fn ratio_of(&self, split: u32) -> Option<f32> {
        (self.kind(split) == NodeKind::Split).then(|| self.s.node_ratio[split as usize])
    }

    /// Slot of a docked panel id.
    pub(crate) fn find_panel(&self, id: u32) -> Option<u32> {
        self.s
            .panel_id
            .iter()
            .zip(self.s.panel_node.iter())
            .position(|(pid, node)| *node != NONE && *pid == id)
            .map(|i| i as u32)
    }

    pub fn leaf_of(&self, panel: u32) -> Option<u32> {
        self.find_panel(panel)
            .map(|slot| self.s.panel_node[slot as usize])
    }

    /// Slot of the panel at `order` within `leaf`.
    pub(crate) fn panel_at(&self, leaf: u32, order: u16) -> Option<u32> {
        self.s
            .panel_node
            .iter()
            .zip(self.s.panel_order.iter())
            .position(|(n, o)| *n == leaf && *o == order)
            .map(|i| i as u32)
    }

    pub(crate) fn leaf_len(&self, leaf: u32) -> u16 {
        self.s.panel_node.iter().filter(|n| **n == leaf).count() as u16
    }

    pub fn active_panel(&self, leaf: u32) -> Option<u32> {
        if self.kind(leaf) != NodeKind::Leaf {
            return None;
        }
        let slot = self.s.node_active[leaf as usize];
        if slot == NONE || self.s.panel_node.get(slot as usize) != Some(&leaf) {
            return None;
        }
        Some(self.s.panel_id[slot as usize])
    }

    fn alloc_node(&mut self, kind: NodeKind) -> Result<u32, DockError> {
        if self.node_free_len == 0 {
            return Err(DockError::NodesFull);
        }
        self.node_free_len -= 1;
        let i = self.s.node_free[self.node_free_len as usize];
        let u = i as usize;
        self.s.node_kind[u] = kind;
        self.s.node_axis[u] = Axis::X;
        self.s.node_ratio[u] = 0.5;
        self.s.node_a[u] = NONE;
        self.s.node_b[u] = NONE;
        self.s.node_parent[u] = NONE;
        self.s.node_active[u] = NONE;
        self.s.node_rect[u] = Rect::default();
        self.node_live += 1;
        Ok(i)
    }

    fn free_node(&mut self, i: u32) {
        self.s.node_kind[i as usize] = NodeKind::Free;
        self.s.node_free[self.node_free_len as usize] = i;
        self.node_free_len += 1;
        self.node_live -= 1;
    }

    pub(crate) fn alloc_panel(&mut self, id: u32, leaf: u32, order: u16) -> Result<u32, DockError> {
        if self.panel_free_len == 0 {
            return Err(DockError::PanelsFull);
        }
        self.panel_free_len -= 1;
        let i = self.s.panel_free[self.panel_free_len as usize];
        let u = i as usize;
        self.s.panel_id[u] = id;
        self.s.panel_node[u] = leaf;
        self.s.panel_order[u] = order;
        self.panel_live += 1;
        Ok(i)
    }

    fn free_panel(&mut self, i: u32) {
        self.s.panel_node[i as usize] = NONE;
        self.s.panel_free[self.panel_free_len as usize] = i;
        self.panel_free_len += 1;
        self.panel_live -= 1;
    }

    /// Seed an initial layout. An empty tree gains a root leaf holding
    /// `panel`; a leaf root gains another tab. A split root refuses, because
    /// there is no single group to join and [`DockTree::dock`] names one.
    pub fn insert_root(&mut self, panel: u32) -> Result<(), DockError> {
        if self.find_panel(panel).is_some() {
            return Err(DockError::PanelDocked);
        }
        if self.root == NONE {
            return self.seed_root(panel);
        }
        if self.kind(self.root) != NodeKind::Leaf {
            return Err(DockError::NotALeaf);
        }
        self.join_leaf(panel, self.root)
    }

    fn seed_root(&mut self, panel: u32) -> Result<(), DockError> {
        if self.node_free_len == 0 {
            return Err(DockError::NodesFull);
        }
        if self.panel_free_len == 0 {
            return Err(DockError::PanelsFull);
        }
        let leaf = self.alloc_node(NodeKind::Leaf)?;
        let slot = self.alloc_panel(panel, leaf, 0)?;
        self.s.node_active[leaf as usize] = slot;
        self.root = leaf;
        Ok(())
    }

    fn join_leaf(&mut self, panel: u32, leaf: u32) -> Result<(), DockError> {
        let order = self.leaf_len(leaf);
        let slot = self.alloc_panel(panel, leaf, order)?;
        self.s.node_active[leaf as usize] = slot;
        Ok(())
    }

    /// Dock an undocked `panel` relative to `target_leaf`. Center joins the
    /// tab group; a side wraps the leaf in a new split at ratio 0.5 with the
    /// panel on that side.
    pub fn dock(&mut self, panel: u32, target_leaf: u32, zone: DropZone) -> Result<(), DockError> {
        if self.find_panel(panel).is_some() {
            return Err(DockError::PanelDocked);
        }
        match self.kind(target_leaf) {
            NodeKind::Leaf => {}
            NodeKind::Split => return Err(DockError::NotALeaf),
            NodeKind::Free => return Err(DockError::NoSuchNode),
        }
        match zone {
            DropZone::Center => self.join_leaf(panel, target_leaf),
            _ => self.wrap_in_split(panel, target_leaf, zone),
        }
    }

    /// Split the whole workspace, wrapping the root. Center joins a leaf root.
    pub fn dock_to_root(&mut self, panel: u32, zone: DropZone) -> Result<(), DockError> {
        if self.find_panel(panel).is_some() {
            return Err(DockError::PanelDocked);
        }
        if self.root == NONE {
            return self.insert_root(panel);
        }
        let center = matches!(zone, DropZone::Center);
        if center && self.kind(self.root) != NodeKind::Leaf {
            return Err(DockError::NotALeaf);
        }
        if center {
            return self.join_leaf(panel, self.root);
        }
        self.wrap_in_split(panel, self.root, zone)
    }

    /// Wrap `target` (leaf or subtree) in a new split with a fresh leaf
    /// holding `panel` on the `zone` side.
    fn wrap_in_split(&mut self, panel: u32, target: u32, zone: DropZone) -> Result<(), DockError> {
        // Checked up front so a refusal cannot leave a half built split.
        if self.node_free_len < 2 {
            return Err(DockError::NodesFull);
        }
        if self.panel_free_len < 1 {
            return Err(DockError::PanelsFull);
        }
        let new_leaf = self.alloc_node(NodeKind::Leaf)?;
        let slot = self.alloc_panel(panel, new_leaf, 0)?;
        self.s.node_active[new_leaf as usize] = slot;
        let split = self.alloc_node(NodeKind::Split)?;
        let u = split as usize;
        self.s.node_axis[u] = zone_axis(zone);
        self.s.node_ratio[u] = 0.5;
        let (a, b) = if zone_leads(zone) {
            (new_leaf, target)
        } else {
            (target, new_leaf)
        };
        self.s.node_a[u] = a;
        self.s.node_b[u] = b;
        let parent = self.s.node_parent[target as usize];
        self.s.node_parent[u] = parent;
        self.s.node_parent[new_leaf as usize] = split;
        self.s.node_parent[target as usize] = split;
        self.replace_child(parent, target, split);
        Ok(())
    }

    fn replace_child(&mut self, parent: u32, from: u32, to: u32) {
        if parent == NONE && self.root == from {
            self.root = to;
        }
        if parent == NONE {
            return;
        }
        let u = parent as usize;
        if self.s.node_a[u] == from {
            self.s.node_a[u] = to;
            return;
        }
        if self.s.node_b[u] == from {
            self.s.node_b[u] = to;
        }
    }

    /// Undock `panel`. An emptied leaf collapses together with its parent
    /// split, promoting the sibling; the last panel empties the tree.
    pub fn remove(&mut self, panel: u32) -> Result<(), DockError> {
        let slot = self.find_panel(panel).ok_or(DockError::NoSuchPanel)?;
        let leaf = self.s.panel_node[slot as usize];
        let ord = self.s.panel_order[slot as usize];
        let was_active = self.s.node_active[leaf as usize] == slot;
        self.free_panel(slot);
        self.compact_orders(leaf, ord);
        let n = self.leaf_len(leaf);
        if n == 0 {
            self.s.node_active[leaf as usize] = NONE;
            self.collapse_leaf(leaf);
            return Ok(());
        }
        if was_active {
            self.fix_active_after_remove(leaf, ord, n);
        }
        Ok(())
    }

    /// The tab that inherits the selection is the one now sitting where the
    /// removed tab was, or the last tab when the end was removed.
    fn fix_active_after_remove(&mut self, leaf: u32, ord: u16, n: u16) {
        let pick = if ord < n { ord } else { n - 1 };
        let Some(next) = self.panel_at(leaf, pick) else {
            return;
        };
        self.s.node_active[leaf as usize] = next;
    }

    fn compact_orders(&mut self, leaf: u32, above: u16) {
        let it = self.s.panel_node.iter().zip(self.s.panel_order.iter_mut());
        for (_, order) in it.filter(|(n, o)| **n == leaf && **o > above) {
            *order -= 1;
        }
    }

    fn collapse_leaf(&mut self, leaf: u32) {
        let parent = self.s.node_parent[leaf as usize];
        if parent == NONE {
            // A parentless leaf is the root, since only reachable leaves
            // carry panels.
            self.root = NONE;
            self.free_node(leaf);
            return;
        }
        let pu = parent as usize;
        let sibling = if self.s.node_a[pu] == leaf {
            self.s.node_b[pu]
        } else {
            self.s.node_a[pu]
        };
        let grand = self.s.node_parent[pu];
        self.s.node_parent[sibling as usize] = grand;
        self.replace_child(grand, parent, sibling);
        self.free_node(leaf);
        self.free_node(parent);
    }

    /// Select `panel` within its tab group.
    pub fn activate(&mut self, panel: u32) -> Result<(), DockError> {
        let slot = self.find_panel(panel).ok_or(DockError::NoSuchPanel)?;
        let leaf = self.s.panel_node[slot as usize];
        self.s.node_active[leaf as usize] = slot;
        Ok(())
    }

    pub fn set_ratio(&mut self, split: u32, ratio: f32) -> Result<(), DockError> {
        if self.kind(split) != NodeKind::Split {
            return Err(DockError::NotASplit);
        }
        if !ratio.is_finite() {
            return Err(DockError::RatioOutOfRange);
        }
        self.s.node_ratio[split as usize] = clamp_ratio(ratio);
        Ok(())
    }

    /// Remove then dock. A panel that is the sole occupant of `target_leaf`
    /// is a no-op in every zone, because removing it would delete the leaf it
    /// is about to dock into.
    pub fn move_panel(
        &mut self,
        panel: u32,
        target_leaf: u32,
        zone: DropZone,
    ) -> Result<(), DockError> {
        let slot = self.find_panel(panel).ok_or(DockError::NoSuchPanel)?;
        match self.kind(target_leaf) {
            NodeKind::Leaf => {}
            NodeKind::Split => return Err(DockError::NotALeaf),
            NodeKind::Free => return Err(DockError::NoSuchNode),
        }
        let src = self.s.panel_node[slot as usize];
        if src == target_leaf && self.leaf_len(target_leaf) == 1 {
            return Ok(());
        }
        // A side dock needs two nodes. Refuse before removing, or the panel
        // would be lost when the re-dock refuses after the removal.
        let side = !matches!(zone, DropZone::Center);
        if side && (self.node_free_len as usize) + collapse_gain(self, src) < 2 {
            return Err(DockError::NodesFull);
        }
        self.remove(panel)?;
        self.dock(panel, target_leaf, zone)
    }
}

/// Nodes a removal of the last panel of `leaf` would hand back.
fn collapse_gain(t: &DockTree<'_>, leaf: u32) -> usize {
    if t.leaf_len(leaf) != 1 {
        return 0;
    }
    if t.s.node_parent[leaf as usize] == NONE {
        1
    } else {
        2
    }
}

fn zone_axis(zone: DropZone) -> Axis {
    match zone {
        DropZone::Left | DropZone::Right => Axis::X,
        _ => Axis::Y,
    }
}

fn zone_leads(zone: DropZone) -> bool {
    matches!(zone, DropZone::Left | DropZone::Top)
}

pub(crate) fn clamp_ratio(r: f32) -> f32 {
    fmin(fmax(r, RATIO_MIN), RATIO_MAX)
}

#[inline]
pub(crate) fn fmax(a: f32, b: f32) -> f32 {
    if a > b { a } else { b }
}
#[inline]
pub(crate) fn fmin(a: f32, b: f32) -> f32 {
    if a < b { a } else { b }
}
#[inline]
pub(crate) fn fabs(a: f32) -> f32 {
    if a < 0.0 { -a } else { a }
}
#[inline]
pub(crate) fn axis_index(a: Axis) -> usize {
    match a {
        Axis::X => 0,
        Axis::Y => 1,
    }
}
