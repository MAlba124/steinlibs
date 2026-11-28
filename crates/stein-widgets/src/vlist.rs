//! Uniform row virtualization.
//!
//! Only rows intersecting the viewport become boxes. Spacer leaves stand in
//! for everything scrolled away, so the solve's content extent stays exact
//! and the scroll area's clamp needs no declared content size. Row keys are
//! seeded by caller supplied ids so selection and drag state survive a
//! re-sort. This is what makes a 100k row list emit a couple of dozen boxes.

use stein_layout::{BoxSpec, SizeSpec};
use stein_ui::key::{child, hash_str, seeded};
use stein_ui::{Response, Ui};

use crate::WidgetError;
use crate::util::fmax;

/// Rows `first..end` intersect the viewport.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VirtualRange {
    pub first: u32,
    pub end: u32,
}

impl VirtualRange {
    pub fn len(&self) -> u32 {
        self.end.saturating_sub(self.first)
    }
    pub fn is_empty(&self) -> bool {
        self.end <= self.first
    }
}

/// Vertical list of uniform height rows. Emit inside an open scroll viewport.
pub struct VirtualList {
    key: u64,
    row_h: f32,
    count: u32,
    range: VirtualRange,
}

impl VirtualList {
    /// Emits the top spacer and computes the visible range. The caller then
    /// emits one row per index in the range, looking up only that data.
    pub fn begin(
        ui: &mut Ui,
        key: u64,
        offset: f32,
        view_len: f32,
        row_count: u32,
        row_height: f32,
    ) -> Result<VirtualList, WidgetError> {
        if row_height <= 0.0 {
            return Err(WidgetError::RowHeightNotPositive);
        }
        let range = visible_range(offset, view_len, row_count, row_height);
        let top = range.first as f32 * row_height;
        ui.leaf(
            child(key, hash_str("vlist.top")),
            BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::px(top)),
            0,
        )?;
        Ok(VirtualList {
            key,
            row_h: row_height,
            count: row_count,
            range,
        })
    }

    pub fn range(&self) -> VirtualRange {
        self.range
    }

    /// Key for a row, seeded by the caller's stable id rather than the row's
    /// position, so state stays with the row through a re-sort.
    pub fn row_key(&self, id: u64) -> u64 {
        seeded(self.key, hash_str("vlist.row"), id)
    }

    /// A plain filled row. Callers wanting richer rows emit their own boxes
    /// under [`VirtualList::row_key`].
    pub fn row(&self, ui: &mut Ui, id: u64, fill: u32) -> Result<Response, WidgetError> {
        let r = ui.leaf(
            self.row_key(id),
            BoxSpec::leaf()
                .w(SizeSpec::grow())
                .h(SizeSpec::px(self.row_h)),
            fill,
        )?;
        Ok(r)
    }

    /// Emits the bottom spacer and returns the range that was visible.
    pub fn end(self, ui: &mut Ui) -> Result<VirtualRange, WidgetError> {
        let below = self.count.saturating_sub(self.range.end) as f32 * self.row_h;
        ui.leaf(
            child(self.key, hash_str("vlist.bottom")),
            BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::px(below)),
            0,
        )?;
        Ok(self.range)
    }
}

fn visible_range(offset: f32, view_len: f32, count: u32, h: f32) -> VirtualRange {
    if view_len <= 0.0 || count == 0 {
        return VirtualRange::default();
    }
    let off = fmax(offset, 0.0);
    // Truncating casts are floor for non negative values, and core has no
    // f32 floor without libm.
    let first = ((off / h) as u32).min(count);
    let stop = off + view_len;
    let mut end = (stop / h) as u32;
    if (end as f32) * h < stop {
        end += 1;
    }
    VirtualRange {
        first,
        end: end.clamp(first, count),
    }
}
