//! Regressions for the queue findings of the production review.

use super::*;
use std::net::TcpStream;
use std::os::fd::{AsFd, OwnedFd};

/// O_NONBLOCK is set on `fd`'s file description.
fn nonblocking(fd: impl AsFd) -> bool {
    rustix::fs::fcntl_getfl(fd)
        .expect("flags")
        .contains(OFlags::NONBLOCK)
}

/// Finding 1, a close behind a chain that succeeded runs too, after it.
#[test]
fn a_ring_close_behind_a_chain_that_succeeds_runs() {
    let setup = Setup {
        want: Want::Uring,
        ..Setup::default()
    };
    let Ok(mut q) = Queue::new(Owned::new(8, 64), setup) else {
        return;
    };
    let (rx, tx) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::NONBLOCK).expect("pipe");
    q.link();
    q.push(Op::Nop, 1).expect("nop");
    q.close(tx, 2).expect("close");
    let mut seen = Vec::new();
    assert_eq!(wait(&mut q, 1, &mut seen).result(), Ok(0));
    assert_eq!(wait(&mut q, 2, &mut seen).result(), Ok(0));
    let mut b = [0u8; 4];
    assert_eq!(rustix::io::read(&rx, &mut b), Ok(0), "the close never ran");
}

/// A listener whose accept queue is full, so a further SYN is dropped and a blocking
/// connect to it would wait for the retries.
fn full_listener() -> (OwnedFd, SocketAddr, Vec<OwnedFd>) {
    use rustix::net::{AddressFamily, SocketType, bind, getsockname, listen, socket};
    let l = socket(AddressFamily::INET, SocketType::STREAM, None).expect("socket");
    let any: SocketAddr = "127.0.0.1:0".parse().expect("addr");
    bind(&l, &any).expect("bind");
    listen(&l, 0).expect("listen");
    let to = SocketAddr::try_from(getsockname(&l).expect("name")).expect("inet");
    let fill = (0..3)
        .map(|_| {
            let fd = nonblocking_tcp();
            // SAFETY: fresh from `nonblocking_tcp`, owned here.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let _ = rustix::net::connect(&fd, &to);
            fd
        })
        .collect();
    std::thread::sleep(Duration::from_millis(100));
    (l, to, fill)
}

/// Finding 2, epoll ran a connect on a blocking socket as is, which held the loop for
/// the whole handshake. It runs non-blocking now and waits for writable.
#[test]
fn a_connect_on_a_blocking_socket_does_not_hold_the_loop() {
    let mut q = queue(Want::Epoll);
    let (_l, to, _fill) = full_listener();
    let s = rustix::net::socket(
        rustix::net::AddressFamily::INET,
        rustix::net::SocketType::STREAM,
        None,
    )
    .expect("socket");
    let start = Instant::now();
    let key = q
        .push(
            Op::Connect {
                fd: Fd::Raw(s.as_raw_fd()),
                to,
            },
            1,
        )
        .expect("connect");
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "the connect held the loop {:?}",
        start.elapsed()
    );
    assert!(!nonblocking(&s), "the socket was left non-blocking");
    if q.pop().is_none() {
        assert!(q.cancel(key));
        let c = wait(&mut q, 1, &mut Vec::new());
        assert_eq!(c.result(), Err(Errno::CANCELED));
    }
}

/// A connected pair with the client blocking and its send buffer full.
fn full_blocking_pair() -> (TcpStream, TcpStream) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let c = TcpStream::connect(l.local_addr().expect("addr")).expect("connect");
    let peer = l.accept().expect("accept").0;
    c.set_nonblocking(true).expect("nonblocking");
    let chunk = [7u8; 65536];
    while (&c).write(&chunk).is_ok() {}
    c.set_nonblocking(false).expect("blocking");
    (c, peer)
}

/// Finding 2, epoll spliced into a blocking socket as is, so a full one held the loop
/// until the peer read. The splice waits for writable now.
#[test]
fn a_splice_into_a_full_blocking_socket_waits() {
    let mut q = queue(Want::Epoll);
    let (c, mut peer) = full_blocking_pair();
    let (rx, tx) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::NONBLOCK).expect("pipe");
    rustix::io::write(&tx, b"piped").expect("fill the pipe");
    let op = Op::Splice {
        from: Fd::Raw(rx.as_raw_fd()),
        at: POSITION,
        to: Fd::Raw(c.as_raw_fd()),
        len: 5,
    };
    let start = Instant::now();
    q.push(op, 1).expect("splice");
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "the splice held the loop {:?}",
        start.elapsed()
    );
    assert!(!nonblocking(&c), "the socket was left non-blocking");
    q.tick(Some(Duration::ZERO)).expect("tick");
    assert!(q.pop().is_none(), "a full socket took the splice");
    let reader = std::thread::spawn(move || {
        let mut sink = vec![0u8; 1 << 16];
        let mut got = 0usize;
        while let Ok(n) = peer.read(&mut sink) {
            if n == 0 {
                break;
            }
            got += n;
        }
        got
    });
    assert_eq!(wait(&mut q, 1, &mut Vec::new()).result(), Ok(5));
    drop(c);
    assert!(reader.join().expect("reader") > 5);
}

/// Finding 2, epoll spliced from a blocking socket as is, so an empty one held the loop
/// until data came. It ends with `EAGAIN` now, as on a non-blocking one.
#[test]
fn a_splice_from_an_empty_blocking_socket_does_not_hold_the_loop() {
    let mut q = queue(Want::Epoll);
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let c = TcpStream::connect(l.local_addr().expect("addr")).expect("connect");
    let _peer = l.accept().expect("accept").0;
    let (_rx, tx) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::NONBLOCK).expect("pipe");
    let op = Op::Splice {
        from: Fd::Raw(c.as_raw_fd()),
        at: POSITION,
        to: Fd::Raw(tx.as_raw_fd()),
        len: 64,
    };
    let start = Instant::now();
    let r = one(&mut q, op, 1).result();
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "the splice held the loop {:?}",
        start.elapsed()
    );
    assert_eq!(r, Err(Errno::AGAIN));
    assert!(!nonblocking(&c), "the socket was left non-blocking");
}
