#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::op::Kind;

fn op() -> Op {
    Op::new(Kind::Timeout, -1)
}

/// Review 2 finding 7, a slot whose generation wraps would hand a stale id its op back.
#[test]
fn a_slot_whose_generation_would_wrap_is_retired() {
    let mut t = Table::new(1);
    let first = t.insert(op()).expect("slot");
    t.remove(first).expect("live");
    t.generations[0] = u32::MAX - 1;
    let last = t.insert(op()).expect("slot");
    assert!(t.remove(last).is_some());
    assert!(t.insert(op()).is_none(), "a wrapped slot came back");
    assert!(t.get(first).is_none(), "a stale id named a live op");
    assert_eq!(t.len(), 0);
}

#[test]
fn slots_are_reused_before_the_wrap() {
    let mut t = Table::new(2);
    let a = t.insert(op()).expect("slot");
    t.remove(a).expect("live");
    let b = t.insert(op()).expect("slot");
    assert_eq!(a.slot(), b.slot());
    assert!(t.get(a).is_none());
    assert!(t.get(b).is_some());
}
