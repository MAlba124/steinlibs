//! Shelf packing into a single texture.
//!
//! The atlas is a stack of horizontal shelves. A shelf is opened at the current
//! frontier with a height quantised up to [`SHELF_QUANTUM`], so glyphs whose
//! heights differ by a couple of pixels land on the same shelf instead of each
//! opening its own row. Allocation walks the shelf table and takes the best fit,
//! which is a linear scan over a handful of `u32`s.
//!
//! Shelves never free. Glyph rasterization is monotonic within a size, so a
//! free list would be dead weight. Use [`ShelfPacker::reset`] to start over.

use crate::error::TextError;

/// New shelves round their height up to a multiple of this, so near-identical
/// glyph heights share a row.
pub const SHELF_QUANTUM: u32 = 4;

/// A placed rectangle in atlas pixel coordinates.
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    /// True when the two rectangles share at least one pixel.
    #[must_use]
    pub fn overlaps(self, other: Self) -> bool {
        if self.w == 0 || self.h == 0 || other.w == 0 || other.h == 0 {
            return false;
        }
        self.x < other.x + other.w
            && other.x < self.x + self.w
            && self.y < other.y + other.h
            && other.y < self.y + self.h
    }
}

#[derive(Copy, Clone, Debug)]
struct Shelf {
    y: u32,
    height: u32,
    cursor: u32,
}

/// Packs rectangles into one texture of fixed size.
pub struct ShelfPacker {
    width: u32,
    height: u32,
    padding: u32,
    frontier: u32,
    shelves: Vec<Shelf>,
    used_px: u64,
}

impl ShelfPacker {
    /// `padding` pixels are reserved to the right of and below every rectangle so
    /// a bilinear sampler cannot pull in a neighbour.
    #[must_use]
    pub fn new(width: u32, height: u32, padding: u32) -> Self {
        Self {
            width,
            height,
            padding,
            frontier: 0,
            shelves: Vec::new(),
            used_px: 0,
        }
    }

    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub fn padding(&self) -> u32 {
        self.padding
    }

    #[must_use]
    pub fn shelf_count(&self) -> usize {
        self.shelves.len()
    }

    /// Fraction of the texture handed out, padding included.
    #[must_use]
    pub fn occupancy(&self) -> f32 {
        let total = u64::from(self.width) * u64::from(self.height);
        if total == 0 {
            return 0.0;
        }
        self.used_px as f32 / total as f32
    }

    /// Drops every shelf. Keeps the shelf table's capacity.
    pub fn reset(&mut self) {
        self.shelves.clear();
        self.frontier = 0;
        self.used_px = 0;
    }

    /// Reserves `w` by `h` pixels.
    ///
    /// A zero-area request is legal and yields a zero rectangle at the origin,
    /// which is what a blank glyph such as space wants.
    pub fn alloc(&mut self, w: u32, h: u32) -> Result<Rect, TextError> {
        if w == 0 || h == 0 {
            return Ok(Rect { x: 0, y: 0, w, h });
        }
        let pw = w + self.padding;
        let ph = h + self.padding;
        if pw > self.width || ph > self.height {
            return Err(TextError::GlyphTooLarge {
                width: w,
                height: h,
            });
        }
        // Prefer a tight existing shelf, then a fresh one, then any shelf at all.
        // The last step trades vertical waste for not failing while space remains.
        let tight = self.best_shelf(pw, ph, waste_budget(ph));
        if let Some(i) = tight {
            return Ok(self.place(i, w, h, pw, ph));
        }
        if let Some(i) = self.open_shelf(ph) {
            return Ok(self.place(i, w, h, pw, ph));
        }
        match self.best_shelf(pw, ph, u32::MAX) {
            Some(i) => Ok(self.place(i, w, h, pw, ph)),
            None => Err(TextError::AtlasFull {
                width: w,
                height: h,
            }),
        }
    }

    /// Index of the shelf that fits `pw` by `ph` with the least leftover height,
    /// rejecting shelves that waste more than `budget` rows.
    fn best_shelf(&self, pw: u32, ph: u32, budget: u32) -> Option<usize> {
        self.shelves
            .iter()
            .enumerate()
            .filter(|(_, s)| s.height >= ph && s.cursor + pw <= self.width)
            .map(|(i, s)| (i, s.height - ph))
            .filter(|(_, waste)| *waste <= budget)
            .min_by_key(|(_, waste)| *waste)
            .map(|(i, _)| i)
    }

    /// Opens a shelf at the frontier, quantised if the quantised height still fits.
    fn open_shelf(&mut self, ph: u32) -> Option<usize> {
        let remaining = self.height - self.frontier;
        if ph > remaining {
            return None;
        }
        let quantised = ph.div_ceil(SHELF_QUANTUM) * SHELF_QUANTUM;
        let height = if quantised <= remaining {
            quantised
        } else {
            ph
        };
        self.shelves.push(Shelf {
            y: self.frontier,
            height,
            cursor: 0,
        });
        self.frontier += height;
        Some(self.shelves.len() - 1)
    }

    fn place(&mut self, i: usize, w: u32, h: u32, pw: u32, ph: u32) -> Rect {
        let s = &mut self.shelves[i];
        let rect = Rect {
            x: s.cursor,
            y: s.y,
            w,
            h,
        };
        s.cursor += pw;
        self.used_px += u64::from(pw) * u64::from(ph);
        rect
    }
}

/// How many rows of shelf height a rectangle may waste before a new shelf is
/// worth opening. One quantum plus a quarter keeps small glyphs off tall shelves.
fn waste_budget(ph: u32) -> u32 {
    SHELF_QUANTUM + ph / 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_area_is_free() {
        let mut p = ShelfPacker::new(64, 64, 1);
        let r = p.alloc(0, 0).expect("zero area");
        assert_eq!(r, Rect::default());
        assert_eq!(p.shelf_count(), 0);
    }

    #[test]
    fn similar_heights_share_a_shelf() {
        let mut p = ShelfPacker::new(256, 256, 1);
        let a = p.alloc(6, 12).expect("fits");
        let b = p.alloc(6, 13).expect("fits");
        let c = p.alloc(6, 11).expect("fits");
        assert_eq!((a.y, b.y, c.y), (0, 0, 0));
        assert_eq!(p.shelf_count(), 1);
    }

    #[test]
    fn a_much_taller_glyph_opens_a_new_shelf() {
        let mut p = ShelfPacker::new(256, 256, 1);
        let a = p.alloc(6, 8).expect("fits");
        let b = p.alloc(6, 40).expect("fits");
        assert_ne!(a.y, b.y);
        assert_eq!(p.shelf_count(), 2);
    }

    #[test]
    fn nothing_overlaps_and_nothing_escapes() {
        let mut p = ShelfPacker::new(128, 128, 1);
        let mut placed: Vec<Rect> = Vec::new();
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..400 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let w = 1 + (state % 17) as u32;
            let h = 1 + ((state >> 20) % 21) as u32;
            let r = match p.alloc(w, h) {
                Ok(r) => r,
                Err(_) => break,
            };
            assert!(r.x + r.w <= 128 && r.y + r.h <= 128, "escaped: {r:?}");
            let clear = placed.iter().all(|prev| !r.overlaps(*prev));
            assert!(clear, "{r:?} overlaps an earlier rectangle");
            placed.push(r);
        }
        assert!(placed.len() > 50, "packer gave up far too early");
    }

    #[test]
    fn a_full_atlas_reports_failure() {
        let mut p = ShelfPacker::new(32, 32, 0);
        let mut count = 0;
        let err = loop {
            match p.alloc(16, 16) {
                Ok(_) => count += 1,
                Err(e) => break e,
            }
            assert!(count <= 4, "packer overflowed a 32x32 atlas");
        };
        assert_eq!(
            err,
            TextError::AtlasFull {
                width: 16,
                height: 16
            }
        );
        assert_eq!(count, 4);
    }

    #[test]
    fn an_oversized_glyph_is_rejected_outright() {
        let mut p = ShelfPacker::new(32, 32, 1);
        assert_eq!(
            p.alloc(64, 8),
            Err(TextError::GlyphTooLarge {
                width: 64,
                height: 8
            })
        );
    }

    #[test]
    fn reset_reclaims_everything() {
        let mut p = ShelfPacker::new(64, 64, 1);
        p.alloc(30, 30).expect("fits");
        p.reset();
        assert_eq!(p.shelf_count(), 0);
        assert_eq!(p.occupancy(), 0.0);
        assert_eq!(p.alloc(30, 30).expect("fits").y, 0);
    }
}
