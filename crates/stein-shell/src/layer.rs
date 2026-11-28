//! A surface on the desktop's layers rather than a window, zwlr_layer_shell_v1.
//!
//! What bars, launchers, notifications and wallpapers sit on. The compositor
//! places it from anchors and margins instead of tiling it, and a positive
//! exclusive zone keeps windows off the strip it covers.
//!
//! The spec is plain data the scene hands over once. The checks here are pure
//! so the protocol errors the compositor would raise are caught in tests
//! rather than as a dead connection.

use std::fmt;

/// Which stack the surface sits in, bottom to top.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Level {
    Background,
    Bottom,
    /// Above windows, below fullscreen ones. Where a bar goes.
    #[default]
    Top,
    /// Above everything, fullscreen windows included.
    Overlay,
}

/// Edges of the output the surface is pinned to, as the protocol's bits.
///
/// Two opposite edges stretch the surface between them when its size on that
/// axis is zero. None centres it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Anchor(pub u8);

impl Anchor {
    pub const NONE: Self = Self(0);
    pub const TOP: Self = Self(1);
    pub const BOTTOM: Self = Self(2);
    pub const LEFT: Self = Self(4);
    pub const RIGHT: Self = Self(8);

    pub const fn has(self, edge: Self) -> bool {
        self.0 & edge.0 == edge.0
    }
}

impl core::ops::BitOr for Anchor {
    type Output = Self;
    fn bitor(self, o: Self) -> Self {
        Self(self.0 | o.0)
    }
}

/// Whether the surface takes the keyboard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Keys {
    /// Never focused. A bar.
    #[default]
    None,
    /// Holds the keyboard while mapped on the top or overlay layer. A launcher.
    Exclusive,
    /// Focused like a window, when clicked. Needs version 4, older
    /// compositors get `Exclusive`.
    OnDemand,
}

/// Everything the compositor needs to place a layer surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layer {
    pub level: Level,
    pub anchor: Anchor,
    /// Pixels, zero on an axis to take the compositor's size, which needs
    /// both edges of that axis anchored.
    pub size: [u32; 2],
    /// Pixels from the anchored edge windows keep clear of. Zero to be moved
    /// clear of other zones, -1 to cover them.
    pub exclusive: i32,
    /// Top, right, bottom, left.
    pub margin: [i32; 4],
    pub keys: Keys,
    /// What the compositor calls this surface in its rules, "panel" say.
    pub namespace: &'static str,
}

impl Default for Layer {
    fn default() -> Self {
        Self {
            level: Level::Top,
            anchor: Anchor::NONE,
            size: [0, 0],
            exclusive: 0,
            margin: [0; 4],
            keys: Keys::None,
            namespace: "stein",
        }
    }
}

/// A spec the compositor would reject with a protocol error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerError {
    /// Zero width without both left and right anchored.
    ZeroWidth,
    /// Zero height without both top and bottom anchored.
    ZeroHeight,
    /// Anchor bits outside the four edges.
    Anchor(u8),
}

impl fmt::Display for LayerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroWidth => write!(f, "layer width 0 needs left and right anchors"),
            Self::ZeroHeight => write!(f, "layer height 0 needs top and bottom anchors"),
            Self::Anchor(a) => write!(f, "layer anchor bits {a:#x} name no edge"),
        }
    }
}

impl std::error::Error for LayerError {}

/// Check a spec before any of it goes on the wire.
pub fn check(spec: &Layer) -> Result<(), LayerError> {
    let a = spec.anchor;
    if a.0 & !0xf != 0 {
        return Err(LayerError::Anchor(a.0));
    }
    if spec.size[0] == 0 && !a.has(Anchor::LEFT | Anchor::RIGHT) {
        return Err(LayerError::ZeroWidth);
    }
    if spec.size[1] == 0 && !a.has(Anchor::TOP | Anchor::BOTTOM) {
        return Err(LayerError::ZeroHeight);
    }
    Ok(())
}

/// The size to draw at after a configure. A zero axis from the compositor
/// leaves it to us, which means what the spec asked for.
pub fn configured(spec: &Layer, width: u32, height: u32) -> [u32; 2] {
    let pick = |got: u32, want: u32| if got == 0 { want.max(1) } else { got };
    [pick(width, spec.size[0]), pick(height, spec.size[1])]
}

/// The keyboard mode a compositor at `version` accepts.
pub fn keys_for(keys: Keys, version: u32) -> Keys {
    match keys {
        Keys::OnDemand if version < 4 => Keys::Exclusive,
        k => k,
    }
}

/// A full-width bar `height` pixels tall on the top or bottom edge, keeping
/// windows clear of it.
pub fn bar(height: u32, bottom: bool, namespace: &'static str) -> Layer {
    let edge = if bottom { Anchor::BOTTOM } else { Anchor::TOP };
    Layer {
        level: Level::Top,
        anchor: edge | Anchor::LEFT | Anchor::RIGHT,
        size: [0, height],
        exclusive: height.min(i32::MAX as u32) as i32,
        namespace,
        ..Layer::default()
    }
}

#[cfg(test)]
mod tests;
