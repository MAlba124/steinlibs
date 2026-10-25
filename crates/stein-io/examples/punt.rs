//! Which ring ops the kernel punts to `iou-wrk` threads, via context switches and worker runtime.
//! Run with `cargo run --release -p stein-io --example punt`.

use std::ffi::{CStr, CString};
use std::os::fd::{AsFd, IntoRawFd, OwnedFd, RawFd};
use std::path::Path;

use io_uring::{IoUring, opcode, squeue, types};
use rustix::fs::{AtFlags, Mode, OFlags, StatxFlags};

const TURNS: u64 = 20_000;
const AT_EMPTY_PATH: i32 = 0x1000;
const STATX_BASIC: u32 = 0x7ff;
const RESOLVE_CACHED: u64 = 0x20;
const O_CLOEXEC: i32 = 0o2000000;
const CWD: types::Fd = types::Fd(-100);

/// This thread's context switches, plus worker count and runtime.
#[derive(Clone, Copy, Default)]
struct Sample {
    voluntary: u64,
    preempted: u64,
    workers: u64,
    worker_ns: u64,
}

fn field(text: &str, name: &str) -> u64 {
    text.lines()
        .find_map(|l| l.strip_prefix(name))
        .and_then(|v| v.trim_start_matches(':').trim().parse().ok())
        .unwrap_or(0)
}

/// Runtime of an `iou-wrk` thread, None for others.
fn worker_ns(task: &Path) -> Option<u64> {
    let comm = std::fs::read_to_string(task.join("comm")).ok()?;
    if !comm.starts_with("iou-wrk") {
        return None;
    }
    let stat = std::fs::read_to_string(task.join("schedstat")).ok()?;
    stat.split_whitespace().next()?.parse().ok()
}

fn sample() -> Sample {
    let status = std::fs::read_to_string("/proc/thread-self/status").unwrap_or_default();
    let mut s = Sample {
        voluntary: field(&status, "voluntary_ctxt_switches"),
        preempted: field(&status, "nonvoluntary_ctxt_switches"),
        ..Sample::default()
    };
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return s;
    };
    for ns in tasks.flatten().filter_map(|t| worker_ns(&t.path())) {
        s.workers += 1;
        s.worker_ns += ns;
    }
    s
}

/// Submit one entry, wait, return its result.
fn one(ring: &mut IoUring, e: squeue::Entry) -> i32 {
    // SAFETY: entry pointees outlive this call, which returns after completion.
    if unsafe { ring.submission().push(&e) }.is_err() {
        return -1;
    }
    if ring.submit_and_wait(1).is_err() {
        return -1;
    }
    ring.completion().next().map_or(-1, |c| c.result())
}

fn plain_open(path: &CStr) -> Option<OwnedFd> {
    rustix::fs::open(path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()).ok()
}

fn plain_close(fd: i32) {
    if fd >= 0 {
        // SAFETY: freshly returned fd, owned by nothing else.
        unsafe { libc::close(fd) };
    }
}

/// Run `turn` `TURNS` times and print per-turn cost. `turn` returns success.
fn measure(name: &str, mut turn: impl FnMut(u64) -> bool) {
    let before = sample();
    let failed = (0..TURNS).filter(|i| !turn(*i)).count();
    let now = sample();
    let per = |a: u64, b: u64| (a - b) as f64 / TURNS as f64;
    println!(
        "{name:<34} {:>9.3} {:>9.3} {:>8} {:>10.0} {:>7}",
        per(now.voluntary, before.voluntary),
        per(now.preempted, before.preempted),
        now.workers,
        per(now.worker_ns, before.worker_ns),
        failed,
    );
}

fn c_path(p: &Path) -> CString {
    CString::new(p.as_os_str().as_encoded_bytes()).unwrap_or_default()
}

fn stats(ring: &mut IoUring, path: &CStr, held: RawFd) {
    // `struct statx` is 256 bytes.
    let mut room = [0u64; 32];
    let sx: *mut types::statx = room.as_mut_ptr().cast();
    measure("statx by path", |_| {
        let e = opcode::Statx::new(CWD, path.as_ptr(), sx).mask(STATX_BASIC);
        one(ring, e.build()) >= 0
    });
    measure("statx of a descriptor", |_| {
        let e = opcode::Statx::new(types::Fd(held), c"".as_ptr(), sx)
            .flags(AT_EMPTY_PATH)
            .mask(STATX_BASIC);
        one(ring, e.build()) >= 0
    });
}

fn opens(ring: &mut IoUring, dir: &Path, path: &CStr) {
    measure("openat, plain close", |_| {
        let e = opcode::OpenAt::new(CWD, path.as_ptr()).flags(O_CLOEXEC);
        let fd = one(ring, e.build());
        plain_close(fd);
        fd >= 0
    });
    let how = types::OpenHow::new()
        .flags(O_CLOEXEC as u64)
        .resolve(RESOLVE_CACHED);
    measure("openat2 cached only, plain close", |_| {
        let fd = one(ring, opcode::OpenAt2::new(CWD, path.as_ptr(), &how).build());
        plain_close(fd);
        fd >= 0
    });
    // Fresh names are dcache misses, not even negative entries.
    measure("openat of a name never seen", |i| {
        let p = c_path(&dir.join(format!("missing-{i}")));
        let e = opcode::OpenAt::new(CWD, p.as_ptr()).flags(O_CLOEXEC);
        one(ring, e.build()) == -libc::ENOENT
    });
    measure("openat2 cached only, never seen", |i| {
        let p = c_path(&dir.join(format!("absent-{i}")));
        one(ring, opcode::OpenAt2::new(CWD, p.as_ptr(), &how).build()) == -libc::EAGAIN
    });
}

fn reads_and_closes(ring: &mut IoUring, path: &CStr, held: RawFd) {
    let mut buf = vec![0u8; 4096];
    measure("read of a cached file", |_| {
        let e = opcode::Read::new(types::Fd(held), buf.as_mut_ptr(), 4096).offset(0);
        one(ring, e.build()) == 4096
    });
    measure("close, plain open", |_| {
        let fd = plain_open(path).map_or(-1, IntoRawFd::into_raw_fd);
        one(ring, opcode::Close::new(types::Fd(fd)).build()) >= 0
    });
    let Ok((a, _b)) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::empty(),
        None,
    ) else {
        return;
    };
    measure("close of a socket", |_| {
        let fd = rustix::io::dup(&a).map_or(-1, IntoRawFd::into_raw_fd);
        one(ring, opcode::Close::new(types::Fd(fd)).build()) >= 0
    });
}

fn main() {
    let dir = std::env::temp_dir().join(format!("stein-punt-{}", std::process::id()));
    let file = dir.join("file");
    let made = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&file, vec![7u8; 4096]));
    let path = c_path(&file);
    let Some(held) = made.ok().and_then(|()| plain_open(&path)) else {
        return eprintln!("no scratch file");
    };
    let mut b = IoUring::builder();
    b.setup_single_issuer()
        .setup_defer_taskrun()
        .setup_taskrun_flag();
    let Ok(mut ring) = b.build(64) else {
        return eprintln!("no ring with DEFER_TASKRUN");
    };
    let raw = std::os::fd::AsRawFd::as_raw_fd(&held);

    println!("{TURNS} turns each, per operation\n");
    println!(
        "{:<34} {:>9} {:>9} {:>8} {:>10} {:>7}",
        "operation", "voluntary", "preempted", "workers", "worker ns", "failed"
    );
    stats(&mut ring, &path, raw);
    opens(&mut ring, &dir, &path);
    reads_and_closes(&mut ring, &path, raw);
    // Plain syscall baseline.
    measure("statx(2) of a descriptor", |_| {
        rustix::fs::statx(
            held.as_fd(),
            "",
            AtFlags::EMPTY_PATH,
            StatxFlags::BASIC_STATS,
        )
        .is_ok()
    });
    let _ = std::fs::remove_dir_all(&dir);
}
