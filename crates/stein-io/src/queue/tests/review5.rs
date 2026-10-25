//! Regressions for the queue findings of the fifth review. Entries of the queue's or its
//! caller's own never join a caller's chain.

use super::*;
use std::os::fd::OwnedFd;

fn ring() -> Option<Queue> {
    let setup = Setup {
        want: Want::Uring,
        ..Setup::default()
    };
    let q = Queue::new(Owned::new(16, 64), setup).ok()?;
    (q.backend() == Backend::Uring).then_some(q)
}

/// A pipe whose read end polls readable once written.
fn gate() -> (OwnedFd, OwnedFd) {
    rustix::pipe::pipe_with(rustix::pipe::PipeFlags::NONBLOCK).expect("pipe")
}

fn readable(fd: &OwnedFd) -> Op {
    Op::Poll {
        fd: Fd::Raw(fd.as_raw_fd()),
        events: libc::POLLIN as u32,
        multi: false,
    }
}

/// An op that fails with `EBADF`, on a number nothing has.
fn failing() -> Op {
    // SAFETY: `F_GETFD` only asks whether the number is open.
    let bad = (1 << 18..)
        .find(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } < 0)
        .expect("a free number");
    Op::Fsync {
        fd: Fd::Raw(bad),
        data_only: true,
    }
}

/// Tick a while, collecting what completes.
fn settle(q: &mut Queue, seen: &mut Vec<Cqe>) {
    for _ in 0..5 {
        q.tick(Some(Duration::from_millis(10))).expect("tick");
        while let Some(c) = q.pop() {
            seen.push(c);
        }
    }
}

/// An entry apart waited behind an open chain, or took a pending link from the next push.
#[test]
fn an_entry_apart_neither_waits_behind_a_chain_nor_takes_its_link() {
    let Some(mut q) = ring() else {
        return;
    };
    let (rx, tx) = gate();
    q.link();
    q.push_apart(Op::Nop, 1).expect("apart, link pending");
    q.push(readable(&rx), 2).expect("head");
    q.push_apart(Op::Nop, 3).expect("apart, chain open");
    q.push(Op::Nop, 4).expect("tail");
    let mut seen = Vec::new();
    assert_eq!(wait(&mut q, 1, &mut seen).result(), Ok(0));
    assert_eq!(wait(&mut q, 3, &mut seen).result(), Ok(0));
    settle(&mut q, &mut seen);
    assert!(
        seen.iter().all(|c| c.user != 4),
        "the tail lost its link to the head"
    );
    rustix::io::write(&tx, b"x").expect("open the gate");
    assert!(wait(&mut q, 2, &mut seen).result().is_ok());
    assert_eq!(wait(&mut q, 4, &mut seen).result(), Ok(0));
}

/// An entry apart joined a chain and failed with it.
#[test]
fn an_entry_apart_does_not_fail_with_a_chain() {
    let Some(mut q) = ring() else {
        return;
    };
    q.link();
    q.push(failing(), 1).expect("head");
    q.push_apart(Op::Nop, 2).expect("apart");
    q.push(Op::Nop, 3).expect("tail");
    let mut seen = Vec::new();
    assert_eq!(wait(&mut q, 1, &mut seen).result(), Err(Errno::BADF));
    assert_eq!(wait(&mut q, 3, &mut seen).result(), Err(Errno::CANCELED));
    assert_eq!(wait(&mut q, 2, &mut seen).result(), Ok(0));
}

/// A cancel took the chain's next place, so it waited behind the chain and ended it.
#[test]
fn a_cancel_neither_waits_behind_a_chain_nor_ends_it() {
    let Some(mut q) = ring() else {
        return;
    };
    let hour = Op::Timeout {
        after: Duration::from_secs(3600),
    };
    let timer = q.push(hour, 9).expect("timer");
    let (rx, tx) = gate();
    q.link();
    q.push(readable(&rx), 1).expect("head");
    assert!(q.cancel(timer));
    q.push(Op::Nop, 2).expect("tail");
    let mut seen = Vec::new();
    assert_eq!(wait(&mut q, 9, &mut seen).result(), Err(Errno::CANCELED));
    settle(&mut q, &mut seen);
    assert!(
        seen.iter().all(|c| c.user != 2),
        "the cancel ended the chain"
    );
    rustix::io::write(&tx, b"x").expect("open the gate");
    assert!(wait(&mut q, 1, &mut seen).result().is_ok());
    assert_eq!(wait(&mut q, 2, &mut seen).result(), Ok(0));
}

/// A cancel in a failed chain failed with it and never ran.
#[test]
fn a_cancel_does_not_fail_with_a_chain() {
    let Some(mut q) = ring() else {
        return;
    };
    let hour = Op::Timeout {
        after: Duration::from_secs(3600),
    };
    let timer = q.push(hour, 9).expect("timer");
    q.link();
    q.push(failing(), 1).expect("head");
    assert!(q.cancel(timer));
    q.push(Op::Nop, 2).expect("tail");
    let mut seen = Vec::new();
    assert_eq!(wait(&mut q, 1, &mut seen).result(), Err(Errno::BADF));
    assert_eq!(wait(&mut q, 2, &mut seen).result(), Err(Errno::CANCELED));
    assert_eq!(wait(&mut q, 9, &mut seen).result(), Err(Errno::CANCELED));
}

/// An entry cancelled while it waits behind a chain never goes out, so it never runs on
/// a descriptor closed meanwhile.
#[test]
fn an_entry_cancelled_behind_a_chain_never_goes_out() {
    let Some(mut q) = ring() else {
        return;
    };
    let (rx, tx) = gate();
    let (ready, fill) = gate();
    rustix::io::write(&fill, b"x").expect("fill");
    q.link();
    q.push(readable(&rx), 1).expect("head");
    let k = q.push_apart(readable(&ready), 2).expect("apart");
    assert!(q.cancel(k));
    q.push(Op::Nop, 3).expect("tail");
    let mut seen = Vec::new();
    assert_eq!(wait(&mut q, 2, &mut seen).result(), Err(Errno::CANCELED));
    rustix::io::write(&tx, b"x").expect("open the gate");
    assert!(wait(&mut q, 1, &mut seen).result().is_ok());
    assert_eq!(wait(&mut q, 3, &mut seen).result(), Ok(0));
}

/// A chain left open held what waited behind it until the next push.
#[test]
fn a_tick_ends_a_chain_left_open() {
    let Some(mut q) = ring() else {
        return;
    };
    let (rx, tx) = gate();
    q.link();
    q.push(readable(&rx), 1).expect("head");
    q.push_apart(Op::Nop, 2).expect("apart");
    let mut seen = Vec::new();
    assert_eq!(wait(&mut q, 2, &mut seen).result(), Ok(0));
    assert!(!q.in_chain());
    q.push(Op::Nop, 3).expect("after");
    assert_eq!(wait(&mut q, 3, &mut seen).result(), Ok(0));
    rustix::io::write(&tx, b"x").expect("open the gate");
    assert!(wait(&mut q, 1, &mut seen).result().is_ok());
}
