//! Synchronous directory listing. `io_uring` has no getdents opcode.

use core::mem::MaybeUninit;

use rustix::fs::{FileType, RawDir};
use rustix::io::Errno;
use std::os::fd::BorrowedFd;

pub struct DirEntry<'a> {
    /// Bare name. Never `.` or `..`.
    pub name: &'a [u8],
    /// [`FileType::Unknown`] means stat to find out.
    pub kind: FileType,
}

/// Call `f` for every entry of the directory behind `fd`. Advances the
/// descriptor, so listing twice needs a fresh open.
pub fn list(
    fd: BorrowedFd<'_>,
    buf: &mut [MaybeUninit<u8>],
    mut f: impl FnMut(DirEntry<'_>),
) -> Result<(), Errno> {
    let mut dir = RawDir::new(fd, buf);
    while let Some(entry) = dir.next() {
        let e = entry?;
        let name = e.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        f(DirEntry {
            name,
            kind: e.file_type(),
        });
    }
    Ok(())
}

/// Packed entry header. `u16` name length, one type byte.
pub const PACK_HEAD: usize = 3;

pub const NAME_MAX: usize = 255;

/// Pack as many whole entries as fit into `out`, as [`entries`] reads them.
/// Returns bytes written, zero at end. The descriptor is left at the first
/// entry that did not fit. `EINVAL` if none fit.
pub fn pack(fd: BorrowedFd<'_>, out: &mut [u8]) -> Result<usize, Errno> {
    use rustix::fs::{SeekFrom, seek, tell};
    let mut raw = [MaybeUninit::<u8>::uninit(); 4096];
    // A cookie names the entry after the one carrying it, so this trails by one.
    let mut keep = tell(fd)?;
    let mut n = 0;
    let mut full = false;
    let mut dir = RawDir::new(fd, &mut raw);
    while let Some(entry) = dir.next() {
        let e = entry?;
        let name = e.file_name().to_bytes();
        let dots = name == b"." || name == b"..";
        let end = n + PACK_HEAD + name.len();
        if !dots && end > out.len() {
            full = true;
            break;
        }
        keep = e.next_entry_cookie();
        if dots {
            continue;
        }
        out[n..n + 2].copy_from_slice(&(name.len() as u16).to_ne_bytes());
        out[n + 2] = type_code(e.file_type());
        out[n + PACK_HEAD..end].copy_from_slice(name);
        n = end;
    }
    if !full {
        return Ok(n);
    }
    seek(fd, SeekFrom::Start(keep))?;
    if n == 0 {
        return Err(Errno::INVAL);
    }
    Ok(n)
}

/// Walk what [`pack`] wrote. Stops at the first partial entry.
pub fn entries(bytes: &[u8]) -> Entries<'_> {
    Entries { bytes }
}

pub struct Entries<'a> {
    bytes: &'a [u8],
}

impl<'a> Iterator for Entries<'a> {
    type Item = DirEntry<'a>;

    fn next(&mut self) -> Option<DirEntry<'a>> {
        let len = u16::from_ne_bytes(self.bytes.get(..2)?.try_into().ok()?) as usize;
        let kind = type_of(*self.bytes.get(2)?);
        let name = self.bytes.get(PACK_HEAD..PACK_HEAD + len)?;
        self.bytes = &self.bytes[PACK_HEAD + len..];
        Some(DirEntry { name, kind })
    }
}

/// `d_type` numbering.
fn type_code(t: FileType) -> u8 {
    match t {
        FileType::Fifo => 1,
        FileType::CharacterDevice => 2,
        FileType::Directory => 4,
        FileType::BlockDevice => 6,
        FileType::RegularFile => 8,
        FileType::Symlink => 10,
        FileType::Socket => 12,
        FileType::Unknown => 0,
    }
}

fn type_of(code: u8) -> FileType {
    match code {
        1 => FileType::Fifo,
        2 => FileType::CharacterDevice,
        4 => FileType::Directory,
        6 => FileType::BlockDevice,
        8 => FileType::RegularFile,
        10 => FileType::Symlink,
        12 => FileType::Socket,
        _ => FileType::Unknown,
    }
}

#[cfg(test)]
mod tests;
