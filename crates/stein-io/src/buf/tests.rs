#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn arena(count: usize, size: u32) -> (Vec<u8>, Vec<u16>, Vec<u8>) {
    (
        vec![0u8; count * size as usize],
        vec![0u16; count],
        vec![0u8; state_bytes(count)],
    )
}

/// A duplicate of `id`, which safe code cannot make, to test what the arena checks itself.
fn dup(id: &BufId) -> BufId {
    BufId {
        index: id.index,
        stamp: id.stamp,
    }
}

macro_rules! bufs {
    ($m:ident, $f:ident, $l:ident, $size:expr) => {
        Bufs::new(&mut $m, &mut $f, &mut $l, $size).expect("arena")
    };
}

#[test]
fn an_arena_divides_into_whole_buffers() {
    let (mut m, mut f, mut l) = arena(4, 64);
    let b = bufs!(m, f, l, 64);
    assert_eq!(b.count(), 4);
    assert_eq!(b.size(), 64);
    assert_eq!(b.available(), 4);
}

#[test]
fn a_remainder_is_not_a_buffer() {
    // 260 / 64 is four buffers.
    let mut m = vec![0u8; 260];
    let mut f = vec![0u16; 8];
    let mut l = vec![0u8; 8];
    let b = Bufs::new(&mut m, &mut f, &mut l, 64).expect("arena");
    assert_eq!(b.count(), 4);
}

#[test]
fn taking_and_releasing_round_trips() {
    let (mut m, mut f, mut l) = arena(3, 16);
    let mut b = bufs!(m, f, l, 16);
    let x = b.take().unwrap();
    let y = b.take().unwrap();
    assert_ne!(x, y, "the same buffer was handed out twice");
    assert_eq!(b.available(), 1);
    assert!(b.is_loaned(&x) && b.is_loaned(&y));

    let again = dup(&x);
    assert!(b.release(x));
    assert_eq!(b.available(), 2);
    assert!(!b.is_loaned(&again));
}

#[test]
fn an_exhausted_arena_says_so_rather_than_aliasing() {
    let (mut m, mut f, mut l) = arena(2, 16);
    let mut b = bufs!(m, f, l, 16);
    let x = b.take().unwrap();
    let _y = b.take().unwrap();
    assert!(b.is_empty());
    assert_eq!(b.take(), Err(PoolFull));
    b.release(x);
    assert!(b.take().is_ok());
}

#[test]
fn a_double_release_is_refused() {
    let (mut m, mut f, mut l) = arena(2, 16);
    let mut b = bufs!(m, f, l, 16);
    let x = b.take().unwrap();
    let again = dup(&x);
    assert!(b.release(x), "the first release should take");
    assert!(!b.release(again), "the second release should be refused");
    assert_eq!(b.available(), 2, "the free list grew past the arena");

    let a = b.take().unwrap();
    let c = b.take().unwrap();
    assert_ne!(a, c);
    assert_eq!(b.take(), Err(PoolFull));
}

#[test]
fn releasing_something_never_taken_is_refused() {
    let (mut m, mut f, mut l) = arena(2, 16);
    let mut b = bufs!(m, f, l, 16);
    assert!(!b.release(b.mint(0)));
    assert!(!b.release(b.mint(99)), "an id outside the arena");
    assert_eq!(b.available(), 2);
}

#[test]
fn every_buffer_is_a_distinct_region() {
    let (mut m, mut f, mut l) = arena(4, 8);
    let mut b = bufs!(m, f, l, 8);
    let ids: Vec<BufId> = (0..4).map(|_| b.take().unwrap()).collect();
    for (i, id) in ids.iter().enumerate() {
        b.bytes_mut(id).fill(i as u8 + 1);
    }
    for (i, id) in ids.iter().enumerate() {
        assert!(
            b.bytes(id).iter().all(|&x| x == i as u8 + 1),
            "buffer {i} was overwritten by a neighbour"
        );
    }
}

#[test]
fn a_buffer_is_exactly_its_size() {
    let (mut m, mut f, mut l) = arena(3, 16);
    let mut b = bufs!(m, f, l, 16);
    let x = b.take().unwrap();
    assert_eq!(b.bytes(&x).len(), 16);
    assert_eq!(b.bytes_mut(&x).len(), 16);
}

#[test]
fn an_unknown_id_reads_empty_rather_than_panicking() {
    let (mut m, mut f, mut l) = arena(2, 16);
    let mut b = bufs!(m, f, l, 16);
    let unknown = b.mint(50);
    assert!(b.bytes(&unknown).is_empty());
    assert!(b.bytes_mut(&unknown).is_empty());
    assert!(!b.is_loaned(&unknown));
}

#[test]
fn filled_returns_what_was_written_not_the_capacity() {
    let (mut m, mut f, mut l) = arena(2, 32);
    let mut b = bufs!(m, f, l, 32);
    let id = b.take().unwrap();
    b.bytes_mut(&id)[..5].copy_from_slice(b"hello");
    let mut got = Buf { id, len: 5 };
    assert_eq!(b.filled(&got), b"hello");
    got.len = 0;
    assert_eq!(b.filled(&got), b"");
}

#[test]
fn an_overlong_length_is_clamped_to_the_buffer() {
    let (mut m, mut f, mut l) = arena(2, 16);
    let mut b = bufs!(m, f, l, 16);
    let id = b.take().unwrap();
    assert_eq!(b.filled(&Buf { id, len: 9999 }).len(), 16);
    let unknown = Buf {
        id: b.mint(77),
        len: 9,
    };
    assert_eq!(b.filled(&unknown).len(), 0);
}

#[test]
fn an_arena_too_small_for_one_buffer_is_refused() {
    let mut m = vec![0u8; 10];
    let mut f = vec![0u16; 4];
    let mut l = vec![0u8; 4];
    assert!(matches!(
        Bufs::new(&mut m, &mut f, &mut l, 64),
        Err(IoError::BadArena { .. })
    ));
    assert!(matches!(
        Bufs::new(&mut m, &mut f, &mut l, 0),
        Err(IoError::BadArena { .. })
    ));
}

#[test]
fn columns_too_short_for_the_buffer_count_are_refused() {
    let mut m = vec![0u8; 8 * 64];
    let mut short = vec![0u16; 2];
    let mut l = vec![0u8; 8];
    assert!(matches!(
        Bufs::new(&mut m, &mut short, &mut l, 64),
        Err(IoError::BadArena { .. })
    ));
    let mut f = vec![0u16; 8];
    let mut tiny = vec![0u8; 0];
    assert!(matches!(
        Bufs::new(&mut m, &mut f, &mut tiny, 64),
        Err(IoError::BadArena { .. })
    ));
}

#[test]
fn a_reused_arena_starts_empty_of_loans() {
    let (mut m, mut f, mut l) = arena(3, 16);
    {
        let mut b = bufs!(m, f, l, 16);
        let _ = b.take().unwrap();
        let _ = b.take().unwrap();
        assert_eq!(b.available(), 1);
    }
    let b = bufs!(m, f, l, 16);
    assert_eq!(b.available(), 3);
    assert!(!b.is_loaned(&b.mint(0)));
}

#[test]
fn the_state_is_one_byte_per_buffer() {
    assert_eq!(state_bytes(0), 0);
    assert_eq!(state_bytes(9), 9);
    assert_eq!(state_bytes(4096), 4096);
}

/// Finding 2, a buffer lent to an op was released and handed out again.
#[test]
fn a_lent_buffer_is_neither_released_nor_reachable() {
    let (mut m, mut f, mut l) = arena(3, 16);
    let mut b = bufs!(m, f, l, 16);
    let x = b.take().unwrap();
    b.bytes_mut(&x)[..4].copy_from_slice(b"mine");
    let kept = dup(&x);
    let i = b.lend(x).expect("lent");
    let x = kept;
    assert_eq!(b.owner(&x), Some(Own::Kernel));
    assert!(!b.release(dup(&x)), "a lent buffer was released");
    assert!(b.bytes(&x).is_empty() && b.bytes_mut(&x).is_empty());
    let x = Buf { id: x, len: 4 };
    assert!(b.filled(&x).is_empty());
    let y = b.take().unwrap();
    assert_ne!(x.id, y, "a lent buffer was handed out again");
    assert_eq!(
        b.copy_range(&y, 0, &x.id, 0, 4),
        0,
        "copied out of a lent buffer"
    );
    assert_eq!(
        b.copy_range(&x.id, 0, &y, 0, 4),
        0,
        "copied into a lent buffer"
    );
    assert!(b.lend(dup(&x.id)).is_none(), "lent twice");
    let landed = b.land(i);
    assert_eq!(landed, x.id);
    assert_eq!(b.owner(&x.id), Some(Own::Caller));
    assert_eq!(b.filled(&x), b"mine");
    let again = dup(&x.id);
    assert!(b.release(x));
    assert!(!b.release(again));
    assert!(b.release(y));
}

#[test]
fn a_ring_buffer_is_out_of_reach_until_claimed_once() {
    let (mut m, mut f, mut l) = arena(4, 16);
    let mut b = bufs!(m, f, l, 16);
    b.reserve(2).unwrap();
    let ring = b.mint(0);
    assert!(
        b.bytes(&ring).is_empty(),
        "a ring buffer read while the kernel may fill it"
    );
    assert!(!b.release(dup(&ring)));
    assert!(b.claim(0));
    assert!(!b.claim(0), "claimed twice");
    assert_eq!(b.bytes(&ring).len(), 16);
    assert!(b.release(ring));
    assert_eq!(b.available(), 2, "a ring buffer went to the free list");
    assert!(!b.claim(2), "a free list buffer claimed as a ring one");
}

#[test]
fn an_owned_arena_frees_and_takes_like_a_borrowed_one() {
    let mut b = Bufs::owned(5, 32).expect("arena");
    assert_eq!((b.count(), b.size(), b.available()), (5, 32, 5));
    let x = b.take().unwrap();
    assert_eq!(b.bytes_mut(&x).len(), 32);
    let y = b.take_lent().unwrap();
    assert_eq!(b.owner(&b.mint(y)), Some(Own::Kernel));
    assert!(b.free(y) && !b.free(y));
    assert!(b.release(x));
    assert_eq!(b.available(), 5);
    assert!(matches!(Bufs::owned(0, 32), Err(IoError::BadArena { .. })));
}

#[test]
fn loans_are_tracked_for_every_buffer() {
    let (mut m, mut f, mut l) = arena(20, 8);
    let mut b = bufs!(m, f, l, 8);
    let ids: Vec<BufId> = (0..20).map(|_| b.take().unwrap()).collect();
    assert!(ids.iter().all(|i| b.is_loaned(i)));
    let kept: Vec<BufId> = ids.iter().map(dup).collect();
    for id in ids {
        let name = format!("{id:?}");
        assert!(b.release(id), "{name} was not seen as loaned");
    }
    assert_eq!(b.available(), 20);
    assert!(kept.iter().all(|i| !b.is_loaned(i)));
}

#[test]
fn a_large_arena_hands_out_every_buffer_exactly_once() {
    const N: usize = 10_000;
    let (mut m, mut f, mut l) = arena(N, 8);
    let mut b = bufs!(m, f, l, 8);
    let mut seen = vec![false; N];
    for _ in 0..N {
        let id = b.take().expect("arena ran dry early");
        assert!(!seen[id.index() as usize], "{id:?} was handed out twice");
        seen[id.index() as usize] = true;
    }
    assert_eq!(b.take(), Err(PoolFull));
    assert!(seen.iter().all(|&s| s));
}

#[test]
fn an_arena_wider_than_an_id_is_refused() {
    let count = MAX_BUFFERS + 1;
    let mut m = vec![0u8; count];
    let mut f = vec![0u16; count];
    let mut l = vec![0u8; state_bytes(count)];
    assert!(matches!(
        Bufs::new(&mut m, &mut f, &mut l, 1),
        Err(IoError::TooManySlots(_))
    ));
}

/// Review 2 finding 7, a reserve rewrote the free list over buffers on loan.
#[test]
fn a_reserve_is_refused_while_a_buffer_is_on_loan() {
    let (mut m, mut f, mut l) = arena(4, 16);
    let mut b = bufs!(m, f, l, 16);
    let x = b.take().unwrap();
    assert!(b.reserve(2).is_err(), "reserved over a loaned buffer");
    assert_eq!(b.available(), 3, "the free list was rewritten");
    let y = b.take().unwrap();
    assert_ne!(x, y, "a loaned buffer was handed out again");
    assert!(b.release(x) && b.release(y));
    b.reserve(2).expect("all free");
    assert_eq!(b.available(), 2);
}
