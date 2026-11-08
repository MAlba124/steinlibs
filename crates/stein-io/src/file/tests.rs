#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use rustix::fs::{AtFlags, CWD, StatxFlags};

#[test]
fn serialize_and_meta_round_trip() {
    let path = std::env::temp_dir().join(format!("stein-io-meta-{}", std::process::id()));
    std::fs::write(&path, b"twelve bytes").expect("write");
    let sx =
        rustix::fs::statx(CWD, &path, AtFlags::empty(), StatxFlags::BASIC_STATS).expect("statx");
    let mut out = [0xffu8; META_LEN as usize];
    serialize(&sx, &mut out);
    let m = meta(&out).expect("meta");
    assert_eq!(m.mode, sx.stx_mode);
    assert_eq!(m.size, 12);
    assert_eq!(m.mtime_sec, sx.stx_mtime.tv_sec);
    assert!(m.is_regular());
    assert!(!m.is_dir());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_short_buffer_is_no_meta() {
    assert!(meta(&[0u8; 64]).is_none());
    assert!(meta(&[]).is_none());
}

#[test]
fn the_execute_bits_are_the_question() {
    let m = FileMeta {
        mode: 0o100755,
        ..FileMeta::default()
    };
    assert!(m.is_regular());
    assert!(m.is_executable());
    let m = FileMeta {
        mode: 0o100644,
        ..FileMeta::default()
    };
    assert!(!m.is_executable());
}

#[test]
fn a_directory_reads_as_one() {
    let m = FileMeta {
        mode: 0o040755,
        ..FileMeta::default()
    };
    assert!(m.is_dir());
    assert!(!m.is_regular());
}

#[test]
fn serialize_zeroes_what_it_does_not_set() {
    let path = std::env::temp_dir().join(format!("stein-io-zero-{}", std::process::id()));
    std::fs::write(&path, b"x").expect("write");
    let sx =
        rustix::fs::statx(CWD, &path, AtFlags::empty(), StatxFlags::BASIC_STATS).expect("statx");
    let mut out = [0xaau8; META_LEN as usize];
    serialize(&sx, &mut out);
    // Offset 4 is stx_blksize, not carried.
    assert_eq!(&out[4..8], &[0, 0, 0, 0]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn serialize_carries_the_identity_and_the_mount() {
    use std::os::unix::fs::MetadataExt as _;
    let path = std::env::temp_dir().join(format!("stein-io-ident-{}", std::process::id()));
    std::fs::write(&path, b"who").expect("write");
    let md = std::fs::metadata(&path).expect("metadata");
    let asked = StatxFlags::BASIC_STATS | StatxFlags::MNT_ID;
    let sx = rustix::fs::statx(CWD, &path, AtFlags::empty(), asked).expect("statx");
    let mut out = [0xffu8; META_LEN as usize];
    serialize(&sx, &mut out);
    let m = meta(&out).expect("meta");
    assert_eq!((m.ino, m.dev), (md.ino(), md.dev()));
    assert_eq!(m.mnt_id, Some(sx.stx_mnt_id));
    assert_eq!(m, from_statx(&sx));

    // A kernel that gave no mount id, before 5.8.
    let mut old = sx;
    old.stx_mask &= !StatxFlags::MNT_ID.bits();
    serialize(&old, &mut out);
    assert_eq!(meta(&out).expect("meta").mnt_id, None);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn encode_and_meta_agree_on_every_field() {
    let m = FileMeta {
        mode: 0o060640,
        size: u64::MAX - 1,
        mtime_sec: -5,
        mtime_nsec: 999_999_999,
        ino: 0x0123_4567_89ab_cdef,
        dev: rustix::fs::makedev(0x1234_5678, 0x9abc_def0),
        mnt_id: Some(u64::MAX),
    };
    let mut out = [0xaau8; META_LEN as usize];
    encode(&m, &mut out);
    assert_eq!(meta(&out), Some(m));
    let none = FileMeta { mnt_id: None, ..m };
    encode(&none, &mut out);
    assert_eq!(meta(&out), Some(none));
    assert!(meta(&out[..META_LEN as usize - 1]).is_none());
}
