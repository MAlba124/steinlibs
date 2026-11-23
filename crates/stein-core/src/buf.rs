//! Fixed-size block pool over caller-supplied storage.
//!
//! Block generations follow [`crate::handle`], even free, odd held, retired at the top,
//! so a copied id refuses to release a block it no longer holds.

use crate::handle::{RETIRED, settle};

/// Block index plus the generation it was acquired at. Plain data, fine to store in tables.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BlockId {
    index: u32,
    generation: u32,
}

impl BlockId {
    pub fn index(&self) -> u32 {
        self.index
    }
}

/// Per-block cursors and generation. Live bytes are `[consumed, filled)`,
/// `consumed <= filled <= block_size`.
#[derive(Clone, Copy, Debug)]
pub struct BlockMeta {
    filled: u32,
    consumed: u32,
    generation: u32,
}

impl BlockMeta {
    /// Fill value for backing arrays.
    pub const EMPTY: BlockMeta = BlockMeta {
        filled: 0,
        consumed: 0,
        generation: 0,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufError {
    /// No free block.
    Exhausted,
    /// Write does not fit, even after compaction.
    BlockFull,
    /// `storage` does not split evenly into a positive number of blocks.
    BadStorage { bytes: usize, blocks: usize },
    /// `meta` and `free` differ in length.
    LengthMismatch { meta: usize, free: usize },
    /// More blocks than a `u32` index addresses.
    TooManyBlocks(usize),
    /// Block size past the `u32` cursors. The pool as a whole may be larger.
    BlockTooLarge(usize),
    /// Release of a block this id does not hold, already released or reacquired. Refused.
    NotHeld(BlockId),
}

/// Size of each of `blocks` equal blocks carved from `bytes`.
fn block_size(bytes: usize, blocks: usize) -> Result<usize, BufError> {
    if blocks == 0 || !bytes.is_multiple_of(blocks) || bytes / blocks == 0 {
        return Err(BufError::BadStorage { bytes, blocks });
    }
    let size = bytes / blocks;
    if size > u32::MAX as usize {
        return Err(BufError::BlockTooLarge(size));
    }
    Ok(size)
}

pub struct BlockPool<'a> {
    storage: &'a mut [u8],
    meta: &'a mut [BlockMeta],
    /// Stack of free block indices. Live entries are `free[..free_len]`.
    free: &'a mut [u32],
    free_len: u32,
    retired: u32,
    block_size: usize,
}

impl<'a> BlockPool<'a> {
    /// Carve `storage` into `meta.len()` equal blocks. Length must be a multiple of the
    /// count and each block at most `u32::MAX` bytes.
    ///
    /// Generations in `meta` carry over, so ids from an earlier pool on the same `meta`
    /// cannot release here. Retired blocks stay retired.
    pub fn new(
        storage: &'a mut [u8],
        meta: &'a mut [BlockMeta],
        free: &'a mut [u32],
    ) -> Result<Self, BufError> {
        if meta.len() != free.len() {
            return Err(BufError::LengthMismatch {
                meta: meta.len(),
                free: free.len(),
            });
        }
        if meta.len() > u32::MAX as usize {
            return Err(BufError::TooManyBlocks(meta.len()));
        }
        let block_size = block_size(storage.len(), meta.len())?;
        // Reset cursors so reused storage cannot leak them. Pushed descending so block 0
        // is acquired first.
        let mut free_len = 0;
        for i in (0..meta.len()).rev() {
            let generation = settle(meta[i].generation);
            meta[i] = BlockMeta {
                filled: 0,
                consumed: 0,
                generation,
            };
            if generation != RETIRED {
                free[free_len] = i as u32;
                free_len += 1;
            }
        }
        let retired = (meta.len() - free_len) as u32;
        Ok(BlockPool {
            storage,
            meta,
            free,
            free_len: free_len as u32,
            retired,
            block_size,
        })
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }
    pub fn free_blocks(&self) -> usize {
        self.free_len as usize
    }
    /// Blocks in the storage, retired ones included.
    pub fn total_blocks(&self) -> usize {
        self.meta.len()
    }
    /// Blocks whose generations ran out. They are never acquired again.
    pub fn retired_blocks(&self) -> usize {
        self.retired as usize
    }

    /// Total bytes held, the memory ceiling.
    pub fn bytes(&self) -> usize {
        self.storage.len()
    }

    pub fn acquire(&mut self) -> Result<BlockId, BufError> {
        if self.free_len == 0 {
            return Err(BufError::Exhausted);
        }
        self.free_len -= 1;
        let index = self.free[self.free_len as usize];
        let m = &mut self.meta[index as usize];
        // Free is even and below RETIRED, so odd and no overflow.
        let generation = m.generation + 1;
        *m = BlockMeta {
            filled: 0,
            consumed: 0,
            generation,
        };
        Ok(BlockId { index, generation })
    }

    /// Return a held block. Refuses an id that does not hold it, so a double release or a
    /// stale copy cannot hand one block to two owners.
    pub fn release(&mut self, id: BlockId) -> Result<(), BufError> {
        let Some(m) = self.meta.get_mut(id.index as usize) else {
            return Err(BufError::NotHeld(id));
        };
        if m.generation != id.generation {
            return Err(BufError::NotHeld(id));
        }
        // Held is odd and at most RETIRED - 1, so no overflow.
        let generation = id.generation + 1;
        *m = BlockMeta {
            filled: 0,
            consumed: 0,
            generation,
        };
        if generation == RETIRED {
            self.retired += 1;
        } else {
            self.free[self.free_len as usize] = id.index;
            self.free_len += 1;
        }
        Ok(())
    }

    /// Meta index for a held `id`.
    fn at(&self, id: BlockId) -> usize {
        let i = id.index as usize;
        debug_assert!(
            self.meta[i].generation == id.generation,
            "block {i} used by a stale id"
        );
        i
    }

    fn start(&self, id: BlockId) -> usize {
        id.index as usize * self.block_size
    }

    /// Written and not yet consumed bytes.
    pub fn data(&self, id: BlockId) -> &[u8] {
        let start = self.start(id);
        let m = self.meta[self.at(id)];
        &self.storage[start + m.consumed as usize..start + m.filled as usize]
    }

    pub fn len(&self, id: BlockId) -> usize {
        let m = self.meta[self.at(id)];
        (m.filled - m.consumed) as usize
    }

    pub fn is_empty(&self, id: BlockId) -> bool {
        self.len(id) == 0
    }

    /// Space for `append`, counting compaction.
    pub fn writable(&self, id: BlockId) -> usize {
        let m = self.meta[self.at(id)];
        self.block_size - (m.filled - m.consumed) as usize
    }

    /// Append bytes, compacting first if needed.
    pub fn append(&mut self, id: BlockId, bytes: &[u8]) -> Result<(), BufError> {
        if bytes.len() > self.writable(id) {
            return Err(BufError::BlockFull);
        }
        let i = self.at(id);
        if (self.block_size - self.meta[i].filled as usize) < bytes.len() {
            self.compact(i);
        }
        let at = self.start(id) + self.meta[i].filled as usize;
        self.storage[at..at + bytes.len()].copy_from_slice(bytes);
        // Fits in u32, the block size does.
        self.meta[i].filled += bytes.len() as u32;
        Ok(())
    }

    /// Mark `n` bytes at the front as consumed.
    pub fn consume(&mut self, id: BlockId, n: usize) {
        let i = self.at(id);
        let m = &mut self.meta[i];
        let avail = (m.filled - m.consumed) as usize;
        debug_assert!(n <= avail, "consumed {n} of {avail} available");
        let n = n.min(avail) as u32;
        m.consumed += n;
        if m.consumed == m.filled {
            m.filled = 0;
            m.consumed = 0;
        }
    }

    /// Move block `i`'s live bytes to its front.
    fn compact(&mut self, i: usize) {
        let m = self.meta[i];
        if m.consumed == 0 {
            return;
        }
        let start = i * self.block_size;
        self.storage.copy_within(
            start + m.consumed as usize..start + m.filled as usize,
            start,
        );
        self.meta[i].filled = m.filled - m.consumed;
        self.meta[i].consumed = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stack backing storage.
    macro_rules! blocks {
        ($size:expr, $count:expr) => {{
            (
                [0u8; $size * $count],
                [BlockMeta::EMPTY; $count],
                [0u32; $count],
            )
        }};
    }

    #[test]
    fn acquire_release_roundtrip() {
        let (mut s, mut m, mut f) = blocks!(16, 2);
        let mut p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
        assert_eq!(p.free_blocks(), 2);
        let a = p.acquire().expect("free");
        assert_eq!(p.free_blocks(), 1);
        assert_eq!(p.release(a), Ok(()));
        assert_eq!(p.free_blocks(), 2);
    }

    #[test]
    fn block_size_is_derived_from_the_storage() {
        let (mut s, mut m, mut f) = blocks!(16, 4);
        let p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
        assert_eq!(p.block_size(), 16);
        assert_eq!(p.total_blocks(), 4);
        assert_eq!(p.bytes(), 64);
    }

    #[test]
    fn exhaustion_is_reported_not_grown() {
        let (mut s, mut m, mut f) = blocks!(16, 1);
        let mut p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
        let _a = p.acquire().expect("free");
        assert_eq!(p.acquire(), Err(BufError::Exhausted));
        assert_eq!(p.total_blocks(), 1, "must not grow");
    }

    #[test]
    fn append_and_consume() {
        let (mut s, mut m, mut f) = blocks!(8, 1);
        let mut p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
        let b = p.acquire().expect("free");
        p.append(b, b"hello").expect("fits");
        assert_eq!(p.data(b), b"hello");
        p.consume(b, 2);
        assert_eq!(p.data(b), b"llo");
        assert_eq!(p.len(b), 3);
    }

    #[test]
    fn full_consume_resets_the_cursor() {
        let (mut s, mut m, mut f) = blocks!(8, 1);
        let mut p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
        let b = p.acquire().expect("free");
        p.append(b, b"abcd").expect("fits");
        p.consume(b, 4);
        assert!(p.is_empty(b));
        assert_eq!(p.writable(b), 8, "fully consumed block is fully writable");
    }

    #[test]
    fn compaction_reclaims_the_front() {
        let (mut s, mut m, mut f) = blocks!(8, 1);
        let mut p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
        let b = p.acquire().expect("free");
        p.append(b, b"abcdef").expect("fits");
        p.consume(b, 4); // live = "ef", 4 bytes dead at the front
        // 6 bytes would not fit after `filled` without compaction
        p.append(b, b"123456").expect("compaction makes room");
        assert_eq!(p.data(b), b"ef123456");
    }

    #[test]
    fn overfull_append_is_refused() {
        let (mut s, mut m, mut f) = blocks!(4, 1);
        let mut p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
        let b = p.acquire().expect("free");
        assert_eq!(p.append(b, b"toolong"), Err(BufError::BlockFull));
        assert!(p.is_empty(b), "failed append must not partially write");
    }

    #[test]
    fn released_block_is_clean() {
        let (mut s, mut m, mut f) = blocks!(8, 1);
        let mut p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
        let b = p.acquire().expect("free");
        p.append(b, b"dirty").expect("fits");
        assert_eq!(p.release(b), Ok(()));
        let c = p.acquire().expect("free");
        assert!(p.is_empty(c), "reused block must not carry old data");
    }

    #[test]
    fn blocks_do_not_overlap() {
        let (mut s, mut m, mut f) = blocks!(4, 3);
        let mut p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
        let a = p.acquire().expect("free");
        let b = p.acquire().expect("free");
        let c = p.acquire().expect("free");
        p.append(a, b"aaaa").expect("fits");
        p.append(b, b"bbbb").expect("fits");
        p.append(c, b"cccc").expect("fits");
        assert_eq!(p.data(a), b"aaaa");
        assert_eq!(p.data(b), b"bbbb");
        assert_eq!(p.data(c), b"cccc");
    }

    #[test]
    fn reused_storage_starts_empty() {
        let (mut s, mut m, mut f) = blocks!(8, 1);
        {
            let mut p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
            let b = p.acquire().expect("free");
            p.append(b, b"stale").expect("fits");
        }
        let mut p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
        assert_eq!(p.free_blocks(), 1, "every block is free again");
        let b = p.acquire().expect("free");
        assert!(p.is_empty(b), "a fresh pool must not inherit old cursors");
    }

    #[test]
    fn ragged_storage_is_refused() {
        let mut s = [0u8; 10];
        let mut m = [BlockMeta::EMPTY; 4];
        let mut f = [0u32; 4];
        assert_eq!(
            BlockPool::new(&mut s, &mut m, &mut f).err(),
            Some(BufError::BadStorage {
                bytes: 10,
                blocks: 4
            })
        );
    }

    #[test]
    fn storage_smaller_than_the_block_count_is_refused() {
        let mut s = [0u8; 0];
        let mut m = [BlockMeta::EMPTY; 2];
        let mut f = [0u32; 2];
        assert_eq!(
            BlockPool::new(&mut s, &mut m, &mut f).err(),
            Some(BufError::BadStorage {
                bytes: 0,
                blocks: 2
            })
        );
    }

    #[test]
    fn mismatched_metadata_is_refused() {
        let mut s = [0u8; 16];
        let mut m = [BlockMeta::EMPTY; 2];
        let mut f = [0u32; 3];
        assert_eq!(
            BlockPool::new(&mut s, &mut m, &mut f).err(),
            Some(BufError::LengthMismatch { meta: 2, free: 3 })
        );
    }

    #[test]
    fn double_release_cannot_hand_out_one_block_twice() {
        let (mut s, mut m, mut f) = blocks!(8, 3);
        let mut p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
        let a = p.acquire().expect("free");
        let _b = p.acquire().expect("free");
        assert_eq!(p.release(a), Ok(()));
        assert_eq!(p.release(a), Err(BufError::NotHeld(a)));
        assert_eq!(p.free_blocks(), 2, "the refused release pushed nothing");
        let x = p.acquire().expect("free");
        let y = p.acquire().expect("free");
        assert_ne!(x.index(), y.index(), "two owners of one block");
        assert_eq!(p.acquire(), Err(BufError::Exhausted));
    }

    #[test]
    fn stale_copy_cannot_release_the_next_owner() {
        let (mut s, mut m, mut f) = blocks!(8, 2);
        let mut p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
        let a = p.acquire().expect("free");
        let stale = a;
        assert_eq!(p.release(a), Ok(()));
        let b = p.acquire().expect("free");
        assert_eq!(a.index(), b.index(), "setup: the block is reused");
        p.append(b, b"live").expect("fits");
        assert_eq!(p.release(stale), Err(BufError::NotHeld(stale)));
        assert_eq!(p.data(b), b"live", "the owner lost its bytes");
        let c = p.acquire().expect("one block is free");
        assert_ne!(c.index(), b.index(), "two owners of one block");
        assert_eq!(p.release(b), Ok(()));
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn blocks_beyond_the_u32_cursors_are_refused() {
        // Checked on sizes alone, nothing is allocated.
        let four_gib = u32::MAX as usize + 1;
        assert_eq!(
            block_size(four_gib, 1),
            Err(BufError::BlockTooLarge(four_gib)),
            "cursor would wrap"
        );
        assert_eq!(
            block_size(2 * four_gib, 2),
            Err(BufError::BlockTooLarge(four_gib))
        );
        // The total may exceed 4 GiB, each block may not.
        assert_eq!(block_size(2 * (four_gib - 1), 2), Ok(four_gib - 1));
        assert_eq!(block_size(4 * four_gib, 8), Ok(four_gib / 2));
    }

    #[test]
    fn rebuilt_pool_refuses_ids_from_its_last_life() {
        let (mut s, mut m, mut f) = blocks!(8, 2);
        let old = {
            let mut p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
            p.acquire().expect("free")
        };
        let mut p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
        assert_eq!(p.free_blocks(), 2, "every block is free again");
        let b = p.acquire().expect("free");
        assert_eq!(old.index(), b.index(), "setup: the block is reused");
        assert_eq!(p.release(old), Err(BufError::NotHeld(old)));
        assert_eq!(p.release(b), Ok(()));
    }

    #[test]
    fn exhausted_block_generation_retires_the_block() {
        let (mut s, mut m, mut f) = blocks!(8, 2);
        let mut p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
        // Block 0 is free with one held generation left.
        p.meta[0].generation = RETIRED - 2;
        let a = p.acquire().expect("free");
        assert_eq!((a.index(), a.generation), (0, RETIRED - 1));
        assert_eq!(p.release(a), Ok(()));
        assert_eq!(p.retired_blocks(), 1);
        assert_eq!(p.free_blocks(), 1, "a retired block left the free stack");
        let b = p.acquire().expect("block 1 is free");
        assert_eq!(b.index(), 1, "a retired block was reissued");
        assert_eq!(p.acquire(), Err(BufError::Exhausted));
        assert_eq!(p.release(a), Err(BufError::NotHeld(a)));
        assert_eq!(p.release(b), Ok(()));
        let p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
        assert_eq!((p.retired_blocks(), p.free_blocks()), (1, 1), "retirement survives");
    }

    #[test]
    fn ceiling_is_statable() {
        let mut s = [0u8; 4096 * 10];
        let mut m = [BlockMeta::EMPTY; 10];
        let mut f = [0u32; 10];
        let p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
        assert_eq!(p.bytes(), 40_960);
    }
}
