//! Cross-driver wake cost, post vs eventfd. Latency to a sleeping target, and ping-pong rate.
//! Run with `cargo test -p stein-io --release --test wake -- --ignored --nocapture`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rustix::event::{EventfdFlags, eventfd};
use stein_io::{Backend, Completion, Io, Kind, Mailbox, Storage};

const SAMPLES: usize = 20_000;
const RATE_FOR: Duration = Duration::from_secs(2);
/// Ends the rate run. Not `u64::MAX`, which an eventfd rejects.
const STOP: u64 = 1 << 62;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Start {
    Epoll,
    Core,
}

fn start(how: Start) -> Io {
    let store = Storage::new(64, 16);
    match how {
        Start::Epoll => Io::with_backend(store, 64, Backend::Epoll),
        Start::Core => Io::single_issuer(store, 64),
    }
    .expect("driver")
}

enum Way {
    Post(stein_io::Peer),
    Eventfd(OwnedFd),
}

/// Handed to the other side so it can wake this driver.
enum Door {
    Mailbox(Mailbox),
    Eventfd(OwnedFd),
}

fn listen(io: &mut Io, eventfds: bool) -> Door {
    if !eventfds {
        return Door::Mailbox(io.mailbox().expect("mailbox"));
    }
    let efd = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).expect("eventfd");
    let theirs = efd.as_fd().try_clone_to_owned().expect("dup");
    let raw = io.own(efd);
    io.read_stream(raw).expect("read_stream");
    Door::Eventfd(theirs)
}

fn way(io: &mut Io, door: Door) -> Way {
    match door {
        Door::Mailbox(m) => Way::Post(io.peer(m)),
        Door::Eventfd(fd) => Way::Eventfd(fd),
    }
}

fn send(io: &mut Io, way: &Way, value: u64) {
    match way {
        Way::Post(to) => {
            io.post(*to, value).expect("post");
            io.flush().expect("flush");
        }
        Way::Eventfd(fd) => {
            rustix::io::write(fd, &value.to_ne_bytes()).expect("write");
        }
    }
}

/// Sleep until a value arrives. Drops the sender's own post completions.
fn wait(io: &mut Io) -> u64 {
    loop {
        while let Some(c) = io.next_completion() {
            match c.kind {
                Kind::Msg => return c.value().expect("a value"),
                Kind::Read => return count(io, c),
                _ => (),
            }
        }
        io.tick(None).expect("tick");
    }
}

/// Eventfd value, read rearmed.
fn count(io: &mut Io, c: Completion) -> u64 {
    let b = c.buf.expect("the count");
    let mut v = [0u8; 8];
    v.copy_from_slice(&io.filled(&b)[..8]);
    io.release(b);
    io.read_stream(c.fd).expect("read_stream");
    u64::from_ne_bytes(v)
}

fn nanos(since: Instant) -> u64 {
    since.elapsed().as_nanos() as u64
}

/// One way, target asleep each time. Returns sorted nanoseconds.
fn latency(how: Start, eventfds: bool) -> Vec<u64> {
    let epoch = Instant::now();
    let (door_tx, door_rx) = mpsc::channel();
    let (ack_tx, ack_rx) = mpsc::channel::<()>();
    let target = std::thread::spawn(move || {
        let mut io = start(how);
        door_tx.send(listen(&mut io, eventfds)).expect("send");
        let mut took = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let sent = wait(&mut io);
            took.push(nanos(epoch).saturating_sub(sent));
            ack_tx.send(()).expect("ack");
        }
        took
    });
    let mut io = start(how);
    let way = way(&mut io, door_rx.recv().expect("door"));
    for _ in 0..SAMPLES {
        // Let the target get back to sleep.
        let pause = Instant::now();
        while pause.elapsed() < Duration::from_micros(150) {
            std::hint::spin_loop();
        }
        // Never zero, a zero write does not wake an eventfd.
        send(&mut io, &way, nanos(epoch).max(1));
        ack_rx.recv().expect("ack");
        while io.next_completion().is_some() {}
        let _ = io.tick(Some(Duration::ZERO));
    }
    let mut took = target.join().expect("target");
    took.sort_unstable();
    took
}

/// Ping-pong. Returns wakes per second.
fn rate(how: Start, eventfds: bool) -> f64 {
    let (door_tx, door_rx) = mpsc::channel();
    let (back_tx, back_rx) = mpsc::channel();
    let other = std::thread::spawn(move || {
        let mut io = start(how);
        door_tx.send(listen(&mut io, eventfds)).expect("send");
        let way = way(&mut io, back_rx.recv().expect("door"));
        let mut v = wait(&mut io);
        while v != STOP {
            send(&mut io, &way, v + 1);
            v = wait(&mut io);
        }
    });
    let mut io = start(how);
    back_tx.send(listen(&mut io, eventfds)).expect("send");
    let way = way(&mut io, door_rx.recv().expect("door"));
    let begin = Instant::now();
    let mut v = 1;
    while begin.elapsed() < RATE_FOR {
        send(&mut io, &way, v);
        v = wait(&mut io) + 1;
    }
    let took = begin.elapsed();
    send(&mut io, &way, STOP);
    other.join().expect("other");
    v as f64 / took.as_secs_f64()
}

#[test]
#[ignore = "a measurement, and a noisy one on a shared machine"]
fn what_a_wake_costs() {
    eprintln!(
        "{:<8} {:<8} {:>9} {:>9} {:>9} {:>9} {:>11}",
        "backend", "way", "p50 us", "p90 us", "p99 us", "min us", "wakes/s"
    );
    for how in [Start::Core, Start::Epoll] {
        for eventfds in [false, true] {
            let took = latency(how, eventfds);
            let at = |q: f64| took[((took.len() - 1) as f64 * q) as usize] as f64 / 1e3;
            let label = match how {
                Start::Core => "ring",
                Start::Epoll => "epoll",
            };
            let kind = match (eventfds, how) {
                (true, _) => "eventfd",
                (false, Start::Core) => "MSG_RING",
                (false, Start::Epoll) => "pipe",
            };
            eprintln!(
                "{label:<8} {kind:<8} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>11.0}",
                at(0.5),
                at(0.9),
                at(0.99),
                at(0.0),
                rate(how, eventfds)
            );
        }
    }
}
