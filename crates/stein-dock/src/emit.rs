//! Building the workspace into a stein-ui frame.
//!
//! Interaction first, boxes second, like every widget in the stack: the
//! previous frame's solve is read out of `WidgetState`, divider drags and tab
//! drags are resolved from peeks against it, the tree is mutated, and only
//! then are boxes emitted. The drag-to-redock state lives in the workspace
//! key's `user` words so it survives the immediate mode rebuild.

use stein_layout::{Axis, BoxSpec, Rect, SizeSpec};
use stein_theme::Theme;
use stein_ui::key::{child, hash_str, indexed, seeded};
use stein_ui::{Ui, WidgetState};

use crate::geom::preview_rect;
use crate::{DockError, DockTree, DropZone, NONE, NodeKind, axis_index, fabs, fmax, fmin};

/// Leaves a frame can report. Refused up front, not truncated.
pub const MAX_FRAME_PANELS: usize = 64;
/// Tabs a frame can report.
pub const MAX_FRAME_TABS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DockStyle {
    /// Grab width of a divider, straddling the split boundary.
    pub divider: f32,
    pub tab_h: f32,
    /// Tab width, the widest a tab gets. This crate never measures text.
    pub tab_w: f32,
    /// The narrowest a tab gets when a group has more than its bar holds at
    /// `tab_w`. Past that the bar scrolls. Zero keeps every tab at `tab_w`.
    pub tab_min_w: f32,
    /// Room kept free after the last tab, for the caller's own button.
    pub bar_reserve: f32,
    /// Pixels of tab drag before a re-dock drag starts.
    pub drag_threshold: f32,
    /// Pixels per wheel notch over a tab bar.
    pub tab_wheel_step: f32,
    pub bar_fill: u32,
    pub tab_fill: u32,
    pub tab_hot_fill: u32,
    pub tab_active_fill: u32,
    pub divider_fill: u32,
    pub divider_hot_fill: u32,
    pub content_fill: u32,
    pub preview_fill: u32,
}

impl Default for DockStyle {
    fn default() -> Self {
        DockStyle::from(&Theme::default())
    }
}

impl From<&Theme> for DockStyle {
    fn from(t: &Theme) -> Self {
        DockStyle {
            divider: t.size.divider,
            tab_h: 26.0,
            tab_w: 96.0,
            tab_min_w: 0.0,
            bar_reserve: 0.0,
            drag_threshold: 6.0,
            tab_wheel_step: 60.0,
            bar_fill: t.bg,
            tab_fill: t.bg,
            tab_hot_fill: t.hover,
            tab_active_fill: t.pane,
            divider_fill: t.bg,
            divider_hot_fill: t.accent,
            content_fill: t.pane,
            preview_fill: t.drop,
        }
    }
}

/// One visible panel: the active tab of its leaf, with the content box the
/// caller draws into.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FramePanel {
    pub panel: u32,
    pub leaf: u32,
    pub box_index: u32,
    pub rect: Rect,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameTab {
    pub panel: u32,
    pub leaf: u32,
    pub box_index: u32,
    pub rect: Rect,
    pub active: bool,
}

/// What one emit concluded, for the caller to draw labels and panel contents.
pub struct DockFrame {
    panels: [FramePanel; MAX_FRAME_PANELS],
    panel_len: u32,
    tabs: [FrameTab; MAX_FRAME_TABS],
    tab_len: u32,
    /// Panel id mid drag, if any.
    pub dragging: Option<u32>,
    /// Where a drop right now would land.
    pub hover: Option<(u32, DropZone)>,
    /// A drop was refused because the tree was out of node or panel slots.
    pub drop_refused: bool,
    /// Panels whose [`DockTree::emit_with`] callback returned an error or
    /// left the box stack unbalanced. A count, not the errors, since the
    /// error type belongs to the caller.
    pub panel_errors: u32,
}

impl DockFrame {
    fn new() -> Self {
        DockFrame {
            panels: [FramePanel::default(); MAX_FRAME_PANELS],
            panel_len: 0,
            tabs: [FrameTab::default(); MAX_FRAME_TABS],
            tab_len: 0,
            dragging: None,
            hover: None,
            drop_refused: false,
            panel_errors: 0,
        }
    }
    pub fn panels(&self) -> &[FramePanel] {
        &self.panels[..self.panel_len as usize]
    }
    pub fn tabs(&self) -> &[FrameTab] {
        &self.tabs[..self.tab_len as usize]
    }
    fn push_panel(&mut self, p: FramePanel) {
        let n = self.panel_len as usize;
        if n < MAX_FRAME_PANELS {
            self.panels[n] = p;
            self.panel_len += 1;
        }
    }
    fn push_tab(&mut self, t: FrameTab) {
        let n = self.tab_len as usize;
        if n < MAX_FRAME_TABS {
            self.tabs[n] = t;
            self.tab_len += 1;
        }
    }
}

/// Workspace `user[0]` bit: a tab drag is in flight. `user[1]` is the panel.
const DRAG_FLAG: u64 = 1;
/// Divider key `user[0]` bit: held last frame. `user[1]` is the anchor ratio
/// as f32 bits, captured on the press edge so a long drag cannot accumulate
/// rounding.
const HELD: u64 = 1;

pub(crate) fn k_node(key: u64, node: u32) -> u64 {
    indexed(key, hash_str("dock.node"), u64::from(node))
}
pub(crate) fn k_bar(key: u64, node: u32) -> u64 {
    indexed(key, hash_str("dock.bar"), u64::from(node))
}
pub(crate) fn k_content(key: u64, node: u32) -> u64 {
    indexed(key, hash_str("dock.content"), u64::from(node))
}
pub(crate) fn k_div(key: u64, node: u32) -> u64 {
    indexed(key, hash_str("dock.div"), u64::from(node))
}
/// Seeded by panel id, so a tab keeps its identity when it moves leaves.
pub(crate) fn k_tab(key: u64, panel: u32) -> u64 {
    seeded(key, hash_str("dock.tab"), u64::from(panel))
}
pub(crate) fn k_preview(key: u64) -> u64 {
    child(key, hash_str("dock.preview"))
}

fn load(ui: &mut Ui, key: u64) -> Result<WidgetState, DockError> {
    ui.state(key).map(|s| *s).ok_or(DockError::StateFull)
}

fn store(ui: &mut Ui, key: u64, st: WidgetState) -> Result<(), DockError> {
    match ui.state(key) {
        Some(s) => {
            *s = st;
            Ok(())
        }
        None => Err(DockError::StateFull),
    }
}

fn sized(a: usize, along: SizeSpec, cross: SizeSpec) -> [SizeSpec; 2] {
    if a == 0 {
        [along, cross]
    } else {
        [cross, along]
    }
}

struct DragState {
    dragging: Option<u32>,
    hover: Option<(u32, DropZone)>,
    refused: bool,
}

struct TabCtx {
    leaf: u32,
    slot: u32,
    ord: u16,
    /// The leaf's bar scroll, so the reported rect is the on-screen one.
    offset: f32,
    /// This group's tab width.
    w: f32,
}

/// Tab width for `n` tabs in a bar `bar_w` wide with `reserve` kept free:
/// as wide as `style.tab_w`, shrinking to fit down to `style.tab_min_w`.
pub(crate) fn fit_tab_w(bar_w: f32, n: u16, style: &DockStyle) -> f32 {
    let max = style.tab_w;
    if style.tab_min_w <= 0.0 || style.tab_min_w >= max || n == 0 {
        return max;
    }
    let share = (bar_w - style.bar_reserve) / f32::from(n);
    fmin(fmax(share, style.tab_min_w), max)
}

/// Walk state threaded through the tree emission.
struct EmitCtx<'a, F> {
    key: u64,
    /// The caller's open box when emit began, the floor rebalancing never
    /// closes past.
    caller_parent: u64,
    style: &'a DockStyle,
    frame: &'a mut DockFrame,
    panel: &'a mut F,
}

fn tab_fill(style: &DockStyle, active: bool, hot: bool) -> u32 {
    if active {
        return style.tab_active_fill;
    }
    if hot {
        return style.tab_hot_fill;
    }
    style.tab_fill
}

fn noop_panel(_: &mut Ui, _: u32) -> Result<(), core::convert::Infallible> {
    Ok(())
}

/// Zero sized and floating, so a rebalance duplicate neither claims layout
/// space nor paints.
fn ghost_spec() -> BoxSpec {
    BoxSpec::container(Axis::X)
        .w(SizeSpec::px(0.0))
        .h(SizeSpec::px(0.0))
        .floating([0.0, 0.0])
}

impl DockTree<'_> {
    /// Emit the workspace. Panel content is the caller's business: each
    /// visible panel gets a clipped content box, reported in the frame with
    /// its rect from this crate's own solve.
    pub fn emit(
        &mut self,
        ui: &mut Ui,
        key: u64,
        size: [SizeSpec; 2],
        style: DockStyle,
    ) -> Result<DockFrame, DockError> {
        self.emit_with(ui, key, size, style, noop_panel)
    }

    /// [`DockTree::emit`], but `panel` is called once per visible panel with
    /// that panel's content box open, so the caller can emit real child boxes
    /// into it. The dock closes the box afterwards.
    ///
    /// The frame stays balanced whatever the callback does: an error, boxes
    /// left open or extra closes are repaired against the known chain of open
    /// boxes and counted in [`DockFrame::panel_errors`], and the remaining
    /// panels still get their call.
    pub fn emit_with<E, F>(
        &mut self,
        ui: &mut Ui,
        key: u64,
        size: [SizeSpec; 2],
        style: DockStyle,
        mut panel: F,
    ) -> Result<DockFrame, DockError>
    where
        F: FnMut(&mut Ui, u32) -> Result<(), E>,
    {
        if style.divider <= 0.0 {
            return Err(DockError::DividerNotPositive);
        }
        if style.tab_h <= 0.0 {
            return Err(DockError::TabHeightNotPositive);
        }
        if self.leaf_count() > MAX_FRAME_PANELS || self.panel_count() > MAX_FRAME_TABS {
            return Err(DockError::FrameFull);
        }
        let caller_parent = ui.parent_key();
        let st = load(ui, key)?;
        if st.is_fresh(ui.frame_index()) {
            // Everything is sized blind until the first solve lands.
            ui.request_repaint();
        }
        let ws = st.last_rect;
        self.solve(ws, style.divider, style.tab_h);
        self.drag_dividers(ui, key)?;
        let drag = self.resolve_panel_drag(ui, key, style.drag_threshold)?;
        // Interactions may have moved panels or ratios; the frame reports the
        // tree as it is now.
        self.solve(ws, style.divider, style.tab_h);
        let mut frame = DockFrame::new();
        frame.dragging = drag.dragging;
        frame.hover = drag.hover;
        frame.drop_refused = drag.refused;
        ui.open(
            key,
            BoxSpec::container(Axis::X).w(size[0]).h(size[1]).clip(),
            0,
        )?;
        let root = self.root;
        if root != NONE {
            let mut ctx = EmitCtx {
                key,
                caller_parent,
                style: &style,
                frame: &mut frame,
                panel: &mut panel,
            };
            self.emit_node(ui, &mut ctx, root, [SizeSpec::grow(); 2])?;
        }
        if let Some((leaf, zone)) = drag.hover {
            self.emit_preview(ui, key, ws, leaf, zone, style.preview_fill)?;
        }
        ui.close()?;
        Ok(frame)
    }

    fn drag_dividers(&mut self, ui: &mut Ui, key: u64) -> Result<(), DockError> {
        for i in 0..self.node_capacity() as u32 {
            self.drag_divider(ui, key, i)?;
        }
        Ok(())
    }

    fn drag_divider(&mut self, ui: &mut Ui, key: u64, node: u32) -> Result<(), DockError> {
        if self.kind(node) != NodeKind::Split {
            return Ok(());
        }
        let dk = k_div(key, node);
        let held = ui.peek_active(dk);
        let mut st = load(ui, dk)?;
        let was = st.user[0] & HELD != 0;
        if held && !was {
            st.user[1] = u64::from(self.s.node_ratio[node as usize].to_bits());
        }
        st.user[0] = u64::from(held) * HELD;
        store(ui, dk, st)?;
        if !held {
            return Ok(());
        }
        let a = axis_index(self.s.node_axis[node as usize]);
        let r = self.s.node_rect[node as usize];
        let extent = fmax(r.max[a] - r.min[a], 0.0);
        if extent <= 0.0 {
            return Ok(());
        }
        let anchor = f32::from_bits(st.user[1] as u32);
        let ratio = anchor + ui.peek_drag(dk).total[a] / extent;
        if ratio.is_finite() {
            self.set_ratio(node, ratio)?;
            ui.request_repaint();
        }
        Ok(())
    }

    fn resolve_panel_drag(
        &mut self,
        ui: &mut Ui,
        key: u64,
        threshold: f32,
    ) -> Result<DragState, DockError> {
        let mut st = load(ui, key)?;
        let stored = (st.user[0] & DRAG_FLAG != 0).then_some(st.user[1] as u32);
        let mut dragging = stored;
        let mut refused = false;
        if let Some(p) = stored.filter(|p| !self.tab_held(ui, key, *p)) {
            refused = self.finish_drag(ui, p)?;
            dragging = None;
        }
        if stored.is_none() {
            dragging = self.drag_candidate(ui, key, threshold);
        }
        if stored.is_none() && dragging.is_some() {
            ui.request_repaint();
        }
        st.user[0] = u64::from(dragging.is_some()) * DRAG_FLAG;
        st.user[1] = u64::from(dragging.unwrap_or(0));
        store(ui, key, st)?;
        let hover = dragging.and_then(|_| self.drop_zone_at(ui.input().pointer));
        Ok(DragState {
            dragging,
            hover,
            refused,
        })
    }

    fn tab_held(&self, ui: &Ui, key: u64, panel: u32) -> bool {
        self.find_panel(panel).is_some() && ui.peek_active(k_tab(key, panel))
    }

    /// Apply a released drag. True when the drop was refused for capacity,
    /// which cancels rather than errors: the tree is untouched and the caller
    /// is told through the frame.
    fn finish_drag(&mut self, ui: &mut Ui, panel: u32) -> Result<bool, DockError> {
        ui.request_repaint();
        if self.find_panel(panel).is_none() {
            return Ok(false);
        }
        let Some((leaf, zone)) = self.drop_zone_at(ui.input().pointer) else {
            return Ok(false);
        };
        match self.move_panel(panel, leaf, zone) {
            Ok(()) => Ok(false),
            Err(DockError::NodesFull | DockError::PanelsFull) => Ok(true),
            Err(e) => Err(e),
        }
    }

    /// The held tab, once its drag has crossed the threshold. Only one key
    /// is active at a time, so the first hit settles it.
    fn drag_candidate(&self, ui: &Ui, key: u64, threshold: f32) -> Option<u32> {
        let it = self.s.panel_id.iter().zip(self.s.panel_node.iter());
        let pid = it
            .filter(|(_, node)| **node != NONE)
            .map(|(pid, _)| *pid)
            .find(|pid| ui.peek_active(k_tab(key, *pid)))?;
        let t = ui.peek_drag(k_tab(key, pid)).total;
        (fabs(t[0]) > threshold || fabs(t[1]) > threshold).then_some(pid)
    }

    fn emit_node<E, F>(
        &mut self,
        ui: &mut Ui,
        ctx: &mut EmitCtx<'_, F>,
        node: u32,
        size: [SizeSpec; 2],
    ) -> Result<(), DockError>
    where
        F: FnMut(&mut Ui, u32) -> Result<(), E>,
    {
        match self.kind(node) {
            NodeKind::Split => self.emit_split(ui, ctx, node, size),
            NodeKind::Leaf => self.emit_leaf(ui, ctx, node, size),
            NodeKind::Free => Err(DockError::NoSuchNode),
        }
    }

    fn emit_split<E, F>(
        &mut self,
        ui: &mut Ui,
        ctx: &mut EmitCtx<'_, F>,
        node: u32,
        size: [SizeSpec; 2],
    ) -> Result<(), DockError>
    where
        F: FnMut(&mut Ui, u32) -> Result<(), E>,
    {
        let u = node as usize;
        let axis = self.s.node_axis[u];
        let a = axis_index(axis);
        ui.open(
            k_node(ctx.key, node),
            BoxSpec::container(axis).w(size[0]).h(size[1]),
            0,
        )?;
        let ratio = self.s.node_ratio[u];
        let (ca, cb) = (self.s.node_a[u], self.s.node_b[u]);
        let sa = sized(a, SizeSpec::fraction(ratio), SizeSpec::grow());
        self.emit_node(ui, ctx, ca, sa)?;
        let sb = sized(a, SizeSpec::grow(), SizeSpec::grow());
        self.emit_node(ui, ctx, cb, sb)?;
        // Last, so the grab region paints over both children.
        self.emit_divider(ui, ctx.key, node, a, ctx.style)?;
        ui.close()?;
        Ok(())
    }

    fn emit_divider(
        &mut self,
        ui: &mut Ui,
        key: u64,
        node: u32,
        a: usize,
        style: &DockStyle,
    ) -> Result<(), DockError> {
        let k = k_div(key, node);
        let hot = ui.peek_hovered(k) || ui.peek_active(k);
        let fill = if hot {
            style.divider_hot_fill
        } else {
            style.divider_fill
        };
        let u = node as usize;
        let r = self.s.node_rect[u];
        let extent = fmax(r.max[a] - r.min[a], 0.0);
        let mut offset = [0.0, 0.0];
        offset[a] = extent * self.s.node_ratio[u] - style.divider * 0.5;
        let ds = sized(a, SizeSpec::px(style.divider), SizeSpec::grow());
        ui.leaf(k, BoxSpec::leaf().w(ds[0]).h(ds[1]).floating(offset), fill)?;
        Ok(())
    }

    fn emit_leaf<E, F>(
        &mut self,
        ui: &mut Ui,
        ctx: &mut EmitCtx<'_, F>,
        node: u32,
        size: [SizeSpec; 2],
    ) -> Result<(), DockError>
    where
        F: FnMut(&mut Ui, u32) -> Result<(), E>,
    {
        ui.open(
            k_node(ctx.key, node),
            BoxSpec::container(Axis::Y).w(size[0]).h(size[1]).clip(),
            0,
        )?;
        let bar_w = self.s.node_rect[node as usize].width();
        let tab_w = fit_tab_w(bar_w, self.leaf_len(node), ctx.style);
        let offset = self.bar_offset(ui, ctx.key, ctx.style, node, tab_w)?;
        // `scroll`, not a floating strip: a floating box restarts its clip
        // from the viewport, so scrolled tabs would escape the bar. The tabs
        // are strict px children, so overflow is real rather than squeezed.
        ui.open(
            k_bar(ctx.key, node),
            BoxSpec::container(Axis::X)
                .w(SizeSpec::grow())
                .h(SizeSpec::px(ctx.style.tab_h))
                .clip()
                .scroll([offset, 0.0]),
            ctx.style.bar_fill,
        )?;
        let clicked = self.emit_tabs(ui, ctx, node, offset, tab_w)?;
        ui.close()?;
        ui.open(
            k_content(ctx.key, node),
            BoxSpec::container(Axis::X)
                .w(SizeSpec::grow())
                .h(SizeSpec::grow())
                .clip(),
            ctx.style.content_fill,
        )?;
        let content_index = ui.last_index();
        let active = self.active_panel(node);
        if let Some(pid) = active {
            self.run_panel(ui, ctx, node, pid)?;
        }
        ui.close()?;
        ui.close()?;
        if let Some(pid) = active {
            ctx.frame.push_panel(FramePanel {
                panel: pid,
                leaf: node,
                box_index: content_index,
                rect: self.content_rect_of(node).unwrap_or_default(),
            });
        }
        if let Some(pid) = clicked {
            self.activate(pid)?;
            // The tabs were painted with the old selection.
            ui.request_repaint();
        }
        Ok(())
    }

    /// Run the panel callback with the content box open, then repair the
    /// stack against whatever it did.
    fn run_panel<E, F>(
        &mut self,
        ui: &mut Ui,
        ctx: &mut EmitCtx<'_, F>,
        node: u32,
        pid: u32,
    ) -> Result<(), DockError>
    where
        F: FnMut(&mut Ui, u32) -> Result<(), E>,
    {
        let ok = (ctx.panel)(ui, pid).is_ok();
        let clean = self.rebalance(ui, ctx.key, ctx.caller_parent, node)?;
        if ok && clean {
            return Ok(());
        }
        ctx.frame.panel_errors += 1;
        // A repaired frame may hold ghost boxes; the next build is clean.
        ui.request_repaint();
        Ok(())
    }

    /// Close whatever the callback left open and rebuild whatever of this
    /// crate's own boxes it closed, so the walk continues from the state it
    /// left. Returns whether the callback kept the stack balanced.
    fn rebalance(
        &mut self,
        ui: &mut Ui,
        key: u64,
        caller_parent: u64,
        node: u32,
    ) -> Result<bool, DockError> {
        // Unclosed callback boxes cannot outnumber the boxes emitted so far.
        let budget = ui.box_count();
        let mut steps = 0usize;
        let mut pos = self.chain_pos(key, caller_parent, node, ui.parent_key());
        while pos.is_none() && steps < budget && ui.close().is_ok() {
            pos = self.chain_pos(key, caller_parent, node, ui.parent_key());
            steps += 1;
        }
        let rank = pos.unwrap_or_else(|| self.chain_len(node));
        self.reopen_chain(ui, key, node, rank)?;
        Ok(steps == 0 && rank == 0)
    }

    /// Rank of `probe` among the boxes this crate had open around the
    /// content box: 0 content, 1 the leaf, then each ancestor split, the
    /// workspace, and finally whatever the caller had open.
    fn chain_pos(&self, key: u64, caller_parent: u64, node: u32, probe: u64) -> Option<u32> {
        if probe == k_content(key, node) {
            return Some(0);
        }
        let mut rank = 1u32;
        let mut n = node;
        while n != NONE && probe != k_node(key, n) {
            rank += 1;
            n = self.s.node_parent[n as usize];
        }
        if n != NONE {
            return Some(rank);
        }
        if probe == key {
            return Some(rank);
        }
        (probe == caller_parent).then_some(rank + 1)
    }

    /// One past the workspace rank, for when the callback closed everything.
    fn chain_len(&self, node: u32) -> u32 {
        let mut rank = 1u32;
        let mut n = node;
        while n != NONE {
            rank += 1;
            n = self.s.node_parent[n as usize];
        }
        rank + 1
    }

    fn chain_key_at(&self, key: u64, node: u32, rank: u32) -> u64 {
        if rank == 0 {
            return k_content(key, node);
        }
        let mut n = node;
        let mut r = rank;
        while r > 1 && self.s.node_parent[n as usize] != NONE {
            n = self.s.node_parent[n as usize];
            r -= 1;
        }
        if r > 1 { key } else { k_node(key, n) }
    }

    /// Ghost duplicates for the chain entries the callback closed, so the
    /// walk's later closes match again.
    fn reopen_chain(
        &mut self,
        ui: &mut Ui,
        key: u64,
        node: u32,
        rank: u32,
    ) -> Result<(), DockError> {
        let mut r = rank;
        while r > 0 {
            r -= 1;
            ui.open(self.chain_key_at(key, node, r), ghost_spec(), 0)?;
        }
        Ok(())
    }

    fn emit_tabs<F>(
        &mut self,
        ui: &mut Ui,
        ctx: &mut EmitCtx<'_, F>,
        node: u32,
        offset: f32,
        w: f32,
    ) -> Result<Option<u32>, DockError> {
        let mut clicked = None;
        let mut ord: u16 = 0;
        while let Some(slot) = self.panel_at(node, ord) {
            let tab = TabCtx {
                leaf: node,
                slot,
                ord,
                offset,
                w,
            };
            let c = self.emit_tab(ui, ctx, tab)?;
            clicked = clicked.or(c);
            ord += 1;
        }
        Ok(clicked)
    }

    /// The leaf's tab bar scroll for this frame: wheel over the bar moves it,
    /// a selection the bar has not shown yet is brought into view once, and
    /// the stored value is the clamped one the tab rects are built from.
    fn bar_offset(
        &mut self,
        ui: &mut Ui,
        key: u64,
        style: &DockStyle,
        node: u32,
        tab_w: f32,
    ) -> Result<f32, DockError> {
        let bk = k_bar(key, node);
        let mut st = load(ui, bk)?;
        let bar = st.last_rect;
        let view_w = fmax(bar.max[0] - bar.min[0], 0.0);
        let mut offset = f32::from_bits(st.user[0] as u32);
        if !offset.is_finite() {
            offset = 0.0;
        }
        if bar.contains(ui.input().pointer) {
            offset -= ui.input().wheel[1] * style.tab_wheel_step;
        }
        let strip_w = f32::from(self.leaf_len(node)) * tab_w + style.bar_reserve;
        let max = fmax(strip_w - view_w, 0.0);
        offset = fmin(fmax(offset, 0.0), max);
        offset = self.reveal_active_tab(node, offset, view_w, max, tab_w, &mut st);
        st.user[0] = u64::from(offset.to_bits());
        store(ui, bk, st)?;
        Ok(offset)
    }

    /// Scroll the least distance that shows the active tab, once per
    /// selection change. Revealing every frame would pin the bar to the
    /// selection and the wheel could never reach any other tab.
    fn reveal_active_tab(
        &self,
        node: u32,
        offset: f32,
        view_w: f32,
        max: f32,
        tab_w: f32,
        st: &mut WidgetState,
    ) -> f32 {
        // Nothing solved yet. Leave the marker unset so the reveal fires
        // once real geometry lands.
        if view_w <= 0.0 {
            return offset;
        }
        let Some(pid) = self.active_panel(node) else {
            return offset;
        };
        let mark = u64::from(pid) + 1;
        if st.user[1] == mark {
            return offset;
        }
        st.user[1] = mark;
        let Some(slot) = self.find_panel(pid) else {
            return offset;
        };
        let x0 = f32::from(self.s.panel_order[slot as usize]) * tab_w;
        let x1 = x0 + tab_w;
        let o = fmax(fmin(offset, x0), x1 - view_w);
        fmin(fmax(o, 0.0), max)
    }

    fn emit_tab<F>(
        &mut self,
        ui: &mut Ui,
        ctx: &mut EmitCtx<'_, F>,
        tab: TabCtx,
    ) -> Result<Option<u32>, DockError> {
        let pid = self.s.panel_id[tab.slot as usize];
        let k = k_tab(ctx.key, pid);
        let active = tab.slot == self.s.node_active[tab.leaf as usize];
        let hot = ui.peek_hovered(k) || ui.peek_active(k);
        let r = ui.leaf(
            k,
            BoxSpec::leaf().w(SizeSpec::px(tab.w)).h(SizeSpec::grow()),
            tab_fill(ctx.style, active, hot),
        )?;
        ctx.frame.push_tab(FrameTab {
            panel: pid,
            leaf: tab.leaf,
            box_index: ui.last_index(),
            rect: tab_rect(self.s.node_rect[tab.leaf as usize], &tab, ctx.style),
            active,
        });
        Ok((r.clicked && !active).then_some(pid))
    }

    fn emit_preview(
        &mut self,
        ui: &mut Ui,
        key: u64,
        ws: Rect,
        leaf: u32,
        zone: DropZone,
        fill: u32,
    ) -> Result<(), DockError> {
        let r = preview_rect(self.s.node_rect[leaf as usize], zone);
        let off = [r.min[0] - ws.min[0], r.min[1] - ws.min[1]];
        ui.leaf(
            k_preview(key),
            BoxSpec::leaf()
                .w(SizeSpec::px(r.width()))
                .h(SizeSpec::px(r.height()))
                .floating(off),
            fill,
        )?;
        Ok(())
    }
}

/// On-screen rect of a tab, from this crate's own solve and the bar's scroll.
/// The solved box may clip it when it sits past the bar's edge.
fn tab_rect(leaf_rect: Rect, tab: &TabCtx, style: &DockStyle) -> Rect {
    let x = leaf_rect.min[0] + f32::from(tab.ord) * tab.w - tab.offset;
    Rect {
        min: [x, leaf_rect.min[1]],
        max: [x + tab.w, leaf_rect.min[1] + style.tab_h],
    }
}
