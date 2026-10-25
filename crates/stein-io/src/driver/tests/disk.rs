//! Durable-store file ops on both backends.

use super::*;
use crate::dir;
use crate::{Mode, OFlags};

fn scratch_dir(name: &str, back: Backend) -> (std::path::PathBuf, Vec<u8>) {
    let d = std::env::temp_dir().join(format!("stein-io-{}-{name}-{back:?}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("mkdir");
    let bytes = path_bytes(&d);
    (d, bytes)
}

fn mode() -> Mode {
    Mode::from_raw_mode(0o644)
}

fn open_dir(io: &mut Io, path: &[u8]) -> RawFd {
    io.open_with(CWD, path, OFlags::DIRECTORY, Mode::empty())
        .expect("open dir");
    let c = pump(io).expect("open dir completion");
    assert_eq!(c.kind, Kind::Open);
    c.result.expect("a directory opens") as RawFd
}

fn finish(io: &mut Io, id: OpId, kind: Kind) -> Result<u32, Errno> {
    let c = pump(io).expect("no completion");
    assert_eq!(c.op, id);
    assert_eq!(c.kind, kind);
    assert!(c.is_final());
    assert!(c.buf.is_none(), "{kind} handed back a buffer");
    c.result
}

fn check_a_file_is_created_only_once(back: Backend) {
    let (d, path) = scratch_dir("excl", back);
    driver!(io, back, ops = 8, bufs = 4, size = 512);
    let dir = open_dir(&mut io, &path);
    let new = OFlags::RDWR | OFlags::CREATE | OFlags::EXCL;

    io.open_with(dir, b"seg", new, mode()).expect("create");
    let c = pump(&mut io).expect("create completion");
    let fd = c.result.expect("a new file is created") as RawFd;
    assert_eq!(c.fd, fd, "the driver owns what it opened");
    assert!(d.join("seg").exists());

    io.open_with(dir, b"seg", new, mode())
        .expect("create again");
    let c = pump(&mut io).expect("second completion");
    assert_eq!(c.result, Err(Errno::EXIST), "an existing file was reused");

    // Without EXCL it opens the existing file.
    io.open_with(dir, b"seg", OFlags::RDWR, Mode::empty())
        .expect("open");
    let fd2 = pump(&mut io).expect("open").result.expect("opens") as RawFd;
    io.open_with(dir, b"absent", OFlags::RDWR, Mode::empty())
        .expect("open");
    assert_eq!(pump(&mut io).expect("open").result, Err(Errno::NOENT));
    // Unowned dir fd.
    io.open_with(9999, b"seg", OFlags::RDWR, Mode::empty())
        .expect("open");
    assert_eq!(pump(&mut io).expect("open").result, Err(Errno::BADF));

    for f in [fd, fd2, dir] {
        assert!(io.close(f));
    }
    assert_eq!(io.bufs().available(), 4, "a path buffer leaked");
    assert_eq!(io.in_flight(), 0);
    let _ = std::fs::remove_dir_all(&d);
}

fn check_a_region_is_written_and_read_at_offsets(back: Backend) {
    let (d, path) = scratch_dir("mem", back);
    let mut slab = vec![0u8; 1 << 16];
    driver!(io, back, ops = 8, bufs = 4, size = 512);
    let r = io.add_region(slab.into_boxed_slice()).expect("region");
    let dir = open_dir(&mut io, &path);
    io.open_with(dir, b"data", OFlags::RDWR | OFlags::CREATE, mode())
        .expect("open");
    let fd = pump(&mut io).expect("open").result.expect("opens") as RawFd;

    // Far larger than a buffer.
    const LEN: usize = 40_000;
    for (i, b) in io.region_mut(r)[100..100 + LEN].iter_mut().enumerate() {
        *b = (i * 7) as u8;
    }
    let id = io.write_mem(fd, 3, r, 100, LEN as u32).expect("write");
    assert_eq!(finish(&mut io, id, Kind::WriteMem), Ok(LEN as u32));
    let on_disk = std::fs::read(d.join("data")).expect("read back");
    assert_eq!(on_disk.len(), LEN + 3);
    assert!(
        on_disk[3..]
            .iter()
            .enumerate()
            .all(|(i, b)| *b == (i * 7) as u8)
    );

    // Into an untouched part of the region.
    let id = io.read_mem(fd, 3 + 1000, r, 50_000, 500).expect("read");
    assert_eq!(finish(&mut io, id, Kind::ReadMem), Ok(500));
    let region = io.region(r);
    assert_eq!(&region[50_000..50_500], &region[1100..1600]);
    assert_eq!(region[50_500], 0, "read past its length");
    assert_eq!(region[49_999], 0, "read before its start");

    assert!(io.close(fd));
    assert!(io.close(dir));
    assert_eq!(io.in_flight(), 0);
    let _ = std::fs::remove_dir_all(&d);
}

/// EOF, concurrent writes, out-of-range spans, bad fd.
fn check_a_region_keeps_to_its_bounds(back: Backend) {
    const LEN: usize = 5000;
    let (d, path) = scratch_dir("edges", back);
    let mut slab = vec![3u8; 1 << 16];
    let mut other = vec![0u8; 64];
    driver!(io, back, ops = 8, bufs = 4, size = 512);
    let r = io.add_region(slab.into_boxed_slice()).expect("region");
    let small = io
        .add_region(other.into_boxed_slice())
        .expect("second region");
    assert_ne!(r, small);
    assert_eq!(io.region(small).len(), 64);
    let dir = open_dir(&mut io, &path);
    io.open_with(dir, b"data", OFlags::RDWR | OFlags::CREATE, mode())
        .expect("open");
    let fd = pump(&mut io).expect("open").result.expect("opens") as RawFd;
    let id = io.write_mem(fd, 3, r, 0, LEN as u32).expect("write");
    assert_eq!(finish(&mut io, id, Kind::WriteMem), Ok(LEN as u32));

    // Short read at the tail, zero past the end.
    let id = io.read_mem(fd, LEN as u64 - 7, r, 0, 64).expect("read");
    assert_eq!(finish(&mut io, id, Kind::ReadMem), Ok(10));
    let id = io.read_mem(fd, LEN as u64 + 3, r, 0, 64).expect("read");
    assert_eq!(finish(&mut io, id, Kind::ReadMem), Ok(0));

    io.region_mut(small)[..8].copy_from_slice(b"ABCDEFGH");
    let a = io.write_mem(fd, 4, small, 4, 4).expect("write");
    let b = io.write_mem(fd, 0, small, 0, 4).expect("write");
    let mut seen = Vec::new();
    for _ in 0..2 {
        let c = pump(&mut io).expect("write completion");
        assert_eq!(c.result, Ok(4));
        seen.push(c.op);
    }
    assert!(seen.contains(&a) && seen.contains(&b));
    assert_eq!(
        &std::fs::read(d.join("data")).expect("read")[..8],
        b"ABCDEFGH"
    );

    for (at, len) in [(1usize << 16, 1u32), (65_000, 1000), (usize::MAX, 2)] {
        assert!(matches!(
            io.write_mem(fd, 0, r, at, len),
            Err(IoError::OutOfRegion { .. })
        ));
        assert!(matches!(
            io.read_mem(fd, 0, r, at, len),
            Err(IoError::OutOfRegion { .. })
        ));
    }
    assert!(matches!(
        io.read_mem(fd, 0, Region(7), 0, 1),
        Err(IoError::OutOfRegion { size: 0, .. })
    ));
    let id = io.write_mem(9999, 0, r, 0, 8).expect("submit");
    assert_eq!(finish(&mut io, id, Kind::WriteMem), Err(Errno::BADF));

    assert!(io.close(fd));
    assert!(io.close(dir));
    assert_eq!(io.in_flight(), 0);
    assert_eq!(io.bufs().available(), 4);
    let _ = std::fs::remove_dir_all(&d);
}

fn check_regions_are_few(back: Backend) {
    driver!(io, back, ops = 4, bufs = 4, size = 64);
    let mut taken = 0;
    for _ in 0..9 {
        match io.add_region(vec![0u8; 16].into_boxed_slice()) {
            Ok(r) => {
                assert_eq!(r, Region(taken));
                taken += 1;
            }
            Err(e) => assert!(matches!(e, IoError::RegionsFull), "{e}"),
        }
    }
    assert_eq!(taken, 8);
}

fn check_a_file_is_synced_cut_renamed_and_removed(back: Backend) {
    let (d, path) = scratch_dir("durable", back);
    let mut slab = vec![7u8; 4096];
    driver!(io, back, ops = 8, bufs = 4, size = 512);
    let r = io.add_region(slab.into_boxed_slice()).expect("region");
    let dir = open_dir(&mut io, &path);
    let new = OFlags::RDWR | OFlags::CREATE | OFlags::EXCL;
    io.open_with(dir, b"a.tmp", new, mode()).expect("create");
    let fd = pump(&mut io).expect("open").result.expect("created") as RawFd;
    let id = io.write_mem(fd, 0, r, 0, 4096).expect("write");
    assert_eq!(finish(&mut io, id, Kind::WriteMem), Ok(4096));

    for data in [true, false] {
        let id = io.sync(fd, data).expect("sync");
        assert_eq!(finish(&mut io, id, Kind::Sync), Ok(0));
    }
    // Dir sync makes the new name durable.
    let id = io.sync(dir, false).expect("sync dir");
    assert_eq!(finish(&mut io, id, Kind::Sync), Ok(0));

    let id = io.truncate(fd, 1000).expect("truncate");
    assert_eq!(finish(&mut io, id, Kind::Truncate), Ok(0));
    assert_eq!(
        std::fs::metadata(d.join("a.tmp")).expect("meta").len(),
        1000
    );
    let id = io.truncate(fd, 5000).expect("truncate");
    assert_eq!(finish(&mut io, id, Kind::Truncate), Ok(0));
    let grown = std::fs::read(d.join("a.tmp")).expect("read");
    assert_eq!(grown.len(), 5000);
    assert!(grown[..1000].iter().all(|b| *b == 7));
    assert!(grown[1000..].iter().all(|b| *b == 0));

    io.stat_fd(fd).expect("stat");
    let c = pump(&mut io).expect("stat completion");
    assert_eq!(c.kind, Kind::Stat);
    let b = c.buf.expect("a stat delivers a buffer");
    let m = crate::file::meta(io.filled(&b)).expect("meta");
    assert!(m.is_regular());
    assert_eq!(m.size, 5000);
    io.release(b);

    assert!(io.close(fd));
    assert!(io.close(dir));
    assert_eq!(io.bufs().available(), 4, "a buffer leaked");
    let _ = std::fs::remove_dir_all(&d);
}

fn check_names_are_changed_and_removed(back: Backend) {
    let (d, path) = scratch_dir("names", back);
    driver!(io, back, ops = 8, bufs = 4, size = 512);
    let dir = open_dir(&mut io, &path);
    std::fs::write(d.join("a.tmp"), vec![7u8; 5000]).expect("write");
    io.open_with(dir, b"a.tmp", OFlags::RDWR, Mode::empty())
        .expect("open");
    let fd = pump(&mut io).expect("open").result.expect("opens") as RawFd;

    // Rename replaces the target.
    std::fs::write(d.join("a"), b"old").expect("write");
    let id = io.rename(dir, b"a.tmp", dir, b"a").expect("rename");
    assert_eq!(finish(&mut io, id, Kind::Rename), Ok(0));
    assert!(!d.join("a.tmp").exists());
    assert_eq!(std::fs::metadata(d.join("a")).expect("meta").len(), 5000);
    let id = io.rename(dir, b"a.tmp", dir, b"b").expect("rename");
    assert_eq!(finish(&mut io, id, Kind::Rename), Err(Errno::NOENT));

    // Full paths against CWD.
    let mut full = path.clone();
    full.extend_from_slice(b"/a");
    let mut to = path.clone();
    to.extend_from_slice(b"/c");
    let id = io.rename(CWD, &full, CWD, &to).expect("rename");
    assert_eq!(finish(&mut io, id, Kind::Rename), Ok(0));
    assert!(d.join("c").exists());

    let id = io.unlink(dir, b"c", false).expect("unlink");
    assert_eq!(finish(&mut io, id, Kind::Unlink), Ok(0));
    assert!(!d.join("c").exists());
    let id = io.unlink(dir, b"c", false).expect("unlink");
    assert_eq!(finish(&mut io, id, Kind::Unlink), Err(Errno::NOENT));

    std::fs::create_dir(d.join("sub")).expect("mkdir");
    let id = io.unlink(dir, b"sub", false).expect("unlink");
    assert_eq!(finish(&mut io, id, Kind::Unlink), Err(Errno::ISDIR));
    let id = io.unlink(dir, b"sub", true).expect("rmdir");
    assert_eq!(finish(&mut io, id, Kind::Unlink), Ok(0));
    assert!(!d.join("sub").exists());

    // Both paths must fit one buffer.
    let long = vec![b'x'; 300];
    assert!(matches!(
        io.rename(dir, &long, dir, &long),
        Err(IoError::PathTooLong { len: 600, max: 510 })
    ));
    // Bad fd fails in the completion, not at submit.
    for id in [io.sync(9999, true), io.truncate(9999, 0)] {
        let c = pump(&mut io).expect("completion");
        assert_eq!(c.op, id.expect("submit"));
        assert_eq!(c.result, Err(Errno::BADF));
    }

    assert!(io.close(fd));
    assert!(io.close(dir));
    assert_eq!(io.bufs().available(), 4, "a path buffer leaked");
    assert_eq!(io.in_flight(), 0);
    let _ = std::fs::remove_dir_all(&d);
}

fn check_a_directory_lists_into_a_region(back: Backend) {
    const FILES: usize = 300;
    let (d, path) = scratch_dir("list", back);
    let mut want = Vec::new();
    for i in 0..FILES {
        // Name lengths up to the kernel max.
        let name = format!("{i:03}{}", "n".repeat(i % 250));
        std::fs::write(d.join(&name), b"").expect("write");
        want.push(name.into_bytes());
    }
    std::fs::create_dir(d.join("sub")).expect("mkdir");
    want.push(b"sub".to_vec());
    want.sort();

    let mut names = vec![0u8; 2048];
    driver!(io, back, ops = 8, bufs = 4, size = 512);
    let r = io.add_region(names.into_boxed_slice()).expect("region");
    let dir = open_dir(&mut io, &path);

    // Too small for one name.
    let id = io.list(dir, r, 0, 2).expect("list");
    assert_eq!(finish(&mut io, id, Kind::List), Err(Errno::INVAL));

    let mut got = Vec::new();
    let mut calls = 0;
    loop {
        // Odd offset and length so entries never align.
        let id = io.list(dir, r, 5, 777).expect("list");
        let n = finish(&mut io, id, Kind::List).expect("a listing") as usize;
        if n == 0 {
            break;
        }
        calls += 1;
        assert!(n <= 777);
        let mut used = 0;
        for e in dir::entries(&io.region(r)[5..5 + n]) {
            used += dir::PACK_HEAD + e.name.len();
            let is_dir = e.name == b"sub";
            assert_eq!(e.kind == rustix::fs::FileType::Directory, is_dir);
            got.push(e.name.to_vec());
        }
        assert_eq!(used, n, "a packed entry was cut");
    }
    assert!(calls > 10, "the listing never had to resume, {calls} calls");
    got.sort();
    assert_eq!(got, want);
    let id = io.list(dir, r, 0, 2048).expect("list");
    assert_eq!(finish(&mut io, id, Kind::List), Ok(0));

    assert!(matches!(
        io.list(dir, r, 2000, 100),
        Err(IoError::OutOfRegion { .. })
    ));
    let id = io.list(9999, r, 0, 2048).expect("submit");
    assert_eq!(finish(&mut io, id, Kind::List), Err(Errno::BADF));
    assert!(io.close(dir));
    assert_eq!(io.in_flight(), 0);
    let _ = std::fs::remove_dir_all(&d);
}

/// Including ops that finish inside their own submission.
fn check_slots_are_held_until_taken(back: Backend) {
    let (d, path) = scratch_dir("slots", back);
    let mut slab = vec![0u8; 64];
    driver!(io, back, ops = 4, bufs = 4, size = 512);
    let r = io.add_region(slab.into_boxed_slice()).expect("region");
    let dir = open_dir(&mut io, &path);
    let a = io.sync(dir, false).expect("sync");
    let b = io.list(dir, r, 0, 64).expect("list");
    let c = io.unlink(dir, b"nothing", false).expect("unlink");
    assert_ne!(a.slot(), b.slot());
    assert_ne!(b.slot(), c.slot());
    let mut seen = Vec::new();
    for _ in 0..3 {
        seen.push(pump(&mut io).expect("completion").op);
    }
    seen.sort_by_key(|id| id.slot());
    let mut want = [a, b, c];
    want.sort_by_key(|id| id.slot());
    assert_eq!(seen, want);
    assert_eq!(io.in_flight(), 0);

    // Cancelling a finished op drops only its slot.
    let id = io.truncate(9999, 0).expect("truncate");
    assert!(io.cancel(id));
    assert_eq!(io.in_flight(), 0);
    assert!(io.close(dir));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_operation_record_is_still_four_words() {
    assert_eq!(size_of::<Op>(), 32);
    assert_eq!(size_of::<Option<Op>>(), 32);
}

macro_rules! both {
    ($($name:ident => $check:ident,)*) => {$(
        #[test]
        fn $name() {
            for b in backends() {
                $check(b);
            }
        }
    )*};
}

both! {
    a_file_is_created_only_once => check_a_file_is_created_only_once,
    a_region_is_written_and_read_at_offsets => check_a_region_is_written_and_read_at_offsets,
    regions_are_few => check_regions_are_few,
    a_file_is_synced_cut_renamed_and_removed => check_a_file_is_synced_cut_renamed_and_removed,
    names_are_changed_and_removed => check_names_are_changed_and_removed,
    a_region_keeps_to_its_bounds => check_a_region_keeps_to_its_bounds,
    a_directory_lists_into_a_region => check_a_directory_lists_into_a_region,
    slots_are_held_until_taken => check_slots_are_held_until_taken,
}
