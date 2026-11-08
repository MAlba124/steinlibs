//! [`crate::op::Kind::Stat`] buffer layout, the head of `struct statx` on both backends.

use rustix::fs::{FileType, Statx, StatxFlags};

/// Bytes of the statx head [`meta`] reads. Ends after `stx_mnt_id` at 144.
pub const META_LEN: u32 = 152;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileMeta {
    /// Type and permission bits as in `st_mode`.
    pub mode: u16,
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    pub ino: u64,
    /// `makedev(stx_dev_major, stx_dev_minor)`, as std's `MetadataExt::dev`.
    pub dev: u64,
    /// Set when `stx_mask` has `STATX_MNT_ID`, which kernels before 5.8 never give.
    pub mnt_id: Option<u64>,
}

impl FileMeta {
    pub fn file_type(&self) -> FileType {
        FileType::from_raw_mode(self.mode as u32)
    }

    pub fn is_regular(&self) -> bool {
        self.file_type() == FileType::RegularFile
    }

    pub fn is_dir(&self) -> bool {
        self.file_type() == FileType::Directory
    }

    /// Any execute bit set.
    pub fn is_executable(&self) -> bool {
        self.mode & 0o111 != 0
    }
}

/// `struct statx` field offsets. Kernel ABI.
const AT_MASK: usize = 0;
const AT_MODE: usize = 28;
const AT_INO: usize = 32;
const AT_SIZE: usize = 40;
const AT_MTIME: usize = 112;
const AT_DEV_MAJOR: usize = 136;
const AT_DEV_MINOR: usize = 140;
const AT_MNT_ID: usize = 144;
const MNT_ID: u32 = StatxFlags::MNT_ID.bits();

/// Parse a stat buffer. `None` if shorter than [`META_LEN`].
pub fn meta(bytes: &[u8]) -> Option<FileMeta> {
    if bytes.len() < META_LEN as usize {
        return None;
    }
    let mask = u32::from_ne_bytes(take(bytes, AT_MASK)?);
    let mnt_id = u64::from_ne_bytes(take(bytes, AT_MNT_ID)?);
    let major = u32::from_ne_bytes(take(bytes, AT_DEV_MAJOR)?);
    let minor = u32::from_ne_bytes(take(bytes, AT_DEV_MINOR)?);
    Some(FileMeta {
        mode: u16::from_ne_bytes(take(bytes, AT_MODE)?),
        size: u64::from_ne_bytes(take(bytes, AT_SIZE)?),
        mtime_sec: i64::from_ne_bytes(take(bytes, AT_MTIME)?),
        mtime_nsec: u32::from_ne_bytes(take(bytes, AT_MTIME + 8)?),
        ino: u64::from_ne_bytes(take(bytes, AT_INO)?),
        dev: rustix::fs::makedev(major, minor),
        mnt_id: (mask & MNT_ID != 0).then_some(mnt_id),
    })
}

/// The fields of a statx [`meta`] would read from its bytes.
pub fn from_statx(sx: &Statx) -> FileMeta {
    FileMeta {
        mode: sx.stx_mode,
        size: sx.stx_size,
        mtime_sec: sx.stx_mtime.tv_sec,
        mtime_nsec: sx.stx_mtime.tv_nsec,
        ino: sx.stx_ino,
        dev: rustix::fs::makedev(sx.stx_dev_major, sx.stx_dev_minor),
        mnt_id: (sx.stx_mask & MNT_ID != 0).then_some(sx.stx_mnt_id),
    }
}

fn take<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes.get(at..at + N)?.try_into().ok()
}

/// Write a statx in the layout [`meta`] reads. Zeroes the rest of the head.
pub fn serialize(sx: &Statx, out: &mut [u8]) {
    encode(&from_statx(sx), out);
}

/// Write a [`FileMeta`] in the layout [`meta`] reads. `stx_mask` carries `MNT_ID` alone.
pub fn encode(m: &FileMeta, out: &mut [u8]) {
    let n = (META_LEN as usize).min(out.len());
    out[..n].fill(0);
    let mask = if m.mnt_id.is_some() { MNT_ID } else { 0 };
    put(out, AT_MASK, &mask.to_ne_bytes());
    put(out, AT_MODE, &m.mode.to_ne_bytes());
    put(out, AT_INO, &m.ino.to_ne_bytes());
    put(out, AT_SIZE, &m.size.to_ne_bytes());
    put(out, AT_MTIME, &m.mtime_sec.to_ne_bytes());
    put(out, AT_MTIME + 8, &m.mtime_nsec.to_ne_bytes());
    put(out, AT_DEV_MAJOR, &rustix::fs::major(m.dev).to_ne_bytes());
    put(out, AT_DEV_MINOR, &rustix::fs::minor(m.dev).to_ne_bytes());
    put(out, AT_MNT_ID, &m.mnt_id.unwrap_or(0).to_ne_bytes());
}

fn put(out: &mut [u8], at: usize, bytes: &[u8]) {
    if let Some(slot) = out.get_mut(at..at + bytes.len()) {
        slot.copy_from_slice(bytes);
    }
}

#[cfg(test)]
mod tests;
