//! Same loopback workload on both backends. Many connections, small request/response rounds.

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use stein_io::driver::{Backend, Io, Storage};
use stein_io::op::Kind;

const CONNS: usize = 500;
const ROUNDS: usize = 8;
const MSG: &[u8] = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";

fn main() {
    println!("stein-io, {CONNS} connections x {ROUNDS} round trips\n");
    println!(
        "{:<8}  {:>10}  {:>14}  {:>10}",
        "backend", "wall", "round trips/s", "ticks"
    );
    for back in [Backend::Epoll, Backend::Uring] {
        match run(back) {
            Some((d, ticks)) => {
                let rate = (CONNS * ROUNDS) as f64 / d.as_secs_f64();
                println!(
                    "{:<8}  {:>9.1?}  {:>14.0}  {:>10}",
                    format!("{back:?}"),
                    d,
                    rate,
                    ticks
                );
            }
            None => println!("{:<8}  unavailable", format!("{back:?}")),
        }
    }
}

fn run(back: Backend) -> Option<(Duration, usize)> {
    let listener = TcpListener::bind("127.0.0.1:0").ok()?;
    let addr = listener.local_addr().ok()?;
    let server = std::thread::spawn(move || echo(listener));

    let bufs = CONNS * 2;
    let store = Storage::new(CONNS * 4, bufs);
    let mut io = Io::with_backend(store, 256, back).ok()?;

    let start = Instant::now();
    let mut ticks = 0usize;
    let mut left = CONNS * ROUNDS;
    let mut sent = vec![0usize; 64 * 1024];

    for _ in 0..CONNS {
        io.connect(addr).ok()?;
    }
    while left > 0 && start.elapsed() < Duration::from_secs(30) {
        io.tick(Some(Duration::from_millis(50))).ok()?;
        ticks += 1;
        while let Some(c) = io.next_completion() {
            step(&mut io, c, &mut sent, &mut left);
        }
    }
    let took = start.elapsed();
    drop(io);
    let _ = server.join();
    Some((took, ticks))
}

/// Connect then send, send then recv, recv then send until `ROUNDS`.
fn step(io: &mut Io, c: stein_io::op::Completion, sent: &mut [usize], left: &mut usize) {
    if c.result.is_err() {
        return;
    }
    match c.kind {
        Kind::Connect => write_one(io, c.fd),
        Kind::Send => drop(io.recv(c.fd, false)),
        Kind::Recv => got_one(io, c, sent, left),
        _ => (),
    }
}

fn got_one(io: &mut Io, c: stein_io::op::Completion, sent: &mut [usize], left: &mut usize) {
    if let Some(b) = c.buf {
        io.release(b);
    }
    *left = left.saturating_sub(1);
    let i = c.fd as usize;
    if i >= sent.len() {
        return;
    }
    sent[i] += 1;
    if sent[i] < ROUNDS {
        write_one(io, c.fd);
    }
}

fn write_one(io: &mut Io, fd: std::os::fd::RawFd) {
    let Ok(b) = io.alloc() else {
        return;
    };
    io.buf_mut(&b)[..MSG.len()].copy_from_slice(MSG);
    let _ = io.send(fd, b, MSG.len() as u32);
}

fn echo(listener: TcpListener) {
    let mut conns: Vec<TcpStream> = Vec::with_capacity(CONNS);
    for _ in 0..CONNS {
        match listener.accept() {
            Ok((s, _)) => conns.push(s),
            Err(_) => return,
        }
    }
    let mut done = 0usize;
    let mut finished = vec![false; conns.len()];
    let mut buf = [0u8; 4096];
    while done < conns.len() {
        done += sweep(&mut conns, &mut finished, &mut buf);
    }
}

/// Returns how many connections ended this pass.
fn sweep(conns: &mut [TcpStream], finished: &mut [bool], buf: &mut [u8]) -> usize {
    let mut ended = 0;
    for (i, s) in conns.iter_mut().enumerate() {
        if !finished[i] && !bounce(s, buf) {
            finished[i] = true;
            ended += 1;
        }
    }
    ended
}

/// False once the peer is done.
fn bounce(s: &mut TcpStream, buf: &mut [u8]) -> bool {
    let n = match s.read(buf) {
        Ok(0) | Err(_) => return false,
        Ok(n) => n,
    };
    s.write_all(&buf[..n]).is_ok()
}
