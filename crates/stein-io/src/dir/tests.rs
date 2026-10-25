#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use rustix::fs::{Mode, OFlags};
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("stein-io-dir-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("mkdir");
    d
}

fn names_of(dir: &PathBuf) -> Vec<(Vec<u8>, FileType)> {
    let fd =
        rustix::fs::open(dir, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).expect("open dir");
    let mut buf = [MaybeUninit::uninit(); 4096];
    let mut got = Vec::new();
    list(fd.as_fd(), &mut buf, |e| {
        got.push((e.name.to_vec(), e.kind));
    })
    .expect("list");
    got.sort_by(|a, b| a.0.cmp(&b.0));
    got
}

use std::os::fd::AsFd;

#[test]
fn entries_come_back_with_their_kinds_and_no_dots() {
    let d = scratch("kinds");
    std::fs::write(d.join("a"), b"").expect("write");
    std::fs::write(d.join("b"), b"").expect("write");
    std::fs::create_dir(d.join("sub")).expect("mkdir");
    let got = names_of(&d);
    assert_eq!(got.len(), 3, "dot entries leaked: {got:?}");
    assert_eq!(got[0].0, b"a");
    assert_eq!(got[1].0, b"b");
    assert_eq!(got[2].0, b"sub");
    assert_eq!(got[2].1, FileType::Directory);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_empty_directory_lists_nothing() {
    let d = scratch("empty");
    assert!(names_of(&d).is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

/// More entries than one getdents batch holds, so the refill path runs.
#[test]
fn a_large_directory_is_listed_completely() {
    let d = scratch("large");
    for i in 0..500 {
        std::fs::write(d.join(format!("file-{i:03}")), b"").expect("write");
    }
    let got = names_of(&d);
    assert_eq!(got.len(), 500);
    let _ = std::fs::remove_dir_all(&d);
}
