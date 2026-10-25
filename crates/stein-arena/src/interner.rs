//! Strings in, dense ids out, each distinct string stored once.
//!
//! Ids are assigned in first-sight order, independent of table layout.

use crate::{Arena, ArenaError, Span};

/// Marks an empty probe slot. Also the id ceiling.
const EMPTY: u32 = u32::MAX;

pub struct Interner {
    bytes: Arena,
    /// Indexed by id.
    spans: Vec<Span>,
    /// Open-addressed probe table of ids, power of two length.
    table: Vec<u32>,
    /// Indexed by id, so a rehash never re-reads the bytes.
    hashes: Vec<u64>,
}

impl Interner {
    /// An interner whose byte arena reserves `cap`.
    pub fn reserve(cap: usize) -> Result<Interner, ArenaError> {
        Ok(Interner {
            bytes: Arena::reserve(cap)?,
            spans: Vec::new(),
            table: vec![EMPTY; 64],
            hashes: Vec::new(),
        })
    }

    /// 1 GiB byte reservation.
    pub fn new() -> Result<Interner, ArenaError> {
        Interner::reserve(1 << 30)
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// The id for `s`, interned on first sight.
    pub fn intern(&mut self, s: &str) -> Result<u32, ArenaError> {
        let h = fnv(s.as_bytes());
        let (at, found) = self.probe(s, h);
        if let Some(id) = found {
            return Ok(id);
        }
        if self.spans.len() as u32 >= EMPTY {
            return Err(ArenaError::Full {
                asked: s.len(),
                left: 0,
            });
        }
        let id = self.spans.len() as u32;
        let span = self.bytes.push_bytes(s.as_bytes())?;
        self.spans.push(span);
        self.hashes.push(h);
        self.table[at] = id;
        self.maybe_grow();
        Ok(id)
    }

    /// Unknown ids read empty.
    pub fn get(&self, id: u32) -> &str {
        let bytes = self
            .spans
            .get(id as usize)
            .map(|s| self.bytes.bytes(*s))
            .unwrap_or(&[]);
        // Never fails, the bytes came from `&str`.
        core::str::from_utf8(bytes).unwrap_or("")
    }

    /// Slot of `s` with its id, or the empty slot to fill.
    fn probe(&self, s: &str, h: u64) -> (usize, Option<u32>) {
        let mask = self.table.len() - 1;
        let mut at = (h as usize) & mask;
        loop {
            match self.table[at] {
                EMPTY => return (at, None),
                id if self.hashes[id as usize] == h && self.get(id) == s => return (at, Some(id)),
                _ => at = (at + 1) & mask,
            }
        }
    }

    /// Keep the table under two-thirds full.
    fn maybe_grow(&mut self) {
        if self.spans.len() * 3 < self.table.len() * 2 {
            return;
        }
        let cap = self.table.len() * 2;
        let mask = cap - 1;
        let mut table = vec![EMPTY; cap];
        for (id, h) in self.hashes.iter().enumerate() {
            let at = free_slot(&table, mask, *h);
            table[at] = id as u32;
        }
        self.table = table;
    }
}

/// The first empty slot on `h`'s probe path.
fn free_slot(table: &[u32], mask: usize, h: u64) -> usize {
    let mut at = (h as usize) & mask;
    while table[at] != EMPTY {
        at = (at + 1) & mask;
    }
    at
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests;
