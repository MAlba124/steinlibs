//! Regressions for the driver findings of the 2026-11-23 review. A stale peer reaches no
//! new mailbox, a tick with a completion queued does not sleep, and closing a directory
//! reaches the ops relative to it.

use super::*;
use crate::{Mode, OFlags};
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

fn pair() -> (OwnedFd, OwnedFd) {
    let flags = SocketFlags::CLOEXEC | SocketFlags::NONBLOCK;
    socketpair(AddressFamily::UNIX, SocketType::STREAM, flags, None).expect("socketpair")
}

/// Values posted to `io`, ticking up to `limit` or until `until` arrives.
fn values(io: &mut Io, until: Option<u64>, limit: Duration) -> Vec<u64> {
    let mut got = Vec::new();
    let start = Instant::now();
    while start.elapsed() < limit && until.is_none_or(|u| !got.contains(&u)) {
        io.tick(Some(Duration::from_millis(5))).expect("tick");
        while let Some(c) = io.next_completion() {
            got.extend(c.value());
        }
    }
    got
}

/// Holds every free number below `n`, so the next descriptor made takes `n`.
fn hold_below(n: RawFd) -> Vec<OwnedFd> {
    let mut held = Vec::new();
    loop {
        let d = rustix::fs::open("/dev/null", OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
            .expect("null");
        if d.as_raw_fd() >= n {
            return held;
        }
        held.push(d);
    }
}

/// A peer was its number and kind only, so once its mailbox closed and another of the
/// same kind took the number, a post through the old peer reached the new mailbox.
fn check_a_stale_peer_reaches_no_new_mailbox(back: Backend, target: Backend) {
    driver!(io, back, ops = 8, bufs = 8, size = 64);
    let mut a = Io::with_backend(Storage::new(8, 8), 64, target).expect("target a");
    let mut b = Io::with_backend(Storage::new(8, 8), 64, target).expect("target b");
    // Starts the epoll target's pipe, so the next mailbox makes one descriptor only.
    drop(b.mailbox().expect("warm mailbox b"));
    let old = io.peer(a.mailbox().expect("mailbox a"));
    let n = old.fd();
    assert!(io.close(n));
    io.tick(Some(Duration::ZERO)).expect("tick");
    while io.next_completion().is_some() {}
    let _held = hold_below(n);
    let new = io.peer(b.mailbox().expect("mailbox b"));
    assert_eq!(
        new.fd(),
        n,
        "{back:?} to {target:?} the number was not reused"
    );
    let r = io.post(old, 7);
    assert!(
        matches!(r, Err(IoError::Kernel(Errno::BADF))),
        "{back:?} to {target:?} a stale peer posted, {r:?}"
    );
    io.post(new, 9).expect("post through the new peer");
    let delivers = target == Backend::Epoll || back == Backend::Uring;
    let mut got = Vec::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(200) && !got.contains(&9) {
        io.tick(Some(Duration::ZERO)).expect("tick");
        while io.next_completion().is_some() {}
        got.extend(values(&mut b, Some(9), Duration::from_millis(10)));
    }
    assert!(
        !got.contains(&7),
        "{back:?} to {target:?} the stale post arrived"
    );
    if delivers {
        assert!(got.contains(&9), "{back:?} to {target:?} {got:?}");
    }
    drop(a);
}

#[test]
fn a_stale_peer_reaches_no_new_mailbox() {
    for back in backends() {
        for target in backends() {
            check_a_stale_peer_reaches_no_new_mailbox(back, target);
        }
    }
}

/// A receive armed once a buffer freed completed inline on epoll, yet the tick then waited
/// out its whole timeout with that completion queued.
fn check_a_receive_armed_at_the_tick_ends_the_tick(back: Backend) {
    driver!(io, back, ops = 8, bufs = 8, size = 64);
    let (a, b) = pair();
    let a = io.own(a);
    rustix::io::write(&b, b"hi").expect("write");
    let mut held = Vec::new();
    while let Ok(x) = io.alloc() {
        held.push(x);
    }
    let rx = io.recv(a, false).expect("recv waits for a buffer");
    io.release(held.pop().expect("a held buffer"));
    let start = Instant::now();
    let n = io.tick(Some(Duration::from_millis(300))).expect("tick");
    let took = start.elapsed();
    assert!(n > 0, "{back:?} no completion after the tick");
    assert!(
        took < Duration::from_millis(150),
        "{back:?} the tick slept {took:?} with a completion queued"
    );
    let c = io.next_completion().expect("completion");
    assert_eq!((c.op, c.result), (rx, Ok(2)), "{back:?}");
    if let Some(x) = c.buf {
        io.release(x);
    }
}

#[test]
fn a_receive_armed_at_the_tick_ends_the_tick() {
    // Own thread, as task work a dropped ring left on this one interrupts a wait.
    for b in backends() {
        let t = std::thread::spawn(move || check_a_receive_armed_at_the_tick_ends_the_tick(b));
        t.join().expect("check");
    }
}

/// A fresh directory under the temp dir with an empty file `f`, and its owned fd.
fn scratch(io: &mut Io, name: &str, back: Backend) -> (std::path::PathBuf, RawFd) {
    let d = std::env::temp_dir().join(format!("stein-io-{}-{name}-{back:?}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("mkdir");
    std::fs::write(d.join("f"), b"x").expect("file");
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let fd = rustix::fs::open(&d, flags, Mode::empty()).expect("open dir");
    (d, io.own(fd))
}

/// Ticks for `limit`, returning the ids of the final completions.
fn finals(io: &mut Io, limit: Duration) -> Vec<OpId> {
    let mut got = Vec::new();
    let start = Instant::now();
    while start.elapsed() < limit {
        io.tick(Some(Duration::from_millis(5))).expect("tick");
        while let Some(c) = io.next_completion() {
            if c.is_final() {
                got.push(c.op);
            }
        }
    }
    got
}

/// Path ops recorded no descriptor, so closing the directory they resolve against left
/// them live on the ring, to run against whatever took its number.
fn check_closing_a_directory_reaches_ops_relative_to_it(back: Backend) {
    driver!(io, back, ops = 16, bufs = 16, size = 256);
    let (d1, one) = scratch(&mut io, "dir-one", back);
    let (d2, two) = scratch(&mut io, "dir-two", back);
    let open = io
        .open_with(one, b"f", OFlags::RDONLY, Mode::empty())
        .expect("open");
    let ren = io.rename(one, b"f", two, b"g").expect("rename");
    let gone = io.unlink(two, b"f", false).expect("unlink");
    if back == Backend::Uring {
        assert_eq!(
            io.live_on[one as usize], 2,
            "open and rename name the first"
        );
        assert_eq!(
            io.live_on[two as usize], 2,
            "rename and unlink name the second"
        );
    }
    assert!(io.close(two));
    assert!(
        !io.is_live(ren),
        "{back:?} closing the target left the rename live"
    );
    assert!(
        !io.is_live(gone),
        "{back:?} closing the directory left the unlink live"
    );
    assert!(io.close(one));
    assert!(
        !io.is_live(open),
        "{back:?} closing the directory left the open live"
    );
    let got = finals(&mut io, Duration::from_millis(50));
    if back == Backend::Uring {
        assert!(got.is_empty(), "cancelled ops reported {got:?}");
    }
    let _ = std::fs::remove_dir_all(d1);
    let _ = std::fs::remove_dir_all(d2);
}

#[test]
fn closing_a_directory_reaches_ops_relative_to_it() {
    for b in backends() {
        check_closing_a_directory_reaches_ops_relative_to_it(b);
    }
}

/// A chained path op may look its directories up only when it starts, so closing one
/// waits for the op's last completion. Both sides of a rename count.
fn check_a_chained_path_op_holds_its_directories_open(back: Backend) {
    driver!(io, back, ops = 16, bufs = 16, size = 256);
    let (d1, one) = scratch(&mut io, "chain-one", back);
    let (d2, two) = scratch(&mut io, "chain-two", back);
    let (r, w) = io.pipe().expect("pipe");
    io.chain();
    let _gate = io.ready(r, false).expect("poll");
    io.chain();
    let open = io
        .open_with(one, b"f", OFlags::RDONLY, Mode::empty())
        .expect("open");
    let ren = io.rename(one, b"f", two, b"g").expect("rename");
    if back == Backend::Epoll {
        // Epoll runs no chain, path ops finish at the push.
        assert!(io.chain_fds.is_empty());
        return;
    }
    let names = |id: OpId, fd: RawFd| {
        io.chain_fds
            .iter()
            .any(|&(u, f)| u == id.to_user_data() && f == fd)
    };
    assert!(names(open, one), "the open did not note its directory");
    assert!(names(ren, one), "the rename did not note its source");
    assert!(names(ren, two), "the rename did not note its target");
    assert!(io.close(two));
    assert!(
        io.parked.iter().any(|(f, _)| *f == two),
        "the directory closed under a chained rename"
    );
    rustix::io::write(io.fd(w).expect("write end"), b"x").expect("open the gate");
    let start = Instant::now();
    while !io.parked.is_empty() {
        assert!(start.elapsed() < LIMIT, "the parked directory never closed");
        finals(&mut io, Duration::from_millis(5));
    }
    assert!(io.fd(two).is_none());
    let _ = std::fs::remove_dir_all(d1);
    let _ = std::fs::remove_dir_all(d2);
}

#[test]
fn a_chained_path_op_holds_its_directories_open() {
    for b in backends() {
        check_a_chained_path_op_holds_its_directories_open(b);
    }
}
