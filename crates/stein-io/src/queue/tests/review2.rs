//! Regressions for the queue findings of the second unsafe review.

use super::*;
use std::os::unix::net::UnixStream;

fn setup(want: Want) -> Setup {
    Setup {
        want,
        ..Setup::default()
    }
}

/// Every completion available now.
fn reap_all(q: &mut Queue) -> Vec<Cqe> {
    let mut got = Vec::new();
    let mut buf = [Cqe::EMPTY; 16];
    loop {
        let n = q.reap(&mut buf);
        if n == 0 {
            return got;
        }
        got.extend_from_slice(&buf[..n]);
    }
}

/// Region memory leaked for the process, so nothing the kernel holds is ever freed.
fn leaked(q: &mut Queue, len: usize) -> Region {
    let mem: &'static mut [u8] = Box::leak(vec![0u8; len].into_boxed_slice());
    // SAFETY: leaked, valid for the process and never touched by Rust again.
    unsafe { q.register(mem.as_mut_ptr(), mem.len(), false) }.expect("region")
}

/// Finding 3, a group index reused while an old buffer completion waited in the CQ
/// let that completion count against the new group, so `provide` overfilled it.
fn a_reused_group_index_is_not_overfilled(want: Want) {
    let mut q = Queue::new(Owned::with_rings(8, 64, 2), setup(want)).expect("queue");
    let r = leaked(&mut q, 4096);
    let g = q.add_ring(1).expect("ring");
    q.provide(g, 0, q.mem(r, 0, 64).expect("mem"))
        .expect("provide");
    let (a, mut b) = UnixStream::pair().expect("pair");
    a.set_nonblocking(true).expect("nonblocking");
    let multi = Op::RecvMulti {
        fd: Fd::Raw(a.as_raw_fd()),
        group: g,
    };
    let key = q.push(multi, 1).expect("recv");
    q.submit().expect("submit");
    b.write_all(b"x").expect("write");
    let start = Instant::now();
    while !q.ready() {
        assert!(start.elapsed() < LIMIT, "no completion posted");
        q.tick(Some(Duration::from_millis(10))).expect("tick");
    }
    q.remove_group(g).expect("removed");
    let g2 = q.add_ring(1).expect("ring");
    q.provide(g2, 1, q.mem(r, 64, 64).expect("mem"))
        .expect("provide");
    let old = reap_all(&mut q);
    assert!(old.iter().any(|c| c.user == 1), "{want:?} {old:?}");
    // The new one-entry ring still holds bid 1.
    let second = q.provide(g2, 2, q.mem(r, 128, 64).expect("mem"));
    assert!(
        matches!(second, Err(IoError::BuffersFull)),
        "{want:?} a full ring took another buffer, {second:?}, groups {g:?} {g2:?}"
    );
    // On epoll the receive may have ended on the removed group already.
    if !old.iter().any(|c| c.user == 1 && !c.more()) {
        q.cancel(key);
        let mut seen = Vec::new();
        while wait(&mut q, 1, &mut seen).more() {}
    }
}

/// Finding 3, the index of a removed group is free again once its ops ended.
fn a_group_index_frees_once_its_ops_end(want: Want) {
    let mut q = Queue::new(Owned::with_rings(8, 64, 2), setup(want)).expect("queue");
    let g = q.add_ring(1).expect("ring");
    let (a, _b) = UnixStream::pair().expect("pair");
    a.set_nonblocking(true).expect("nonblocking");
    let multi = Op::RecvMulti {
        fd: Fd::Raw(a.as_raw_fd()),
        group: g,
    };
    let key = q.push(multi, 1).expect("recv");
    q.submit().expect("submit");
    q.remove_group(g).expect("removed");
    // On epoll it ended at once, the group being empty.
    q.cancel(key);
    let mut seen = Vec::new();
    while wait(&mut q, 1, &mut seen).more() {}
    assert_eq!(
        q.add_ring(1).expect("ring"),
        g,
        "{want:?} the index stayed taken"
    );
}

/// Finding 5, a store from an epoll queue kept its watch flags, so a new queue over it
/// never registered those descriptors and waited forever.
#[test]
fn a_reused_epoll_store_watches_afresh() {
    let (a, mut b) = UnixStream::pair().expect("pair");
    let poll = Op::Poll {
        fd: Fd::Raw(a.as_raw_fd()),
        events: libc::POLLIN as u32,
        multi: false,
    };
    let mut q = Queue::new(Owned::new(4, 64), setup(Want::Epoll)).expect("queue");
    q.push(poll, 1).expect("poll");
    q.submit().expect("submit");
    let store = q.into_store().expect("store");
    let mut q = Queue::new(store, setup(Want::Epoll)).expect("queue");
    q.push(poll, 2).expect("poll");
    q.submit().expect("submit");
    b.write_all(b"x").expect("write");
    let mut got = None;
    for _ in 0..50 {
        q.tick(Some(Duration::from_millis(10))).expect("tick");
        if let Some(c) = q.pop() {
            got = Some(c);
            break;
        }
    }
    assert_eq!(
        got.map(|c| c.user),
        Some(2),
        "a reused store never saw the fd"
    );
}

/// Finding 7, a timeout too far out to add to now panicked after taking its slot.
fn a_timeout_past_the_clock_neither_panics_nor_sticks(want: Want) {
    let mut q = queue(want);
    let key = q
        .push(
            Op::Timeout {
                after: Duration::MAX,
            },
            1,
        )
        .expect("timeout");
    q.submit().expect("submit");
    q.tick(Some(Duration::from_millis(5))).expect("tick");
    q.cancel(key);
    let c = wait(&mut q, 1, &mut Vec::new());
    assert!(c.res < 0, "{want:?} {c:?}");
    assert_eq!(q.in_flight(), 0, "{want:?} the slot stuck");
}

/// Finding 8, a path is read for its NUL at push, so a region path over a range an op in
/// flight writes is refused.
fn a_path_over_a_range_in_flight_is_refused(want: Want) {
    let mut q = queue(want);
    let r = leaked(&mut q, 4096);
    let (a, _b) = UnixStream::pair().expect("pair");
    a.set_nonblocking(true).expect("nonblocking");
    let recv = Op::Recv {
        fd: Fd::Raw(a.as_raw_fd()),
        mem: q.mem(r, 0, 64).expect("mem"),
    };
    let key = q.push(recv, 1).expect("recv");
    q.submit().expect("submit");
    let open = |q: &Queue, at: usize| Op::Open {
        dir: CWD,
        path: q.mem(r, at, 16).expect("mem"),
        flags: OFlags::RDONLY,
        mode: Mode::empty(),
    };
    let busy = q.push(open(&q, 60), 2);
    assert!(
        matches!(busy, Err(IoError::RegionBusy { at: 60, len: 16 })),
        "{want:?} {busy:?}"
    );
    // Clear of the receive, the empty path is read and ends in NUL.
    let clear = open(&q, 64);
    assert_eq!(one(&mut q, clear, 3).result(), Err(Errno::NOENT));
    assert!(q.cancel(key));
    wait(&mut q, 1, &mut Vec::new());
}

/// Finding 8, a region buffer handed to `provide` is named by no op, so `unregister`
/// refuses while it sits in a group.
fn a_region_in_a_group_is_not_unregistered(want: Want) {
    let mut slab = vec![0u8; 256];
    let mut q = Queue::new(Owned::with_rings(8, 64, 1), setup(want)).expect("queue");
    // SAFETY: `slab` outlives the queue, which drops first below.
    let r = unsafe { q.register(slab.as_mut_ptr(), slab.len(), false) }.expect("region");
    let g = q.add_ring(2).expect("ring");
    q.provide(g, 0, q.mem(r, 64, 64).expect("mem"))
        .expect("provide");
    let busy = q.unregister(r);
    assert!(
        matches!(busy, Err(IoError::RegionBusy { at: 64, len: 64 })),
        "{want:?} {busy:?}"
    );
    assert!(q.mem(r, 0, 8).is_ok(), "{want:?} the region went anyway");
    q.remove_group(g).expect("removed");
    q.unregister(r).expect("unregistered");
    drop(q);
    drop(slab);
}

/// Finding 8, an op in flight on a region range keeps the region registered.
fn a_region_in_flight_is_not_unregistered(want: Want) {
    let mut slab = vec![0u8; 256];
    let mut q = queue(want);
    // SAFETY: `slab` outlives the queue, which drops first below.
    let r = unsafe { q.register(slab.as_mut_ptr(), slab.len(), false) }.expect("region");
    let (a, _b) = UnixStream::pair().expect("pair");
    a.set_nonblocking(true).expect("nonblocking");
    let recv = Op::Recv {
        fd: Fd::Raw(a.as_raw_fd()),
        mem: q.mem(r, 8, 32).expect("mem"),
    };
    let key = q.push(recv, 1).expect("recv");
    q.submit().expect("submit");
    let busy = q.unregister(r);
    assert!(
        matches!(busy, Err(IoError::RegionBusy { at: 8, len: 32 })),
        "{want:?} {busy:?}"
    );
    assert!(q.cancel(key));
    wait(&mut q, 1, &mut Vec::new());
    q.unregister(r).expect("unregistered");
    drop(q);
    drop(slab);
}

both!(
    a_reused_group_index_is_not_overfilled,
    a_group_index_frees_once_its_ops_end,
    a_timeout_past_the_clock_neither_panics_nor_sticks,
    a_path_over_a_range_in_flight_is_refused,
    a_region_in_a_group_is_not_unregistered,
    a_region_in_flight_is_not_unregistered,
);
