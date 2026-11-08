//! [`Op::Open2`] on both backends, a walk kept beneath its dir by resolve flags.

use super::*;
use std::os::unix::net::UnixStream;

/// `jail/` with `in`, `sub/`, a link to `in`, and links out to `outside`, one relative
/// and one absolute. Returns the root and the jail.
fn tree(want: Want) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = tmp("open2", want);
    let _ = std::fs::remove_dir_all(&root);
    let jail = root.join("jail");
    std::fs::create_dir_all(jail.join("sub")).expect("mkdir");
    std::fs::write(root.join("outside"), b"outside").expect("write");
    std::fs::write(jail.join("in"), b"inside").expect("write");
    std::os::unix::fs::symlink("in", jail.join("link_in")).expect("link");
    std::os::unix::fs::symlink("../outside", jail.join("link_up")).expect("link");
    std::os::unix::fs::symlink(root.join("outside"), jail.join("link_abs")).expect("link");
    (root, jail)
}

fn open2(q: &mut Queue, dir: RawFd, name: &[u8], resolve: ResolveFlags, user: u64) -> Cqe {
    let mut p = name.to_vec();
    p.push(0);
    let op = Op::Open2 {
        dir,
        path: mem(&mut p),
        flags: OFlags::RDONLY | OFlags::CLOEXEC,
        mode: Mode::empty(),
        resolve,
    };
    one(q, op, user)
}

/// Reads the opened file whole, then closes it through the queue.
fn read_all(q: &mut Queue, fd: RawFd, user: u64) -> Vec<u8> {
    let mut back = [0u8; 32];
    let r = Op::Read {
        fd: Fd::Raw(fd),
        mem: mem(&mut back),
        at: 0,
    };
    let n = one(q, r, user).result().expect("read") as usize;
    assert_eq!(close(q, fd, user + 1).result(), Ok(0));
    back[..n].to_vec()
}

fn an_open_beneath_a_dir_stays_inside(want: Want) {
    let mut q = queue(want);
    let (root, jail) = tree(want);
    let dir = std::fs::File::open(&jail).expect("jail");
    let d = dir.as_raw_fd();
    let beneath = ResolveFlags::BENEATH;

    for (i, inside) in [&b"in"[..], b"sub/../in", b"link_in"]
        .into_iter()
        .enumerate()
    {
        let user = 10 * i as u64;
        let c = open2(&mut q, d, inside, beneath, user);
        let fd = c
            .result()
            .unwrap_or_else(|e| panic!("{want:?} {inside:?} {e:?}")) as RawFd;
        assert_eq!(read_all(&mut q, fd, user + 1), b"inside", "{want:?}");
    }

    let abs = path(&root.join("outside"));
    let abs = &abs[..abs.len() - 1];
    for (i, out) in [&b"../outside"[..], abs, b"link_up", b"link_abs"]
        .into_iter()
        .enumerate()
    {
        let c = open2(&mut q, d, out, beneath, 100 + i as u64);
        assert_eq!(c.result(), Err(Errno::XDEV), "{want:?} {out:?} got out");
    }
    let c = open2(&mut q, d, b"absent", beneath, 200);
    assert_eq!(c.result(), Err(Errno::NOENT), "{want:?}");

    // Without resolve flags the same walk goes out.
    let fd = open2(&mut q, d, b"link_up", ResolveFlags::empty(), 300)
        .result()
        .expect("open") as RawFd;
    assert_eq!(read_all(&mut q, fd, 301), b"outside", "{want:?}");
    assert_eq!(q.in_flight(), 0);
    let _ = std::fs::remove_dir_all(&root);
}

/// The path is the queue's to read until the completion, as an [`Op::Open`]'s is.
fn an_open2_path_is_held_until_its_completion(want: Want) {
    let mut q = queue(want);
    let slab: &'static mut [u8] = Box::leak(vec![0u8; 256].into_boxed_slice());
    slab[..10].copy_from_slice(b"/dev/null\0");
    // SAFETY: leaked, valid for the process and never touched by Rust again.
    let r = unsafe { q.register(slab.as_mut_ptr(), slab.len(), false) }.expect("region");
    let op = |q: &Queue, at: usize, len: usize| Op::Open2 {
        dir: CWD,
        path: q.mem(r, at, len).expect("mem"),
        flags: OFlags::RDONLY | OFlags::CLOEXEC,
        mode: Mode::empty(),
        resolve: ResolveFlags::NO_MAGICLINKS,
    };

    q.push(op(&q, 0, 10), 1).expect("open2");
    if want == Want::Uring {
        let busy = q.unregister(r);
        assert!(
            matches!(busy, Err(IoError::RegionBusy { at: 0, len: 10 })),
            "the path went before the open landed, {busy:?}"
        );
    }
    let fd = wait(&mut q, 1, &mut Vec::new()).result().expect("open") as RawFd;
    assert_eq!(close(&mut q, fd, 2).result(), Ok(0));

    // A path over a range an op in flight writes is refused.
    let (a, _b) = UnixStream::pair().expect("pair");
    a.set_nonblocking(true).expect("nonblocking");
    let recv = Op::Recv {
        fd: Fd::Raw(a.as_raw_fd()),
        mem: q.mem(r, 64, 64).expect("mem"),
    };
    let key = q.push(recv, 3).expect("recv");
    q.submit().expect("submit");
    let busy = q.push(op(&q, 100, 8), 4);
    assert!(
        matches!(busy, Err(IoError::RegionBusy { at: 100, len: 8 })),
        "{want:?} {busy:?}"
    );
    // Not terminated.
    let bare = q.push(op(&q, 0, 4), 5);
    assert!(
        matches!(bare, Err(IoError::PathTooLong { .. })),
        "{want:?} {bare:?}"
    );
    assert!(q.cancel(key));
    wait(&mut q, 3, &mut Vec::new());
    assert_eq!(q.in_flight(), 0);
    q.unregister(r).expect("free once the ops ended");
}

both!(
    an_open_beneath_a_dir_stays_inside,
    an_open2_path_is_held_until_its_completion,
);
