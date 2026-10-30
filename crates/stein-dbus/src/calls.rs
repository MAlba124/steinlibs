//! Calls in flight, keyed by serial, each with a deadline.
//!
//! A reply names the call it answers by serial; this table turns that back
//! into whatever the caller attached, and turns silence into a timeout so
//! a peer that never answers costs a slot for a while and no more. Fixed
//! slots from the caller, a linear scan: a desktop client has a handful of
//! calls out at once.

/// Milliseconds since an epoch the caller chose.
pub type Millis = u64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pending {
    pub serial: u32,
    /// Whatever the caller wants back: an item index, a request kind.
    pub tag: u64,
    pub deadline: Millis,
}

/// Every slot is taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallsFull;

pub struct Calls<'a> {
    slots: &'a mut [Pending],
    len: usize,
}

impl<'a> Calls<'a> {
    pub fn new(slots: &'a mut [Pending]) -> Self {
        Self { slots, len: 0 }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Remember call `serial` until `deadline`.
    pub fn track(&mut self, serial: u32, tag: u64, deadline: Millis) -> Result<(), CallsFull> {
        let slot = self.slots.get_mut(self.len).ok_or(CallsFull)?;
        *slot = Pending {
            serial,
            tag,
            deadline,
        };
        self.len += 1;
        Ok(())
    }

    /// The tag of the call a reply answers, forgetting it. `None` for a
    /// reply to nothing tracked, or one that already timed out.
    pub fn answer(&mut self, reply_serial: u32) -> Option<u64> {
        let i = self.slots[..self.len]
            .iter()
            .position(|p| p.serial == reply_serial)?;
        Some(self.remove(i).tag)
    }

    /// The earliest deadline, for the caller's timer.
    pub fn next_deadline(&self) -> Option<Millis> {
        self.slots[..self.len].iter().map(|p| p.deadline).min()
    }

    /// Hand every call past its deadline at `now` to `f`, forgetting it.
    pub fn expire(&mut self, now: Millis, f: &mut dyn FnMut(Pending)) {
        let mut i = 0;
        while i < self.len {
            if self.slots[i].deadline <= now {
                f(self.remove(i));
            } else {
                i += 1;
            }
        }
    }

    /// Whether a call with `tag` is out, so a caller can avoid asking twice.
    pub fn has_tag(&self, tag: u64) -> bool {
        self.slots[..self.len].iter().any(|p| p.tag == tag)
    }

    fn remove(&mut self, i: usize) -> Pending {
        let p = self.slots[i];
        self.len -= 1;
        self.slots[i] = self.slots[self.len];
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    #[test]
    fn replies_find_their_calls() {
        let mut slots = [Pending::default(); 4];
        let mut c = Calls::new(&mut slots);
        c.track(10, 1, 100).unwrap();
        c.track(11, 2, 200).unwrap();
        c.track(12, 3, 50).unwrap();
        assert_eq!(c.answer(11), Some(2));
        assert_eq!(c.answer(11), None, "answered once");
        assert_eq!(c.answer(99), None);
        assert_eq!(c.len(), 2);
        assert_eq!(c.next_deadline(), Some(50));
        assert!(c.has_tag(3) && !c.has_tag(2));
    }

    #[test]
    fn silence_times_out() {
        let mut slots = [Pending::default(); 4];
        let mut c = Calls::new(&mut slots);
        for (s, d) in [(1, 10), (2, 30), (3, 20), (4, 40)] {
            c.track(s, u64::from(s), d).unwrap();
        }
        assert_eq!(c.track(5, 5, 0), Err(CallsFull));
        let mut gone = Vec::new();
        c.expire(25, &mut |p| gone.push(p.serial));
        gone.sort();
        assert_eq!(gone, [1, 3]);
        assert_eq!(c.len(), 2);
        assert_eq!(c.answer(1), None, "a late reply is nobody's");
        assert_eq!(c.answer(4), Some(4));
        assert!(c.track(6, 6, 0).is_ok(), "slots come back");
    }
}
