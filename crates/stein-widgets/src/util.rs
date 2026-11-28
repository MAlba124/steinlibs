//! Shared float math, axis plumbing and state stashing.

use stein_layout::{Axis, SizeSpec};
use stein_ui::{Ui, WidgetState};

use crate::WidgetError;

#[inline]
pub(crate) fn fmax(a: f32, b: f32) -> f32 {
    if a > b { a } else { b }
}

#[inline]
pub(crate) fn fmin(a: f32, b: f32) -> f32 {
    if a < b { a } else { b }
}

#[inline]
pub(crate) fn fabs(v: f32) -> f32 {
    if v < 0.0 { -v } else { v }
}

#[inline]
pub(crate) fn clampf(v: f32, lo: f32, hi: f32) -> f32 {
    fmin(fmax(v, lo), hi)
}

#[inline]
pub(crate) fn axis_index(axis: Axis) -> usize {
    match axis {
        Axis::X => 0,
        Axis::Y => 1,
    }
}

#[inline]
pub(crate) fn axis_of(a: usize) -> Axis {
    if a == 0 { Axis::X } else { Axis::Y }
}

#[inline]
pub(crate) fn cross_of(a: usize) -> Axis {
    if a == 0 { Axis::Y } else { Axis::X }
}

/// Sizes as a `[w, h]` pair from an along-axis and a cross-axis spec, so
/// widgets can be written once for both orientations.
#[inline]
pub(crate) fn sized(a: usize, along: SizeSpec, cross: SizeSpec) -> [SizeSpec; 2] {
    if a == 0 {
        [along, cross]
    } else {
        [cross, along]
    }
}

/// Copy in, mutate, copy out, which sidesteps holding a borrow across
/// further Ui calls. Asking for state is also what opts a key into geometry
/// recording, so a load is a subscription too.
pub(crate) fn load(ui: &mut Ui, key: u64) -> Result<WidgetState, WidgetError> {
    ui.state(key).map(|s| *s).ok_or(WidgetError::StateFull)
}

pub(crate) fn store(ui: &mut Ui, key: u64, st: WidgetState) -> Result<(), WidgetError> {
    match ui.state(key) {
        Some(s) => {
            *s = st;
            Ok(())
        }
        None => Err(WidgetError::StateFull),
    }
}
