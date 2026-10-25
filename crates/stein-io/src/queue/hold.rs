//! Buffers a group lent the kernel, by bid. Open addressing, linear probing.
//!
//! A buffer is held from [`super::Queue::provide`] until a completion hands its bid back,
//! whatever order those come in. Sized at twice the ring so probes stay short, and kept
//! across a group index's reuse so a steady add and remove does not allocate.

#[derive(Clone, Copy, Debug)]
struct Hold {
    addr: usize,
    len: u32,
    bid: u16,
    used: bool,
}

const EMPTY: Hold = Hold {
    addr: 0,
    len: 0,
    bid: 0,
    used: false,
};

pub(super) struct Holds {
    table: Box<[Hold]>,
    /// Probe mask, the table in use is `mask + 1` long.
    mask: usize,
    /// Fibonacci hash shift for the table in use.
    shift: u32,
    len: u32,
}

impl Holds {
    /// No room until [`Holds::reset`]. Allocates nothing.
    pub(super) fn new() -> Holds {
        Holds {
            table: Box::new([]),
            mask: 0,
            shift: 31,
            len: 0,
        }
    }

    /// Empty, with room for a ring of `entries`. Allocates only to grow.
    pub(super) fn reset(&mut self, entries: u16) {
        let cap = (entries as usize * 2).max(2);
        if self.table.len() < cap {
            self.table = vec![EMPTY; cap].into_boxed_slice();
        }
        self.table[..cap].fill(EMPTY);
        self.mask = cap - 1;
        self.shift = 32 - cap.trailing_zeros();
        self.len = 0;
    }

    /// Forget every buffer, for a group whose ops all ended.
    pub(super) fn clear(&mut self) {
        let n = self.mask + 1;
        if let Some(t) = self.table.get_mut(..n) {
            t.fill(EMPTY);
        }
        self.len = 0;
    }

    pub(super) fn len(&self) -> usize {
        self.len as usize
    }

    fn home(&self, bid: u16) -> usize {
        ((bid as u32).wrapping_mul(0x9e37_79b9) >> self.shift) as usize & self.mask
    }

    /// False if `bid` is held already or there is no room.
    pub(super) fn insert(&mut self, bid: u16, addr: usize, len: u32) -> bool {
        let cap = self.mask + 1;
        if self.table.len() < cap || self.len as usize * 2 >= cap {
            return false;
        }
        let mut i = self.home(bid);
        while let Some(h) = self.table.get(i).filter(|h| h.used) {
            if h.bid == bid {
                return false;
            }
            i = (i + 1) & self.mask;
        }
        self.table[i] = Hold {
            addr,
            len,
            bid,
            used: true,
        };
        self.len += 1;
        true
    }

    /// Hand `bid` back. False if it was not held.
    pub(super) fn remove(&mut self, bid: u16) -> bool {
        if self.len == 0 {
            return false;
        }
        let mut i = self.home(bid);
        loop {
            match self.table.get(i) {
                Some(h) if h.used && h.bid == bid => break,
                Some(h) if h.used => i = (i + 1) & self.mask,
                _ => return false,
            }
        }
        self.shift_back(i);
        self.len -= 1;
        true
    }

    /// Backward shift delete, so no tombstones lengthen later probes.
    fn shift_back(&mut self, mut hole: usize) {
        let mut j = hole;
        loop {
            j = (j + 1) & self.mask;
            let Some(h) = self.table.get(j).copied().filter(|h| h.used) else {
                break;
            };
            let k = self.home(h.bid);
            // Movable unless its home lies cyclically in (hole, j].
            let stays = if hole <= j {
                hole < k && k <= j
            } else {
                hole < k || k <= j
            };
            if !stays {
                self.table[hole] = h;
                hole = j;
            }
        }
        self.table[hole] = EMPTY;
    }

    /// First held range overlapping `[lo, hi)`, as address and length.
    pub(super) fn hit(&self, lo: usize, hi: usize) -> Option<(usize, usize)> {
        if self.len == 0 {
            return None;
        }
        let n = self.mask + 1;
        self.table.get(..n)?.iter().find_map(|h| {
            let (p, l) = (h.addr, h.len as usize);
            (h.used && l > 0 && p < hi && lo < p.saturating_add(l)).then_some((p, l))
        })
    }
}

#[cfg(test)]
mod tests;
