//! Widgets built on stein-ui's public surface alone.
//!
//! Everything here follows one shape. The previous frame's solve is read at
//! begin time from `WidgetState`, where `end_frame` records it for every key
//! that asked for state, interaction is resolved from peeks against it, and
//! boxes are emitted. Nothing runs after `end_frame`. A frame emitted against
//! geometry the solve then contradicted is always damaged, and a damaged
//! present is followed by another build, which settles it.

#![no_std]

#[cfg(test)]
extern crate std;

mod util;

pub mod scroll;
pub mod split;
pub mod table;
pub mod tabs;
pub mod textview;
pub mod vlist;

pub use scroll::{ScrollArea, ScrollStyle};
pub use split::{Splitter, SplitterStyle};
pub use table::{Column, ColumnWidth, MAX_COLUMNS, Sort, Table, TableStyle};
pub use tabs::{TabResponse, Tabs, TabsResult, TabsStyle};
pub use textview::{Caret, LineRange, SelectionRects, TextMetrics, TextView, TextViewStyle};
pub use vlist::{VirtualList, VirtualRange};

use stein_ui::UiError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidgetError {
    Ui(UiError),
    /// The keyed state table is full, so persistence would silently vanish.
    StateFull,
    RowHeightNotPositive,
    BarSizeNotPositive,
    DividerNotPositive,
    MinPaneNegative,
    InitialFractionOutOfRange,
    TabHeightNotPositive,
    /// Zero columns, or more than [`table::MAX_COLUMNS`].
    ColumnCountOutOfRange,
    ColumnOutOfRange,
    /// A second `row` before the first was closed.
    RowAlreadyOpen,
    /// `end_row` with no row open.
    NoRowOpen,
    /// A zero or negative line height or character advance.
    TextMetricsNotPositive,
}

impl From<UiError> for WidgetError {
    fn from(e: UiError) -> Self {
        WidgetError::Ui(e)
    }
}

#[cfg(test)]
mod tests;
