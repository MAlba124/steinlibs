//! Regressions for the queue findings of the 2026-11-23 review. A poll fires on any bit
//! it asked for, and a plain send completes short while a linked one goes whole.

use super::*;
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
use std::os::fd::OwnedFd;

fn pair() -> (OwnedFd, OwnedFd) {
    let flags = SocketFlags::CLOEXEC | SocketFlags::NONBLOCK;
    socketpair(AddressFamily::UNIX, SocketType::STREAM, flags, None).expect("socketpair")
}

/// Ticks until `user` completes or `limit` passes.
fn within(q: &mut Queue, user: u64, limit: Duration) -> Option<Cqe> {
    let start = Instant::now();
    while start.elapsed() < limit {
        q.tick(Some(Duration::from_millis(5))).expect("tick");
        while let Some(c) = q.pop() {
            if c.user == user {
                return Some(c);
            }
        }
    }
    None
}

/// Reads everything waiting on `fd`.
fn drain(fd: &OwnedFd) -> usize {
    let mut buf = vec![0u8; 1 << 16];
    let mut n = 0;
    while let Ok(got @ 1..) = rustix::io::read(fd, &mut buf) {
        n += got;
    }
    n
}

/// A poll for readable or writable joined only the read list, so a writable edge alone
/// never ran it.
fn a_combined_poll_fires_on_writable_alone(want: Want) {
    let (a, b) = pair();
    rustix::net::sockopt::set_socket_send_buffer_size(&a, 4096).expect("sndbuf");
    let junk = vec![0u8; 65536];
    while rustix::io::write(&a, &junk).is_ok() {}
    let mut q = queue(want);
    let poll = Op::Poll {
        fd: Fd::Raw(a.as_raw_fd()),
        events: (libc::POLLIN | libc::POLLOUT) as u32,
        multi: false,
    };
    q.push(poll, 1).expect("poll");
    q.tick(Some(Duration::from_millis(10))).expect("tick");
    assert!(
        q.pop().is_none(),
        "{want:?} neither readable nor writable yet"
    );
    assert!(drain(&b) > 0);
    let c = within(&mut q, 1, Duration::from_millis(500));
    let c = c.unwrap_or_else(|| panic!("{want:?} the writable edge never ran the poll"));
    assert_ne!(c.res as i16 & libc::POLLOUT, 0, "{want:?} {c:?}");
}

/// A poll for the peer's shutdown alone sat on the write list, so the read side edge
/// that carries it never ran the poll. A receive waits on the same descriptor.
fn a_shutdown_poll_fires_beside_a_receive(want: Want) {
    let (a, b) = pair();
    // Full, so the edge carries no writable bit either.
    rustix::net::sockopt::set_socket_send_buffer_size(&a, 4096).expect("sndbuf");
    let junk = vec![0u8; 65536];
    while rustix::io::write(&a, &junk).is_ok() {}
    let mut q = queue(want);
    let mut buf = [0u8; 8];
    let recv = Op::Recv {
        fd: Fd::Raw(a.as_raw_fd()),
        mem: mem(&mut buf),
    };
    q.push(recv, 1).expect("recv");
    let hup = Op::Poll {
        fd: Fd::Raw(a.as_raw_fd()),
        events: libc::POLLRDHUP as u32,
        multi: false,
    };
    q.push(hup, 2).expect("poll");
    q.tick(Some(Duration::from_millis(10))).expect("tick");
    assert!(q.pop().is_none(), "{want:?} nothing came yet");
    rustix::net::shutdown(&b, rustix::net::Shutdown::Write).expect("shutdown");
    let mut seen = Vec::new();
    assert_eq!(wait(&mut q, 1, &mut seen).result(), Ok(0), "{want:?}");
    let c = match seen.iter().position(|c| c.user == 2) {
        Some(i) => Some(seen.remove(i)),
        None => within(&mut q, 2, Duration::from_millis(500)),
    };
    let c = c.unwrap_or_else(|| panic!("{want:?} the shutdown never ran the poll"));
    assert_ne!(c.res as i16 & libc::POLLRDHUP, 0, "{want:?} {c:?}");
}

/// An unlinked send was held until all of it went, so under backpressure the queue sat on
/// a prefix the peer had already read and reported nothing.
fn a_plain_send_completes_short(want: Want) {
    const N: usize = 4 << 20;
    let (a, b) = pair();
    let mut q = queue(want);
    let mut data = vec![7u8; N];
    let send = Op::Send {
        fd: Fd::Raw(a.as_raw_fd()),
        mem: mem(&mut data),
    };
    q.push(send, 1).expect("send");
    let c = within(&mut q, 1, Duration::from_millis(500));
    let c = c.unwrap_or_else(|| panic!("{want:?} the short send never completed"));
    let sent = c.result().expect("sent");
    assert!(sent > 0 && (sent as usize) < N, "{want:?} sent {sent}");
    assert_eq!(drain(&b), sent as usize, "{want:?}");
}

/// A send linked to the next goes whole on both backends, so the next cannot start
/// before its rest.
fn a_linked_send_goes_whole(want: Want) {
    const N: usize = 1 << 20;
    let (a, b) = pair();
    rustix::net::sockopt::set_socket_send_buffer_size(&a, 4096).expect("sndbuf");
    let mut q = queue(want);
    let mut data = vec![7u8; N];
    let send = Op::Send {
        fd: Fd::Raw(a.as_raw_fd()),
        mem: mem(&mut data),
    };
    q.link();
    q.push(send, 1).expect("send");
    q.push(Op::Nop, 2).expect("nop");
    let mut got = 0;
    let mut seen = Vec::new();
    let start = Instant::now();
    while !seen.iter().any(|c: &Cqe| c.user == 1) {
        assert!(start.elapsed() < LIMIT, "{want:?} the send never completed");
        got += drain(&b);
        q.tick(Some(Duration::from_millis(1))).expect("tick");
        while let Some(c) = q.pop() {
            seen.push(c);
        }
    }
    got += drain(&b);
    let c = wait(&mut q, 1, &mut seen);
    assert_eq!(c.result(), Ok(N as u32), "{want:?}");
    assert_eq!(got, N, "{want:?}");
    assert_eq!(wait(&mut q, 2, &mut seen).result(), Ok(0), "{want:?}");
}

both!(
    a_combined_poll_fires_on_writable_alone,
    a_shutdown_poll_fires_beside_a_receive,
    a_plain_send_completes_short,
    a_linked_send_goes_whole,
);
