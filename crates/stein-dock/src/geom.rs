//! Solving the tree into rects, and the queries a renderer needs.
//!
//! Children tile the full extent of their split at the ratio; the divider is
//! a grab region straddling the boundary, so it costs no layout width and the
//! leaf rects sum exactly to the root rect. Same model as the table's column
//! dividers.

use stein_layout::{Axis, Rect};

use crate::{DockTree, DropZone, NONE, NodeKind, axis_index, fmax, fmin};

/// One split's divider: where to grab it and the region a drag is relative to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DividerInfo {
    pub split: u32,
    pub axis: Axis,
    pub grab: Rect,
    pub region: Rect,
}

impl DockTree<'_> {
    /// Fill per node rects. `divider_px` only sizes the grab regions returned
    /// by [`DockTree::dividers`]; `tab_h` only offsets
    /// [`DockTree::content_rect_of`].
    pub fn solve(&mut self, root_rect: Rect, divider_px: f32, tab_h: f32) {
        self.divider_px = fmax(divider_px, 0.0);
        self.tab_h = fmax(tab_h, 0.0);
        if self.root != NONE {
            self.solve_at(self.root, root_rect);
        }
    }

    fn solve_at(&mut self, node: u32, rect: Rect) {
        let u = node as usize;
        self.s.node_rect[u] = rect;
        if self.s.node_kind[u] != NodeKind::Split {
            return;
        }
        let a = axis_index(self.s.node_axis[u]);
        let extent = fmax(rect.max[a] - rect.min[a], 0.0);
        let cut = rect.min[a] + extent * self.s.node_ratio[u];
        let (ca, cb) = (self.s.node_a[u], self.s.node_b[u]);
        let mut ra = rect;
        ra.max[a] = cut;
        let mut rb = rect;
        rb.min[a] = cut;
        self.solve_at(ca, ra);
        self.solve_at(cb, rb);
    }

    /// Solved rect of a live node. Zero until the first solve.
    pub fn rect_of(&self, node: u32) -> Option<Rect> {
        (self.kind(node) != NodeKind::Free).then(|| self.s.node_rect[node as usize])
    }

    /// A leaf's rect below its tab bar.
    pub fn content_rect_of(&self, leaf: u32) -> Option<Rect> {
        if self.kind(leaf) != NodeKind::Leaf {
            return None;
        }
        let r = self.s.node_rect[leaf as usize];
        let top = fmin(r.min[1] + self.tab_h, r.max[1]);
        Some(Rect {
            min: [r.min[0], top],
            max: r.max,
        })
    }

    pub(crate) fn divider_info(&self, split: u32) -> DividerInfo {
        let u = split as usize;
        let axis = self.s.node_axis[u];
        let a = axis_index(axis);
        let region = self.s.node_rect[u];
        let extent = fmax(region.max[a] - region.min[a], 0.0);
        let cut = region.min[a] + extent * self.s.node_ratio[u];
        let half = self.divider_px * 0.5;
        let mut grab = region;
        grab.min[a] = cut - half;
        grab.max[a] = cut + half;
        DividerInfo {
            split,
            axis,
            grab,
            region,
        }
    }

    pub fn leaves(&self) -> Leaves<'_> {
        Leaves {
            kind: self.s.node_kind,
            next: 0,
        }
    }

    /// Panel ids of a leaf in tab order.
    pub fn panels_of(&self, leaf: u32) -> PanelsOf<'_> {
        PanelsOf {
            id: self.s.panel_id,
            node: self.s.panel_node,
            order: self.s.panel_order,
            leaf,
            ord: 0,
        }
    }

    pub fn dividers(&self) -> Dividers<'_, '_> {
        Dividers {
            tree: self,
            next: 0,
        }
    }

    /// Leaf rects, for drop hit testing.
    pub fn regions(&self) -> Regions<'_, '_> {
        Regions {
            tree: self,
            next: 0,
        }
    }

    /// The five zone hit test. Over a leaf's tab bar is Center, joining the
    /// group. Below it, the middle ~40% of the content in both axes is
    /// Center, otherwise the nearest edge by normalised distance. Ties at a
    /// diagonal corner go to the horizontal edge, Left before Right.
    pub fn drop_zone_at(&self, p: [f32; 2]) -> Option<(u32, DropZone)> {
        let leaf = self
            .leaves()
            .find(|l| self.s.node_rect[*l as usize].contains(p))?;
        let mut r = self.s.node_rect[leaf as usize];
        if p[1] < r.min[1] + self.tab_h {
            return Some((leaf, DropZone::Center));
        }
        r.min[1] = fmin(r.min[1] + self.tab_h, r.max[1]);
        Some((leaf, zone_in_rect(r, p)))
    }
}

fn zone_in_rect(r: Rect, p: [f32; 2]) -> DropZone {
    let w = fmax(r.max[0] - r.min[0], 1.0e-6);
    let h = fmax(r.max[1] - r.min[1], 1.0e-6);
    let nx = (p[0] - r.min[0]) / w;
    let ny = (p[1] - r.min[1]) / h;
    if (0.3..=0.7).contains(&nx) && (0.3..=0.7).contains(&ny) {
        return DropZone::Center;
    }
    let d = [nx, 1.0 - nx, ny, 1.0 - ny];
    let zones = [
        DropZone::Left,
        DropZone::Right,
        DropZone::Top,
        DropZone::Bottom,
    ];
    let mut best = 0;
    for i in 1..4 {
        if d[i] < d[best] {
            best = i;
        }
    }
    zones[best]
}

/// The region a drop into `zone` of a leaf with `rect` would occupy.
pub fn preview_rect(rect: Rect, zone: DropZone) -> Rect {
    let mx = (rect.min[0] + rect.max[0]) * 0.5;
    let my = (rect.min[1] + rect.max[1]) * 0.5;
    match zone {
        DropZone::Center => rect,
        DropZone::Left => Rect {
            min: rect.min,
            max: [mx, rect.max[1]],
        },
        DropZone::Right => Rect {
            min: [mx, rect.min[1]],
            max: rect.max,
        },
        DropZone::Top => Rect {
            min: rect.min,
            max: [rect.max[0], my],
        },
        DropZone::Bottom => Rect {
            min: [rect.min[0], my],
            max: rect.max,
        },
    }
}

pub struct Leaves<'t> {
    kind: &'t [NodeKind],
    next: usize,
}

impl Iterator for Leaves<'_> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        let off = self.kind[self.next..]
            .iter()
            .position(|k| *k == NodeKind::Leaf)?;
        let i = self.next + off;
        self.next = i + 1;
        Some(i as u32)
    }
}

pub struct PanelsOf<'t> {
    id: &'t [u32],
    node: &'t [u32],
    order: &'t [u16],
    leaf: u32,
    ord: u16,
}

impl Iterator for PanelsOf<'_> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        let want = self.ord;
        let hit = self
            .node
            .iter()
            .zip(self.order.iter())
            .position(|(n, o)| *n == self.leaf && *o == want)?;
        self.ord += 1;
        Some(self.id[hit])
    }
}

pub struct Dividers<'t, 'a> {
    tree: &'t DockTree<'a>,
    next: usize,
}

impl Iterator for Dividers<'_, '_> {
    type Item = DividerInfo;
    fn next(&mut self) -> Option<DividerInfo> {
        let cap = self.tree.node_capacity();
        let i = (self.next..cap).find(|i| self.tree.is_split(*i as u32))?;
        self.next = i + 1;
        Some(self.tree.divider_info(i as u32))
    }
}

pub struct Regions<'t, 'a> {
    tree: &'t DockTree<'a>,
    next: usize,
}

impl Iterator for Regions<'_, '_> {
    type Item = (u32, Rect);
    fn next(&mut self) -> Option<(u32, Rect)> {
        let cap = self.tree.node_capacity();
        let i = (self.next..cap).find(|i| self.tree.is_leaf(*i as u32))?;
        self.next = i + 1;
        Some((i as u32, self.tree.s.node_rect[i]))
    }
}
