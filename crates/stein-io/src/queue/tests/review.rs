//! Regressions for the queue findings of the unsafe review.

use super::*;
use std::net::TcpStream;
use std::os::fd::OwnedFd;

/// Finding 3, cancelling a multishot op still on the retry list relinked `next` and
/// delivered another op's completion twice.
fn a_cancel_on_the_retry_list_delivers_nothing_twice(want: Want) {
    let la = TcpListener::bind("127.0.0.1:0").expect("bind");
    let lb = TcpListener::bind("127.0.0.1:0").expect("bind");
    la.set_nonblocking(true).expect("nonblocking");
    lb.set_nonblocking(true).expect("nonblocking");
    let (a, b) = (
        la.local_addr().expect("addr"),
        lb.local_addr().expect("addr"),
    );
    let _peers: Vec<TcpStream> = [a, a, b, b]
        .into_iter()
        .map(|to| TcpStream::connect(to).expect("connect"))
        .collect();
    std::thread::sleep(Duration::from_millis(50));
    let mut q = queue(want);
    let accept = |fd: RawFd| Op::Accept {
        fd: Fd::Raw(fd),
        multi: true,
    };
    let ka = q.push(accept(la.as_raw_fd()), 1).expect("accept a");
    q.push(accept(lb.as_raw_fd()), 2).expect("accept b");
    q.submit().expect("submit");
    let mut seen = Vec::new();
    let start = Instant::now();
    // No tick between the take and the cancel, so the taken op waits on the retry list.
    while seen.is_empty() {
        assert!(start.elapsed() < LIMIT, "no first accept");
        match q.pop() {
            Some(c) => seen.push(c),
            None => q.tick(Some(Duration::from_millis(10))).expect("tick"),
        }
    }
    assert!(q.cancel(ka));
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(300) {
        while let Some(c) = q.pop() {
            seen.push(c);
        }
        q.tick(Some(Duration::from_millis(10))).expect("tick");
    }
    let fds: Vec<i32> = seen.iter().filter(|c| c.res >= 0).map(|c| c.res).collect();
    let mut unique = fds.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        fds.len(),
        unique.len(),
        "an accepted fd came twice: {seen:?}"
    );
    let finals = seen.iter().filter(|c| c.user == 1 && !c.more()).count();
    assert_eq!(
        finals, 1,
        "the cancelled accept did not end exactly once: {seen:?}"
    );
    let after = seen.iter().position(|c| c.user == 1 && !c.more());
    assert!(
        seen[after.unwrap_or(0) + 1..].iter().all(|c| c.user != 1),
        "a completion after the final one: {seen:?}"
    );
    for fd in fds {
        // SAFETY: accepted for this test and closed once here.
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
    }
}

/// Finding 4, a `Mem` kept past its region's unregister, or taken to another queue,
/// is refused rather than handed to the kernel.
fn a_mem_outliving_its_region_is_refused(want: Want) {
    let mut slab = vec![0u8; 64];
    let mut q = queue(want);
    // SAFETY: `slab` outlives the queue and nothing is in flight on it.
    let r = unsafe { q.register(slab.as_mut_ptr(), slab.len(), false) }.expect("region");
    let m = q.mem(r, 0, 8).expect("mem");
    let (rx, tx) = rustix::pipe::pipe().expect("pipe");
    let write = |m: Mem| Op::Write {
        fd: Fd::Raw(tx.as_raw_fd()),
        mem: m,
        at: POSITION,
    };
    assert_eq!(one(&mut q, write(m.part(0, 4)), 1).result(), Ok(4));

    // Another queue, even with a region at the same index.
    let mut other = queue(want);
    let mut theirs = vec![0u8; 64];
    // SAFETY: as above.
    let r2 = unsafe { other.register(theirs.as_mut_ptr(), theirs.len(), false) }.expect("region");
    assert_eq!(r2, r);
    assert!(matches!(other.push(write(m), 2), Err(IoError::StaleMem)));
    assert!(matches!(
        other.provide(Group(0), 0, m),
        Err(IoError::StaleMem)
    ));

    q.unregister(r).expect("unregistered");
    assert!(matches!(q.push(write(m), 3), Err(IoError::StaleMem)));
    // A new region in the old slot gets a new stamp.
    // SAFETY: as above.
    let again = unsafe { q.register(slab.as_mut_ptr(), slab.len(), false) }.expect("region");
    assert_eq!(again, r);
    assert!(matches!(q.push(write(m), 4), Err(IoError::StaleMem)));
    let fresh = q.mem(again, 0, 4).expect("mem");
    assert_eq!(one(&mut q, write(fresh), 5).result(), Ok(4));
    drop(rx);
}

/// Finding 6, providing more buffers than the ring holds laps the kernel's head.
fn a_full_ring_refuses_another_buffer(want: Want) {
    // Before the queue, so they outlive it.
    let mut bufs = vec![[0u8; 64]; 5];
    let setup = Setup {
        want,
        ..Setup::default()
    };
    let mut q = Queue::new(Owned::with_rings(8, 1024, ring_pages(4)), setup).expect("queue");
    let g = q.add_ring(4).expect("ring");
    for (i, b) in bufs.iter_mut().enumerate().take(4) {
        q.provide(g, i as u16, mem(b)).expect("room");
    }
    assert!(
        matches!(
            q.provide(g, 4, mem(&mut bufs[4])),
            Err(IoError::BuffersFull)
        ),
        "a fifth buffer went into a ring of four"
    );
    let (mut a, b) = std::os::unix::net::UnixStream::pair().expect("pair");
    b.set_nonblocking(true).expect("nonblocking");
    let key = q
        .push(
            Op::RecvMulti {
                fd: Fd::Raw(b.as_raw_fd()),
                group: g,
            },
            1,
        )
        .expect("recv");
    q.submit().expect("submit");
    std::io::Write::write_all(&mut a, b"one").expect("write");
    let c = wait(&mut q, 1, &mut Vec::new());
    assert_eq!(c.result(), Ok(3));
    let bid = c.buffer().expect("a ring buffer");
    // One taken and seen, so one has room again, and only one.
    q.provide(g, 4, mem(&mut bufs[4]))
        .expect("room after a take");
    assert!(matches!(
        q.provide(g, bid, mem(&mut bufs[bid as usize])),
        Err(IoError::BuffersFull)
    ));
    assert!(q.cancel(key));
    let mut seen = Vec::new();
    while wait(&mut q, 1, &mut seen).more() {}
}

/// Finding 8, a quiesce drops the completions it drains, so an accept that landed
/// gets its descriptor closed, not leaked.
fn a_drained_accept_closes_its_descriptor(want: Want) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    l.set_nonblocking(true).expect("nonblocking");
    let mut peer = TcpStream::connect(l.local_addr().expect("addr")).expect("connect");
    peer.set_read_timeout(Some(LIMIT)).expect("timeout");
    std::thread::sleep(Duration::from_millis(20));
    let mut q = queue(want);
    q.push(
        Op::Accept {
            fd: Fd::Raw(l.as_raw_fd()),
            multi: false,
        },
        1,
    )
    .expect("accept");
    let start = Instant::now();
    while !q.ready() {
        assert!(start.elapsed() < LIMIT, "the accept never landed");
        q.submit().expect("submit");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(q.quiesce(LIMIT));
    let mut b = [0u8; 1];
    assert_eq!(peer.read(&mut b).expect("eof"), 0, "the accepted fd leaked");
}

/// A ring refuses to close a ring's own fd with `EBADF`, which a flush error after a real
/// close also gives. The queue leaks it rather than close a number that may be reused.
#[test]
fn a_ring_fd_the_ring_refuses_to_close_is_left_open() {
    let mut q = queue(Want::Uring);
    let other = queue(Want::Uring);
    let dup = rustix::io::fcntl_dupfd_cloexec(other.as_fd(), 0).expect("dup");
    let n = dup.as_raw_fd();
    let ino = rustix::fs::fstat(&dup).expect("stat").st_ino;
    q.close(dup, 1).expect("close");
    let c = wait(&mut q, 1, &mut Vec::new());
    assert_eq!(c.result(), Err(Errno::BADF), "the ring closed a ring fd");
    // SAFETY: nothing in this test opens files, so the number is still the dup.
    let left = unsafe { OwnedFd::from_raw_fd(n) };
    let now = rustix::fs::fstat(&left).expect("still open");
    assert_eq!(now.st_ino, ino, "the queue closed an ambiguous close again");
    drop(left);
}

/// Finding 7, a cancel whose push failed must not block the next one.
#[test]
fn a_failed_cancel_leaves_the_key_cancellable() {
    struct Moved(Queue);
    // SAFETY: the test hands the queue to one thread at a time.
    unsafe impl Send for Moved {}
    let setup = Setup {
        want: Want::Uring,
        single_issuer: true,
        fixed_buffers: 0,
    };
    let mut q = Queue::new(Owned::new(8, 0), setup).expect("queue");
    if q.ring_flags() != RingFlags::Deferred {
        return;
    }
    // Fill the submission ring, unsubmitted. The last is a long timeout.
    for i in 0..7 {
        q.push(Op::Nop, i).expect("nop");
    }
    let key = q
        .push(
            Op::Timeout {
                after: Duration::from_secs(60),
            },
            9,
        )
        .expect("timeout");
    // Another thread cannot submit to a single issuer ring, so the cancel's push fails.
    let moved = Moved(q);
    let (moved, asked) = std::thread::spawn(move || {
        let mut m = moved;
        let asked = m.0.cancel(key);
        (m, asked)
    })
    .join()
    .expect("thread");
    let mut q = moved.0;
    assert!(!asked, "the cancel reached a ring it cannot submit to");
    q.submit().expect("submit");
    assert!(q.cancel(key), "a failed cancel blocked the next");
    let c = wait(&mut q, 9, &mut Vec::new());
    assert_eq!(c.result(), Err(Errno::CANCELED));
}

/// Finding 1, a forgotten queue keeps its slot memory, so the kernel's later write lands
/// in leaked memory and never in a fresh allocation.
#[test]
fn a_forgotten_queue_keeps_what_the_kernel_writes() {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let at = l.local_addr().expect("addr");
    let mut q = queue(Want::Uring);
    q.push(
        Op::Accept {
            fd: Fd::Raw(l.as_raw_fd()),
            multi: false,
        },
        7,
    )
    .expect("accept");
    q.submit().expect("submit");
    let bytes = 32 * size_of::<Side>();
    std::mem::forget(q);
    let fresh = vec![0x10u8; bytes];
    let _c = TcpStream::connect(at).expect("connect");
    std::thread::sleep(Duration::from_millis(100));
    let hit = fresh
        .windows(8)
        .any(|w| w[0] == 2 && w[1] == 0 && w[4..8] == [127, 0, 0, 1]);
    assert!(!hit, "the kernel wrote a peer address into freed memory");
}

both!(
    a_cancel_on_the_retry_list_delivers_nothing_twice,
    a_mem_outliving_its_region_is_refused,
    a_full_ring_refuses_another_buffer,
    a_drained_accept_closes_its_descriptor,
);
