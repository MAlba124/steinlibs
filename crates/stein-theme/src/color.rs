//! Colour arithmetic on ARGB `u32`, `0xAARRGGBB`, the form
//! `stein_ui::draw::Quad` carries.
//!
//! Weights are out of 255 so derivation is integer only and the same on
//! every machine.

/// An opaque colour from its channels.
#[inline]
pub const fn argb(r: u8, g: u8, b: u8) -> u32 {
    0xff00_0000 | ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// `a` moved `w` of 255 of the way to `b`, every channel alpha included.
pub const fn mix(a: u32, b: u32, w: u8) -> u32 {
    let w = w as u32;
    let mut out = 0u32;
    let mut shift = 0;
    while shift < 32 {
        let x = (a >> shift) & 0xff;
        let y = (b >> shift) & 0xff;
        // Rounded, so mixing with weight 255 lands exactly on `b`.
        let v = (x * (255 - w) + y * w + 127) / 255;
        out |= v << shift;
        shift += 8;
    }
    out
}

/// `c` with its alpha replaced.
#[inline]
pub const fn alpha(c: u32, a: u8) -> u32 {
    (c & 0x00ff_ffff) | ((a as u32) << 24)
}

/// Rough perceived brightness, 0 to 255, Rec. 601 on the raw bytes.
pub const fn luma(c: u32) -> u8 {
    let r = (c >> 16) & 0xff;
    let g = (c >> 8) & 0xff;
    let b = c & 0xff;
    ((299 * r + 587 * g + 114 * b) / 1000) as u8
}

/// Whether `c` reads as a light colour.
#[inline]
pub const fn is_light(c: u32) -> bool {
    luma(c) >= 128
}

/// `c` pushed `w` of 255 further from the middle: darker when dark,
/// lighter when light. What a recessed bar under a surface wants in either
/// kind of theme.
pub const fn deepen(c: u32, w: u8) -> u32 {
    let end = if is_light(c) {
        0xffff_ffff
    } else {
        0xff00_0000
    };
    alpha(mix(c, end, w), (c >> 24) as u8)
}

/// Text that reads on top of `c`.
pub const fn on(c: u32) -> u32 {
    if luma(c) >= 170 {
        0xff10_1010
    } else {
        0xffff_ffff
    }
}

/// `#rgb`, `#rrggbb`, `#aarrggbb`, with `0x` accepted in place of `#`.
/// Opaque unless eight digits say otherwise.
pub fn parse_hex(v: &[u8]) -> Option<u32> {
    let hex = v
        .strip_prefix(b"#")
        .or_else(|| v.strip_prefix(b"0x"))
        .or_else(|| v.strip_prefix(b"0X"))?;
    let mut n = 0u32;
    for &c in hex {
        n = (n << 4) | (c as char).to_digit(16)?;
    }
    match hex.len() {
        3 => {
            let d = |s: u32| ((n >> s) & 0xf) as u8 * 17;
            Some(argb(d(8), d(4), d(0)))
        }
        6 => Some(0xff00_0000 | n),
        8 => Some(n),
        _ => None,
    }
}
