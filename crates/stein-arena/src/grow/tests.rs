//! Growth in place, the ceiling, and element types.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::Grow;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rec {
    at: u32,
    span: [u64; 2],
    flag: bool,
}

#[test]
fn it_grows_without_moving() {
    let mut g: Grow<Rec> = Grow::reserve(1 << 20).unwrap();
    g.push(Rec {
        at: 1,
        span: [2, 3],
        flag: true,
    })
    .unwrap();
    let first = g.as_ptr();
    for i in 0..100_000u32 {
        g.push(Rec {
            at: i,
            span: [u64::from(i), 7],
            flag: i % 2 == 0,
        })
        .unwrap();
    }
    assert_eq!(g.as_ptr(), first, "nothing moved");
    assert_eq!(g.len(), 100_001);
    assert_eq!(g[50_001].at, 50_000);
    assert!(g[1].flag && !g[2].flag);
    g.last_mut().unwrap().at = 9;
    assert_eq!(g.pop().unwrap().at, 9);
    g.truncate(3);
    assert_eq!(g.len(), 3);
    g.extend_from_slice(&[g[0], g[1]]).unwrap();
    assert_eq!(g[4], g[1]);
}

#[test]
fn the_reservation_is_a_ceiling() {
    let mut g: Grow<u8> = Grow::reserve(10).unwrap();
    let cap = g.capacity();
    assert!(cap >= 10, "a page at least");
    g.extend_from_slice(&vec![1; cap]).unwrap();
    assert!(g.push(2).is_err());
    assert!(g.extend_from_slice(&[3]).is_err());
    assert_eq!(g.len(), cap, "a refused push changes nothing");
    g.clear();
    assert!(g.push(4).is_ok());
    assert_eq!(g.pop(), Some(4));
    assert_eq!(g.pop(), None);
}

#[test]
fn nonsense_capacities_are_refused() {
    assert!(Grow::<u8>::reserve(0).is_err());
    assert!(Grow::<u64>::reserve(usize::MAX).is_err());
}
