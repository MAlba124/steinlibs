//! The identity and mount id every fd stat carries, on both backends.

use super::*;
use crate::file::{self, FileMeta};
use crate::{Mode, OFlags};
use rustix::fs::{AtFlags, StatxFlags};
use std::os::unix::fs::MetadataExt as _;

const ASKED: StatxFlags = StatxFlags::BASIC_STATS.union(StatxFlags::MNT_ID);

/// The meta a direct statx of an owned `fd` with the mount id gives.
fn direct(io: &Io, fd: RawFd) -> FileMeta {
    let f = io.fd(fd).expect("owned");
    file::from_statx(&rustix::fs::statx(f, "", AtFlags::EMPTY_PATH, ASKED).expect("statx"))
}

fn landed(io: &mut Io, id: OpId) -> FileMeta {
    let c = pump(io).expect("stat completion");
    assert_eq!((c.op, c.kind), (id, Kind::Stat));
    assert_eq!(c.result, Ok(file::META_LEN));
    let b = c.buf.expect("a stat delivers a buffer");
    let m = file::meta(io.filled(&b)).expect("meta");
    io.release(b);
    m
}

fn check_a_file_stat_carries_its_identity_and_mount(back: Backend) {
    let p = scratch_file("statmnt", back, b"mount me");
    let md = std::fs::metadata(&p).expect("metadata");
    driver!(io, back, ops = 8, bufs = 4, size = 512);
    io.open(&path_bytes(&p)).expect("open");
    let fd = pump(&mut io).expect("open").result.expect("opens") as RawFd;

    let id = io.stat_fd(fd).expect("stat");
    let m = landed(&mut io, id);
    assert!(m.is_regular(), "{back:?}");
    assert_eq!((m.ino, m.dev, m.size), (md.ino(), md.dev(), 8), "{back:?}");
    assert!(m.mnt_id.is_some(), "every kernel since 5.8 gives it");
    assert_eq!(m, direct(&io, fd), "{back:?}");

    assert!(io.close(fd));
    assert_eq!(io.bufs().available(), 4, "a buffer leaked");
    let _ = std::fs::remove_file(&p);
}

/// The walk's root stat, a dir opened `O_PATH`.
fn check_a_path_dir_stat_gives_its_type_and_mount(back: Backend) {
    let d = std::env::temp_dir();
    let md = std::fs::metadata(&d).expect("metadata");
    driver!(io, back, ops = 8, bufs = 4, size = 512);
    let flags = OFlags::PATH | OFlags::DIRECTORY;
    io.open_with(CWD, &path_bytes(&d), flags, Mode::empty())
        .expect("open");
    let fd = pump(&mut io).expect("open").result.expect("opens") as RawFd;

    let id = io.stat_fd(fd).expect("stat");
    let m = landed(&mut io, id);
    assert!(m.is_dir(), "{back:?}");
    assert_eq!((m.ino, m.dev), (md.ino(), md.dev()), "{back:?}");
    assert!(m.mnt_id.is_some(), "{back:?}");
    assert_eq!(m.mnt_id, direct(&io, fd).mnt_id, "{back:?}");

    assert!(io.close(fd));
    assert_eq!(io.bufs().available(), 4, "a buffer leaked");
}

/// The sync stat after an open fills the same fields.
fn check_a_stat_after_its_open_carries_the_identity(back: Backend) {
    let p = scratch_file("statopened", back, b"opened");
    let md = std::fs::metadata(&p).expect("metadata");
    driver!(io, back, ops = 8, bufs = 4, size = 512);
    let open = io.open(&path_bytes(&p)).expect("open");
    let stat = io.stat_opened(open).expect("stat");
    let fd = pump(&mut io).expect("open").result.expect("opens") as RawFd;

    let m = landed(&mut io, stat);
    assert_eq!((m.ino, m.dev, m.size), (md.ino(), md.dev(), 6), "{back:?}");
    assert!(m.mnt_id.is_some(), "{back:?}");
    assert_eq!(m, direct(&io, fd), "{back:?}");

    assert!(io.close(fd));
    assert_eq!(io.bufs().available(), 4, "a buffer leaked");
    let _ = std::fs::remove_file(&p);
}

/// A buffer the meta would not fit is refused up front, not answered short.
fn check_a_stat_after_an_open_needs_room_for_the_meta(back: Backend) {
    driver!(io, back, ops = 8, bufs = 4, size = file::META_LEN - 1);
    let open = io.open(b"/").expect("open");
    let max = file::META_LEN as usize - 1;
    let refused = io.stat_opened(open);
    assert!(
        matches!(refused, Err(IoError::TooLong { len, max: m }) if len == max + 1 && m == max),
        "{back:?} {refused:?}"
    );
    let c = pump(&mut io).expect("open");
    assert_eq!(c.op, open, "{back:?}");
    assert!(io.close(c.result.expect("opens") as RawFd));
    assert_eq!(io.bufs().available(), 4, "a buffer leaked");
}

#[test]
fn a_file_stat_carries_its_identity_and_mount() {
    for b in backends() {
        check_a_file_stat_carries_its_identity_and_mount(b);
    }
}

#[test]
fn a_path_dir_stat_gives_its_type_and_mount() {
    for b in backends() {
        check_a_path_dir_stat_gives_its_type_and_mount(b);
    }
}

#[test]
fn a_stat_after_its_open_carries_the_identity() {
    for b in backends() {
        check_a_stat_after_its_open_carries_the_identity(b);
    }
}

#[test]
fn a_stat_after_an_open_needs_room_for_the_meta() {
    for b in backends() {
        check_a_stat_after_an_open_needs_room_for_the_meta(b);
    }
}
