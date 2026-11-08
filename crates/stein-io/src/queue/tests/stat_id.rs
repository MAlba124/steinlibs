//! [`Op::Statx`] of a descriptor on both backends, with the mount id it asks.

use super::*;
use rustix::fs::{AtFlags, FileType, Statx, StatxFlags};
use std::os::unix::fs::MetadataExt as _;

fn stat_fd(q: &mut Queue, fd: RawFd, user: u64) -> Statx {
    let mut out = [0u8; STATX_LEN];
    let op = Op::Statx {
        dir: fd,
        path: Mem::EMPTY,
        out: mem(&mut out),
        follow: true,
    };
    assert_eq!(one(q, op, user).result(), Ok(0));
    statx_of(&out).expect("statx")
}

fn a_descriptor_stat_carries_its_identity_and_mount(want: Want) {
    let mut q = queue(want);
    let p = tmp("statmnt", want);
    std::fs::write(&p, b"mount").expect("write");
    let f = std::fs::File::open(&p).expect("open");
    let md = f.metadata().expect("metadata");

    let sx = stat_fd(&mut q, f.as_raw_fd(), 1);
    assert_eq!((sx.stx_ino, sx.stx_size), (md.ino(), 5), "{want:?}");
    let dev = rustix::fs::makedev(sx.stx_dev_major, sx.stx_dev_minor);
    assert_eq!(dev, md.dev(), "{want:?}");
    let asked = StatxFlags::BASIC_STATS | StatxFlags::MNT_ID;
    let direct = rustix::fs::statx(&f, "", AtFlags::EMPTY_PATH, asked).expect("statx");
    assert_ne!(sx.stx_mask & StatxFlags::MNT_ID.bits(), 0, "{want:?}");
    assert_eq!(sx.stx_mnt_id, direct.stx_mnt_id, "{want:?}");

    let dir = std::fs::File::open(std::env::temp_dir()).expect("dir");
    let sx = stat_fd(&mut q, dir.as_raw_fd(), 2);
    let kind = FileType::from_raw_mode(sx.stx_mode.into());
    assert_eq!(kind, FileType::Directory, "{want:?}");
    assert_eq!(q.in_flight(), 0);
    let _ = std::fs::remove_file(&p);
}

both!(a_descriptor_stat_carries_its_identity_and_mount);
