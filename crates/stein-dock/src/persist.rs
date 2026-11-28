//! Layout persistence: a little-endian, versioned preorder stream.
//!
//! `[magic 4][version u16][has_root u8]` then, when a root exists, one node:
//! a split is `[1][axis u8][ratio f32][a...][b...]`, a leaf is
//! `[0][count u16][panel u32 x count][active u16]`. Anything unrecognised is
//! refused, and the whole buffer is validated before the tree is touched, so
//! a failed restore leaves the previous layout intact.

use stein_layout::Axis;

use crate::{DockError, DockTree, NONE, NodeKind, clamp_ratio};

const MAGIC: [u8; 4] = *b"SDCK";
const VERSION: u16 = 1;
const TAG_LEAF: u8 = 0;
const TAG_SPLIT: u8 = 1;
/// Hostile input could otherwise recurse the validator off the stack.
const MAX_DEPTH: u32 = 64;

struct W<'o> {
    out: &'o mut [u8],
    pos: usize,
}

impl W<'_> {
    fn put(&mut self, b: &[u8]) -> Result<(), DockError> {
        let end = self.pos + b.len();
        if end > self.out.len() {
            return Err(DockError::OutputTooSmall);
        }
        self.out[self.pos..end].copy_from_slice(b);
        self.pos = end;
        Ok(())
    }
}

struct R<'b> {
    b: &'b [u8],
    pos: usize,
}

impl R<'_> {
    fn u8(&mut self) -> Result<u8, DockError> {
        let v = *self.b.get(self.pos).ok_or(DockError::Truncated)?;
        self.pos += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16, DockError> {
        Ok(u16::from_le_bytes([self.u8()?, self.u8()?]))
    }
    fn u32(&mut self) -> Result<u32, DockError> {
        Ok(u32::from_le_bytes([
            self.u8()?,
            self.u8()?,
            self.u8()?,
            self.u8()?,
        ]))
    }
    fn f32(&mut self) -> Result<f32, DockError> {
        Ok(f32::from_bits(self.u32()?))
    }
}

struct Counts {
    nodes: usize,
    panels: usize,
}

fn validate_node(r: &mut R<'_>, c: &mut Counts, depth: u32) -> Result<(), DockError> {
    if depth > MAX_DEPTH {
        return Err(DockError::Corrupt);
    }
    c.nodes += 1;
    match r.u8()? {
        TAG_LEAF => validate_leaf(r, c),
        TAG_SPLIT => {
            if r.u8()? > 1 {
                return Err(DockError::Corrupt);
            }
            if !r.f32()?.is_finite() {
                return Err(DockError::Corrupt);
            }
            validate_node(r, c, depth + 1)?;
            validate_node(r, c, depth + 1)
        }
        _ => Err(DockError::Corrupt),
    }
}

fn validate_leaf(r: &mut R<'_>, c: &mut Counts) -> Result<(), DockError> {
    let n = r.u16()? as usize;
    if n == 0 {
        return Err(DockError::Corrupt);
    }
    c.panels += n;
    for _ in 0..n {
        r.u32()?;
    }
    if r.u16()? as usize >= n {
        return Err(DockError::Corrupt);
    }
    Ok(())
}

/// Walks an already validated body yielding each panel id with its position,
/// so duplicates can be found by rescanning instead of allocating a set.
struct IdScan<'b> {
    r: R<'b>,
    in_leaf: u16,
}

impl<'b> IdScan<'b> {
    fn new(body: &'b [u8]) -> Self {
        IdScan {
            r: R { b: body, pos: 0 },
            in_leaf: 0,
        }
    }

    fn next_id(&mut self) -> Option<(usize, u32)> {
        while self.in_leaf == 0 {
            self.enter_next_leaf()?;
        }
        self.in_leaf -= 1;
        let at = self.r.pos;
        let id = self.r.u32().ok()?;
        if self.in_leaf == 0 {
            let _ = self.r.u16();
        }
        Some((at, id))
    }

    fn enter_next_leaf(&mut self) -> Option<()> {
        match self.r.u8().ok()? {
            TAG_LEAF => self.in_leaf = self.r.u16().ok()?,
            TAG_SPLIT => self.skip_split_header()?,
            _ => return None,
        }
        Some(())
    }

    fn skip_split_header(&mut self) -> Option<()> {
        self.r.u8().ok()?;
        self.r.u32().ok()?;
        Some(())
    }
}

/// Whether `id` already occurred before byte offset `at` in the stream.
fn seen_before(body: &[u8], at: usize, id: u32) -> bool {
    let mut scan = IdScan::new(body);
    while let Some((at2, id2)) = scan.next_id() {
        if at2 >= at {
            return false;
        }
        if id2 == id {
            return true;
        }
    }
    false
}

fn check_dups(body: &[u8]) -> Result<(), DockError> {
    let mut outer = IdScan::new(body);
    while let Some((at, id)) = outer.next_id() {
        if seen_before(body, at, id) {
            return Err(DockError::Corrupt);
        }
    }
    Ok(())
}

fn axis_byte(a: Axis) -> u8 {
    match a {
        Axis::X => 0,
        Axis::Y => 1,
    }
}

fn axis_from(b: u8) -> Axis {
    if b == 1 { Axis::Y } else { Axis::X }
}

impl DockTree<'_> {
    /// Serialise the tree. Returns the bytes written.
    pub fn to_bytes(&self, out: &mut [u8]) -> Result<usize, DockError> {
        let mut w = W { out, pos: 0 };
        w.put(&MAGIC)?;
        w.put(&VERSION.to_le_bytes())?;
        w.put(&[u8::from(self.root != NONE)])?;
        if self.root != NONE {
            self.write_node(&mut w, self.root)?;
        }
        Ok(w.pos)
    }

    fn write_node(&self, w: &mut W<'_>, node: u32) -> Result<(), DockError> {
        let u = node as usize;
        if self.s.node_kind[u] == NodeKind::Split {
            w.put(&[TAG_SPLIT, axis_byte(self.s.node_axis[u])])?;
            w.put(&self.s.node_ratio[u].to_le_bytes())?;
            self.write_node(w, self.s.node_a[u])?;
            return self.write_node(w, self.s.node_b[u]);
        }
        self.write_leaf(w, node)
    }

    fn write_leaf(&self, w: &mut W<'_>, leaf: u32) -> Result<(), DockError> {
        let n = self.leaf_len(leaf);
        w.put(&[TAG_LEAF])?;
        w.put(&n.to_le_bytes())?;
        for ord in 0..n {
            self.write_panel(w, leaf, ord)?;
        }
        w.put(&self.active_ord(leaf, n).to_le_bytes())
    }

    fn write_panel(&self, w: &mut W<'_>, leaf: u32, ord: u16) -> Result<(), DockError> {
        let Some(slot) = self.panel_at(leaf, ord) else {
            return Err(DockError::Corrupt);
        };
        w.put(&self.s.panel_id[slot as usize].to_le_bytes())
    }

    fn active_ord(&self, leaf: u32, n: u16) -> u16 {
        let active = self.s.node_active[leaf as usize];
        (0..n)
            .find(|ord| self.panel_at(leaf, *ord) == Some(active))
            .unwrap_or(0)
    }

    /// Replace the tree with a serialised one. The buffer is validated in
    /// full first: structure, counts against capacity, duplicate panel ids.
    /// On any error the tree is untouched.
    pub fn from_bytes(&mut self, bytes: &[u8]) -> Result<(), DockError> {
        let mut r = R { b: bytes, pos: 0 };
        let mut magic = [0u8; 4];
        for m in &mut magic {
            *m = r.u8()?;
        }
        if magic != MAGIC {
            return Err(DockError::BadMagic);
        }
        if r.u16()? != VERSION {
            return Err(DockError::BadVersion);
        }
        let has_root = match r.u8()? {
            0 => false,
            1 => true,
            _ => return Err(DockError::Corrupt),
        };
        let body_start = r.pos;
        let mut counts = Counts {
            nodes: 0,
            panels: 0,
        };
        if has_root {
            validate_node(&mut r, &mut counts, 0)?;
        }
        if r.pos != bytes.len() {
            return Err(DockError::Corrupt);
        }
        if counts.nodes > self.node_capacity() {
            return Err(DockError::NodesFull);
        }
        if counts.panels > self.panel_capacity() {
            return Err(DockError::PanelsFull);
        }
        check_dups(&bytes[body_start..])?;
        self.reset();
        if has_root {
            let mut r = R {
                b: bytes,
                pos: body_start,
            };
            // Cannot refuse after validation, so the tree is never half built.
            self.root = self.build_node(&mut r)?;
        }
        Ok(())
    }

    fn build_node(&mut self, r: &mut R<'_>) -> Result<u32, DockError> {
        match r.u8()? {
            TAG_LEAF => self.build_leaf(r),
            TAG_SPLIT => self.build_split(r),
            _ => Err(DockError::Corrupt),
        }
    }

    fn build_split(&mut self, r: &mut R<'_>) -> Result<u32, DockError> {
        let axis = axis_from(r.u8()?);
        let ratio = r.f32()?;
        let node = self.alloc_node(NodeKind::Split)?;
        self.s.node_axis[node as usize] = axis;
        self.s.node_ratio[node as usize] = clamp_ratio(ratio);
        let a = self.build_node(r)?;
        let b = self.build_node(r)?;
        self.s.node_a[node as usize] = a;
        self.s.node_b[node as usize] = b;
        self.s.node_parent[a as usize] = node;
        self.s.node_parent[b as usize] = node;
        Ok(node)
    }

    fn build_leaf(&mut self, r: &mut R<'_>) -> Result<u32, DockError> {
        let n = r.u16()?;
        let leaf = self.alloc_node(NodeKind::Leaf)?;
        for ord in 0..n {
            let id = r.u32()?;
            self.alloc_panel(id, leaf, ord)?;
        }
        let active = r.u16()?;
        // Validation bounds `active`, so the fallback is never reached.
        let slot = self
            .panel_at(leaf, active)
            .or_else(|| self.panel_at(leaf, 0));
        if let Some(s) = slot {
            self.s.node_active[leaf as usize] = s;
        }
        Ok(leaf)
    }
}
