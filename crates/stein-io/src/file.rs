//! [`crate::op::Kind::Stat`] buffer layout, the head of `struct statx` on both backends.

use rustix::fs::{FileType, Statx};

/// Bytes of the statx head [`meta`] reads. Ends after `stx_mtime` at 112.
pub const META_LEN: u32 = 128;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileMeta {
    /// Type and permission bits as in `st_mode`.
    pub mode: u16,
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
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
const AT_MODE: usize = 28;
const AT_SIZE: usize = 40;
const AT_MTIME: usize = 112;

/// Parse a stat buffer. `None` if shorter than [`META_LEN`].
pub fn meta(bytes: &[u8]) -> Option<FileMeta> {
    if bytes.len() < META_LEN as usize {
        return None;
    }
    Some(FileMeta {
        mode: u16::from_ne_bytes(take(bytes, AT_MODE)?),
        size: u64::from_ne_bytes(take(bytes, AT_SIZE)?),
        mtime_sec: i64::from_ne_bytes(take(bytes, AT_MTIME)?),
        mtime_nsec: u32::from_ne_bytes(take(bytes, AT_MTIME + 8)?),
    })
}

fn take<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes.get(at..at + N)?.try_into().ok()
}

/// Write a statx in the layout [`meta`] reads. Zeroes the rest of the head.
pub fn serialize(sx: &Statx, out: &mut [u8]) {
    let n = (META_LEN as usize).min(out.len());
    out[..n].fill(0);
    put(out, AT_MODE, &sx.stx_mode.to_ne_bytes());
    put(out, AT_SIZE, &sx.stx_size.to_ne_bytes());
    put(out, AT_MTIME, &sx.stx_mtime.tv_sec.to_ne_bytes());
    put(out, AT_MTIME + 8, &sx.stx_mtime.tv_nsec.to_ne_bytes());
}

/// Write a [`FileMeta`] in the layout [`meta`] reads.
pub fn encode(m: &FileMeta, out: &mut [u8]) {
    let n = (META_LEN as usize).min(out.len());
    out[..n].fill(0);
    put(out, AT_MODE, &m.mode.to_ne_bytes());
    put(out, AT_SIZE, &m.size.to_ne_bytes());
    put(out, AT_MTIME, &m.mtime_sec.to_ne_bytes());
    put(out, AT_MTIME + 8, &m.mtime_nsec.to_ne_bytes());
}

fn put(out: &mut [u8], at: usize, bytes: &[u8]) {
    if let Some(slot) = out.get_mut(at..at + bytes.len()) {
        slot.copy_from_slice(bytes);
    }
}

#[cfg(test)]
mod tests;
