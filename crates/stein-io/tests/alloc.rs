//! Zero steady-state allocations per op on the driver, both backends.
//! Own binary since the counter is global. Counts this thread only, after warmup rounds.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use stein_arena::counting;
use stein_io::msg::Control;
use stein_io::{
    Backend, CWD, Completion, Io, Kind, Mode, OFlags, OpId, Peer, Region, RingFlags, Storage,
};

#[global_allocator]
static ALLOC: stein_arena::Counting = stein_arena::Counting;

const ROUNDS: usize = 200;
const SIZE: usize = 512;

fn storage(ops: usize, bufs: usize) -> Storage {
    Storage::new(ops, bufs)
}

fn pump(io: &mut Io) -> Completion {
    let start = Instant::now();
    loop {
        if let Some(c) = io.next_completion() {
            return c;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "no completion");
        io.tick(Some(Duration::from_millis(20))).expect("tick");
    }
}

/// Asserts `id` completes next.
fn done(io: &mut Io, id: OpId) -> Completion {
    let c = pump(io);
    assert_eq!(c.op, id, "another operation completed first");
    c
}

fn ok(io: &mut Io, id: OpId) -> u32 {
    done(io, id).result.expect("the operation failed")
}

/// Per-backend fixtures, built before counting.
struct Bench {
    io: Io,
    other: Io,
    to: Peer,
    back: Backend,
    dir_path: Vec<u8>,
    dir: RawFd,
    slab: Region,
    names: Region,
    sock: RawFd,
    peer: TcpStream,
    msg_sock: RawFd,
    msg_peer: TcpStream,
    msg_op: OpId,
    pipe: (RawFd, RawFd),
    udp: (RawFd, RawFd),
    udp_op: OpId,
}

fn pair(io: &mut Io) -> (RawFd, TcpStream) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    io.connect(l.local_addr().expect("addr")).expect("connect");
    let c = pump(io);
    assert_eq!(c.kind, Kind::Connect);
    let peer = l.accept().expect("accept").0;
    // Back-to-back small writes, avoid Nagle.
    peer.set_nodelay(true).expect("nodelay");
    (c.fd, peer)
}

fn bench(back: Backend) -> Bench {
    let dir = std::env::temp_dir().join(format!("stein-io-alloc-{}-{back:?}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("body"), b"a file's body").expect("write");
    let dir_path = dir.as_os_str().as_encoded_bytes().to_vec();

    let mut io = Io::with_backend(storage(64, 64), SIZE as u32, back).expect("driver");
    let mut other = Io::with_backend(storage(16, 16), SIZE as u32, back).expect("driver");
    let to = io.peer(other.mailbox().expect("mailbox"));
    let slab = io
        .add_region(vec![0x5au8; 1 << 16].into_boxed_slice())
        .expect("region");
    let names = io
        .add_region(vec![0u8; 4096].into_boxed_slice())
        .expect("region");
    io.open_with(CWD, &dir_path, OFlags::DIRECTORY, Mode::empty())
        .expect("open");
    let dir = pump(&mut io).result.expect("the directory opens") as RawFd;
    let (sock, peer) = pair(&mut io);
    let (msg_sock, msg_peer) = pair(&mut io);
    let msg_op = io.recv_msg(msg_sock).expect("recv_msg");
    let pipe = io.pipe().expect("pipe");
    let rx = io.bind_udp("127.0.0.1:0".parse().unwrap()).expect("bind");
    let tx = io
        .connect_udp(io.local_addr(rx).expect("addr"))
        .expect("connect");
    let udp_op = io.recv_msg(rx).expect("recv_msg");
    Bench {
        io,
        other,
        to,
        back,
        dir_path,
        dir,
        slab,
        names,
        sock,
        peer,
        msg_sock,
        msg_peer,
        msg_op,
        pipe,
        udp: (rx, tx),
        udp_op,
    }
}

fn files(b: &mut Bench) {
    let io = &mut b.io;
    let new = OFlags::RDWR | OFlags::CREATE | OFlags::EXCL;
    let id = io
        .open_with(b.dir, b"seg.tmp", new, Mode::from_raw_mode(0o644))
        .expect("create");
    let fd = ok(io, id) as RawFd;
    let id = io.write_mem(fd, 0, b.slab, 100, 20_000).expect("write");
    assert_eq!(ok(io, id), 20_000);
    let id = io.sync(fd, true).expect("sync");
    ok(io, id);
    let id = io.read_mem(fd, 500, b.slab, 30_000, 4096).expect("read");
    assert_eq!(ok(io, id), 4096);
    let id = io.truncate(fd, 9000).expect("truncate");
    ok(io, id);
    let id = io.stat_fd(fd).expect("stat");
    let c = done(io, id);
    let meta = c.buf.expect("a stat delivers a buffer");
    assert_eq!(stein_io::meta(io.filled(&meta)).expect("meta").size, 9000);
    io.release(meta);
    let id = io.rename(b.dir, b"seg.tmp", b.dir, b"seg").expect("rename");
    ok(io, id);
    let id = io.sync(b.dir, false).expect("sync dir");
    ok(io, id);

    // Listing needs its own fd since it reads to the end.
    let id = io
        .open_with(CWD, &b.dir_path, OFlags::DIRECTORY, Mode::empty())
        .expect("open");
    let list = ok(io, id) as RawFd;
    let mut seen = 0;
    loop {
        let id = io.list(list, b.names, 0, 4096).expect("list");
        let n = ok(io, id) as usize;
        if n == 0 {
            break;
        }
        seen += stein_io::dir::entries(&io.region(b.names)[..n]).count();
    }
    assert_eq!(seen, 2);
    assert!(io.close(list));

    let id = io.unlink(b.dir, b"seg", false).expect("unlink");
    ok(io, id);
    assert!(io.close(fd));

    let open = io
        .open_with(b.dir, b"body", OFlags::RDONLY, Mode::empty())
        .expect("open");
    let stat = io.stat_opened(open).expect("stat");
    let body = ok(io, open) as RawFd;
    let c = done(io, stat);
    let meta = c.buf.expect("a stat delivers a buffer");
    assert_eq!(stein_io::meta(io.filled(&meta)).expect("meta").size, 13);
    io.release(meta);
    assert!(io.close(body));
}

/// Open with `stat_opened`, read, batched close.
fn request(io: &mut Io, path: &[u8]) {
    let open = io.open(path).expect("open");
    let stat = io.stat_opened(open).expect("stat");
    let fd = ok(io, open) as RawFd;
    let c = done(io, stat);
    io.release(c.buf.expect("a stat delivers a buffer"));
    let id = io.read_at(fd, 0).expect("read");
    let c = done(io, id);
    assert_eq!(c.result, Ok(13));
    io.release(c.buf.expect("a read delivers a buffer"));
    assert!(io.close(fd));
    assert!(io.pending(), "the close is an entry");
    io.tick(Some(Duration::ZERO)).expect("tick");
    assert!(!io.pending());
    assert!(io.next_completion().is_none(), "a close reported");
}

/// Sockopts, sized receive, recvmsg stop and rearm, splice.
fn sockets(b: &mut Bench, round: usize) {
    let io = &mut b.io;
    let value = (round as i32 & 1).to_ne_bytes();
    io.chain();
    let first = option(io, b.sock, 6, 1, &value);
    let second = option(io, b.sock, 1, 9, &value);
    ok(io, first);
    ok(io, second);

    // Arrives in two parts.
    let buf = io.alloc().expect("buffer");
    let id = io.recv_into(b.sock, buf, 8, 10).expect("recv_into");
    b.peer.write_all(b"01234").expect("write");
    io.tick(Some(Duration::ZERO)).expect("tick");
    b.peer.write_all(b"56789").expect("write");
    let c = done(io, id);
    assert_eq!(c.result, Ok(10));
    let got = c.buf.expect("the buffer comes back");
    assert_eq!(&io.filled(&got)[8..], b"0123456789");
    io.release(got);

    b.msg_peer.write_all(b"a record").expect("write");
    let c = done(io, b.msg_op);
    let got = c.buf.expect("buffer");
    assert_eq!(io.msg(&got).expect("a message").payload, b"a record");
    io.release(got);
    assert!(io.stop(b.msg_op));
    let c = done(io, b.msg_op);
    assert!(c.is_final());
    b.msg_op = io.recv_msg(b.msg_sock).expect("recv_msg");

    // File to socket through the pipe.
    let id = io
        .open_with(b.dir, b"body", OFlags::RDONLY, Mode::empty())
        .expect("open");
    let file = ok(io, id) as RawFd;
    io.chain();
    let into = io.splice(file, 0, b.pipe.1, 4).expect("splice in");
    let out = io
        .splice(b.pipe.0, stein_io::APPEND, b.sock, 4)
        .expect("splice out");
    assert_eq!(ok(io, into), 4);
    assert_eq!(ok(io, out), 4);
    let mut four = [0u8; 4];
    b.peer.read_exact(&mut four).expect("read");
    let id = io.ready(b.sock, true).expect("ready");
    ok(io, id);
    assert!(io.close(file));

    if b.back == Backend::Uring {
        datagram(b);
    }
}

fn datagram(b: &mut Bench) {
    let io = &mut b.io;
    let buf = io.alloc().expect("buffer");
    io.buf_mut(&buf)[..3].copy_from_slice(b"tos");
    let control = Control {
        level: 0,
        kind: 1,
        data: &0x28i32.to_ne_bytes(),
    };
    let sent = io.send_msg(b.udp.1, buf, 3, control).expect("send_msg");
    for _ in 0..2 {
        let c = pump(io);
        if c.op == sent {
            assert_eq!(c.result, Ok(3));
            continue;
        }
        assert_eq!(c.op, b.udp_op);
        let got = c.buf.expect("buffer");
        assert_eq!(io.msg(&got).expect("a message").payload, b"tos");
        io.release(got);
    }
}

fn option(io: &mut Io, fd: RawFd, level: i32, name: i32, value: &[u8]) -> OpId {
    let buf = io.alloc().expect("buffer");
    io.buf_mut(&buf)[..value.len()].copy_from_slice(value);
    io.set_sockopt(fd, level, name, buf, value.len() as u32)
        .expect("set_sockopt")
}

fn wakes(b: &mut Bench, round: usize) {
    let id = b.io.post(b.to, round as u64).expect("post");
    ok(&mut b.io, id);
    let c = pump(&mut b.other);
    assert_eq!((c.kind, c.value()), (Kind::Msg, Some(round as u64)));
    assert_eq!(b.io.cq_overflow(), 0);
    assert!(!b.io.cq_backlog());
}

fn round(b: &mut Bench, n: usize) {
    files(b);
    sockets(b, n);
    wakes(b, n);
}

#[test]
fn the_new_operations_do_not_allocate() {
    let mut backs = vec![bench(Backend::Epoll)];
    let probe = Io::with_backend(storage(8, 16), SIZE as u32, Backend::Uring);
    if probe.is_ok() {
        backs.push(bench(Backend::Uring));
    }
    drop(probe);
    let mut core = Io::single_issuer(storage(16, 16), SIZE as u32).expect("driver");
    let served = [backs[0].dir_path.as_slice(), b"/body"].concat();
    // Without deferred flags there is no batched close to test.
    let cored = core.ring_flags() == RingFlags::Deferred;
    // Two warmup rounds so lazily sized tables settle.
    for b in &mut backs {
        round(b, 0);
        round(b, 1);
    }
    for _ in 0..2 * usize::from(cored) {
        request(&mut core, &served);
    }
    counting::arm();
    if cored {
        let before = counting::thread_count();
        for _ in 0..ROUNDS {
            request(&mut core, &served);
        }
        let made = counting::thread_count() - before;
        assert_eq!(made, 0, "a core's ring allocated {made} times");
        assert_eq!(core.in_flight(), 0);
    }
    for b in &mut backs {
        let before = counting::thread_count();
        for n in 0..ROUNDS {
            round(b, n);
        }
        let made = counting::thread_count() - before;
        assert_eq!(
            made, 0,
            "{:?} allocated {made} times in {ROUNDS} rounds",
            b.back
        );
        // Only the two armed receives.
        assert_eq!(b.io.in_flight(), 2);
    }
    for b in backs {
        let dir = String::from_utf8(b.dir_path.clone()).expect("a path");
        let _ = std::fs::remove_dir_all(dir);
    }
}
