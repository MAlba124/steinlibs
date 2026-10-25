//! Regressions for the driver findings of the third unsafe review.

use super::*;
use crate::{Mode, OFlags};
use std::os::unix::net::UnixStream;

/// Tick until a completion is queued, without taking it.
fn queued(io: &mut Io) {
    let start = Instant::now();
    while io.done.is_empty() {
        assert!(start.elapsed() < LIMIT, "nothing completed");
        io.tick(Some(Duration::from_millis(10))).expect("tick");
    }
}

/// Reap queue completions directly until one for `user` comes, so the driver never sees it.
fn intercept(io: &mut Io, user: u64) -> Cqe {
    let start = Instant::now();
    loop {
        let mut got = [Cqe::EMPTY; 16];
        let n = io.q.reap(&mut got);
        if let Some(c) = got[..n].iter().find(|c| c.user == user) {
            return *c;
        }
        assert!(start.elapsed() < LIMIT, "{user:#x} never completed");
        io.q.tick(Some(Duration::from_millis(10))).expect("tick");
    }
}

/// Fill every queue slot with a long timeout the driver ignores. Returns their keys.
fn fill_queue(io: &mut Io) -> Vec<queue::Key> {
    let mut keys = Vec::new();
    let hour = queue::Op::Timeout {
        after: Duration::from_secs(3600),
    };
    // A close-tagged user, which the driver drops on completion.
    while let Ok(k) = io.q.push(hour, CLOSING | 1) {
        keys.push(k);
    }
    io.q.submit().expect("submit");
    keys
}

/// Cancel the filler timeouts and wait until their slots are free.
fn empty_queue(io: &mut Io, keys: Vec<queue::Key>) {
    let n = keys.len();
    for k in keys {
        io.q.cancel(k);
    }
    let mut gone = 0;
    let start = Instant::now();
    while gone < n {
        assert!(start.elapsed() < LIMIT, "fillers never ended");
        io.q.tick(Some(Duration::from_millis(10))).expect("tick");
        let mut got = [Cqe::EMPTY; 64];
        let k = io.q.reap(&mut got);
        gone += got[..k].iter().filter(|c| c.user == CLOSING | 1).count();
    }
}

/// Finding 2, `adopt` gave an open its fd without counting it in `live_on`, so taking
/// the open's completion hid another op on that fd from `close`.
fn check_close_cancels_an_op_on_an_opened_fd(back: Backend) {
    let fifo = std::env::temp_dir().join(format!("stein-io-r3-{}-{back:?}", std::process::id()));
    let _ = std::fs::remove_file(&fifo);
    let mode = rustix::fs::Mode::from_raw_mode(0o600);
    rustix::fs::mknodat(rustix::fs::CWD, &fifo, rustix::fs::FileType::Fifo, mode, 0).expect("fifo");
    // A writer keeps the read waiting rather than at end of file.
    let writer = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fifo)
        .expect("writer");
    driver!(io, back, ops = 16, bufs = 32, size = 4096);
    let path = fifo.as_os_str().as_encoded_bytes();
    let open = io
        .open_with(CWD, path, OFlags::RDONLY, Mode::empty())
        .expect("open");
    queued(&mut io);
    let fd = io.op_fd(open).expect("an fd before the open is taken");
    let read = io.read_stream(fd).expect("read");
    while let Some(c) = io.next_completion() {
        if c.op == open {
            break;
        }
    }
    io.tick(Some(Duration::from_millis(20))).expect("tick");
    while io.next_completion().is_some() {}
    assert!(io.is_live(read), "{back:?} the read ended early");
    assert!(io.close(fd));
    let live = io.is_live(read);
    drop(writer);
    let _ = std::fs::remove_file(&fifo);
    assert!(!live, "{back:?} close left an op on the closed fd live");
}

/// Finding 3, a read at the position on a blocking pipe blocked the epoll loop, and
/// `Io::pipe` makes blocking pipes.
fn check_a_read_on_a_blocking_pipe_waits(back: Backend) {
    driver!(io, back, ops = 8, bufs = 8, size = 64);
    let (r, w) = io.pipe().expect("pipe");
    let read = io.read_stream(r).expect("read");
    io.tick(Some(Duration::from_millis(10))).expect("tick");
    assert!(io.is_live(read), "{back:?} the read ended on an empty pipe");
    let b = io.alloc().expect("buf");
    io.buf_mut(&b)[..3].copy_from_slice(b"abc");
    io.write(w, b, 3, APPEND).expect("write");
    let mut got = None;
    while got.is_none() {
        let c = pump(&mut io).expect("a completion");
        if c.op == read {
            got = Some(c);
        } else if let Some(b) = c.buf {
            io.release(b);
        }
    }
    let c = got.expect("read");
    assert_eq!(c.result, Ok(3), "{back:?}");
    let b = c.buf.expect("buffer");
    assert_eq!(io.filled(&b), b"abc");
    io.release(b);
}

/// Finding 4, a connect whose op could not be inserted kept its socket in `owned`, a
/// number the caller never saw, so it leaked until the driver dropped.
fn check_a_refused_connect_closes_its_socket(back: Backend) {
    let (_l, addr) = listener();
    driver!(io, back, ops = 1, bufs = 4, size = 64);
    let _t = io.timeout(Duration::from_secs(60)).expect("timeout");
    let before = io.owned.iter().flatten().count();
    let r = io.connect(addr);
    assert!(matches!(r, Err(IoError::OpsFull)), "{back:?} {r:?}");
    let after = io.owned.iter().flatten().count();
    assert_eq!(after, before, "{back:?} the socket stayed owned");
}

/// Finding 4, `Peer` had public fields and a copy outlived its close, so a post wrote
/// eight bytes to whatever took the number next. Posts go only to adopted peers now.
#[test]
fn a_post_to_a_closed_peer_reaches_no_other_descriptor() {
    driver!(target, Backend::Epoll, ops = 4, bufs = 4, size = 64);
    driver!(io, Backend::Epoll, ops = 8, bufs = 4, size = 64);
    let p = io.peer(target.mailbox().expect("mailbox"));
    assert!(!p.is_ring());
    let fd = p.fd();
    assert!(io.close(fd));
    let (a, mut b) = UnixStream::pair().expect("pair");
    b.set_nonblocking(true).expect("nonblocking");
    // Lowest free number from `fd` up, so another thread's descriptor is never replaced.
    let dup = rustix::io::fcntl_dupfd_cloexec(&a, fd).expect("dup");
    let reused = dup.as_raw_fd() == fd;
    io.own(dup);
    let r = io.post(p, 7);
    assert!(
        matches!(r, Err(IoError::Kernel(Errno::BADF))),
        "a stale peer was posted to, {r:?}"
    );
    let mut got = [0u8; 8];
    let reached = b.read(&mut got).is_ok();
    assert!(!(reused && reached), "eight bytes reached another socket");
}

/// Finding 4, a waker poll that could not be pushed again was ignored, so every later
/// wake was lost. The failure is delivered and the next waker starts over.
fn check_a_waker_that_cannot_rearm_says_so(back: Backend) {
    driver!(io, back, ops = 4, bufs = 4, size = 64);
    let w = io.waker().expect("waker");
    w.wake();
    let c = intercept(&mut io, WAKE);
    let keys = fill_queue(&mut io);
    io.complete(c);
    let got = io.next_completion().expect("a wake");
    assert_eq!(got.kind, Kind::Wake);
    assert!(got.result.is_err(), "{back:?} a lost re-arm read as a wake");
    empty_queue(&mut io, keys);
    while io.next_completion().is_some() {}
    let w = io.waker().expect("waker again");
    w.wake();
    let c = pump(&mut io).expect("a wake after starting over");
    assert_eq!(c.kind, Kind::Wake, "{back:?}");
    assert_eq!(c.result, Ok(0), "{back:?}");
}

/// Finding 4, as for the waker, a mailbox poll that could not be pushed again lost
/// every later value. Epoll only, a ring mailbox needs no poll.
#[test]
fn a_mailbox_that_cannot_rearm_says_so() {
    driver!(io, Backend::Epoll, ops = 4, bufs = 4, size = 64);
    driver!(sender, Backend::Epoll, ops = 4, bufs = 4, size = 64);
    let p = sender.peer(io.mailbox().expect("mailbox"));
    sender.post(p, 7).expect("post");
    let c = intercept(&mut io, MAIL);
    let keys = fill_queue(&mut io);
    io.complete(c);
    let first = io.next_completion().expect("the value");
    assert_eq!((first.kind, first.op.to_user_data()), (Kind::Msg, 7));
    let failed = io.next_completion().expect("the failure");
    assert_eq!(failed.kind, Kind::Msg);
    assert!(failed.result.is_err(), "a lost re-arm went unsaid");
    empty_queue(&mut io, keys);
    while io.next_completion().is_some() {}
    let p = sender.peer(io.mailbox().expect("mailbox again"));
    sender.post(p, 8).expect("post");
    let c = pump(&mut io).expect("a value after starting over");
    assert_eq!((c.kind, c.op.to_user_data()), (Kind::Msg, 8));
}

/// Finding 4, a failed waker poll dropped its eventfd without `forget`, so epoll kept the
/// number's stale watch and a new descriptor on it was never registered.
#[test]
fn a_failed_waker_poll_forgets_its_number() {
    driver!(io, Backend::Epoll, ops = 8, bufs = 8, size = 64);
    let w = io.waker().expect("waker");
    let fd = io.wake.as_ref().expect("eventfd").as_raw_fd();
    w.wake();
    let mut c = intercept(&mut io, WAKE);
    c.res = -Errno::BADF.raw_os_error();
    io.complete(c);
    let got = io.next_completion().expect("the failure");
    assert!(got.result.is_err());
    assert!(!io.q.is_watched(fd), "epoll kept the closed number's watch");
    let (r, mut wr) = UnixStream::pair().expect("pair");
    // Lowest free number from `fd` up, so another thread's descriptor is never replaced.
    let dup = rustix::io::fcntl_dupfd_cloexec(&r, fd).expect("dup");
    let fd = io.own(dup);
    let read = io.read_stream(fd).expect("read");
    wr.write_all(b"x").expect("write");
    let c = pump(&mut io).expect("the read on the reused number");
    assert_eq!((c.op, c.result), (read, Ok(1)));
    if let Some(b) = c.buf {
        io.release(b);
    }
}

#[test]
fn close_cancels_an_op_on_an_opened_fd() {
    for b in backends() {
        check_close_cancels_an_op_on_an_opened_fd(b);
    }
}

#[test]
fn a_read_on_a_blocking_pipe_waits() {
    for b in backends() {
        check_a_read_on_a_blocking_pipe_waits(b);
    }
}

#[test]
fn a_refused_connect_closes_its_socket() {
    for b in backends() {
        check_a_refused_connect_closes_its_socket(b);
    }
}

#[test]
fn a_waker_that_cannot_rearm_says_so() {
    for b in backends() {
        check_a_waker_that_cannot_rearm_says_so(b);
    }
}
