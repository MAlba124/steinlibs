//! [`Io::open_beneath`] on both backends.

use super::*;
use crate::{Mode, OFlags, ResolveFlags};

/// `jail/` with `in`, a link to it, and links out to `outside`, relative and absolute.
fn tree(name: &str, back: Backend) -> (std::path::PathBuf, std::path::PathBuf) {
    let root =
        std::env::temp_dir().join(format!("stein-io-{}-{name}-{back:?}", std::process::id()));
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

fn open_jail(io: &mut Io, jail: &std::path::Path) -> RawFd {
    io.open_with(CWD, &path_bytes(jail), OFlags::DIRECTORY, Mode::empty())
        .expect("open dir");
    pump(io).expect("dir").result.expect("the jail opens") as RawFd
}

fn beneath(io: &mut Io, dir: RawFd, path: &[u8]) -> OpId {
    let flags = OFlags::RDONLY;
    io.open_beneath(dir, path, flags, Mode::empty(), ResolveFlags::BENEATH)
        .expect("submit")
}

fn opened(io: &mut Io, id: OpId) -> Completion {
    let c = pump(io).expect("open2 completion");
    assert_eq!((c.op, c.kind), (id, Kind::Open2));
    assert!(c.is_final());
    assert!(c.buf.is_none(), "an open handed back its path buffer");
    c
}

fn check_an_open_beneath_a_dir_stays_inside(back: Backend) {
    let (root, jail) = tree("beneath", back);
    driver!(io, back, ops = 8, bufs = 8, size = 512);
    let before = io.available();
    let dir = open_jail(&mut io, &jail);

    for inside in [&b"in"[..], b"sub/../in", b"link_in"] {
        let id = beneath(&mut io, dir, inside);
        let c = opened(&mut io, id);
        let fd = c
            .result
            .unwrap_or_else(|e| panic!("{back:?} {inside:?} {e:?}")) as RawFd;
        assert_eq!(c.fd, fd, "the driver owns what it opened");
        io.read_at(fd, 0).expect("read");
        let c = pump(&mut io).expect("read completion");
        let b = c.buf.expect("a read delivers a buffer");
        assert_eq!(io.filled(&b), b"inside");
        io.release(b);
        assert!(io.close(fd));
    }

    let abs = path_bytes(&root.join("outside"));
    for out in [&b"../outside"[..], &abs, b"link_up", b"link_abs"] {
        let id = beneath(&mut io, dir, out);
        let c = opened(&mut io, id);
        assert_eq!(c.result, Err(Errno::XDEV), "{back:?} {out:?} got out");
    }
    let id = beneath(&mut io, dir, b"absent");
    assert_eq!(opened(&mut io, id).result, Err(Errno::NOENT));

    assert!(io.close(dir));
    assert_eq!(io.in_flight(), 0);
    assert_eq!(io.available(), before, "{back:?} a path buffer leaked");
    let _ = std::fs::remove_dir_all(&root);
}

/// The path buffer goes back at the completion, or after a cancel once the kernel let go.
fn check_an_open_beneath_holds_its_path_until_it_lands(back: Backend) {
    let (root, jail) = tree("held", back);
    driver!(io, back, ops = 8, bufs = 8, size = 512);
    let dir = open_jail(&mut io, &jail);
    let before = io.available();

    let id = beneath(&mut io, dir, b"sub");
    if back == Backend::Uring {
        assert!(io.is_live(id));
        assert_eq!(
            io.available(),
            before - 1,
            "the path went before the open landed"
        );
    }
    let fd = opened(&mut io, id).result.expect("sub is a dir") as RawFd;
    assert_eq!(io.available(), before);
    assert!(io.close(fd));

    let id = beneath(&mut io, dir, b"sub");
    assert!(io.cancel(id));
    let start = Instant::now();
    while io.available() < before && start.elapsed() < LIMIT {
        io.tick(Some(Duration::from_millis(10))).expect("tick");
        while io.next_completion().is_some() {}
    }
    assert_eq!(
        io.available(),
        before,
        "{back:?} a cancelled open kept its path"
    );
    assert!(io.close(dir));
    assert_eq!(io.in_flight(), 0);
    let _ = std::fs::remove_dir_all(&root);
}

fn check_a_stat_rides_an_open_beneath(back: Backend) {
    let (root, jail) = tree("ride", back);
    driver!(io, back, ops = 8, bufs = 8, size = 512);
    let dir = open_jail(&mut io, &jail);
    let id = beneath(&mut io, dir, b"in");
    let stat = io.stat_opened(id).expect("stat");
    let fd = opened(&mut io, id).result.expect("opens") as RawFd;
    let c = io.next_completion().expect("the stat came with the open");
    assert_eq!((c.op, c.kind), (stat, Kind::Stat));
    let b = c.buf.expect("a stat delivers a buffer");
    assert_eq!(crate::file::meta(io.filled(&b)).expect("meta").size, 6);
    io.release(b);
    assert!(io.close(fd));
    assert!(io.close(dir));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_open_beneath_a_dir_stays_inside() {
    for b in backends() {
        check_an_open_beneath_a_dir_stays_inside(b);
    }
}

#[test]
fn an_open_beneath_holds_its_path_until_it_lands() {
    for b in backends() {
        check_an_open_beneath_holds_its_path_until_it_lands(b);
    }
}

#[test]
fn a_stat_rides_an_open_beneath() {
    for b in backends() {
        check_a_stat_rides_an_open_beneath(b);
    }
}
