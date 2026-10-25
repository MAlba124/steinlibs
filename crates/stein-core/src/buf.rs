//! Fixed-size block pool over caller-supplied storage.

/// Block index. Not generational, a block has exactly one owner.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BlockId(u32);

impl BlockId {
    pub fn index(&self) -> u32 {
        self.0
    }
}

/// Per-block cursors. Live bytes are `[consumed, filled)`, `consumed <= filled <= block_size`.
#[derive(Clone, Copy, Debug)]
pub struct BlockMeta {
    filled: u32,
    consumed: u32,
}

impl BlockMeta {
    /// Fill value for backing arrays.
    pub const EMPTY: BlockMeta = BlockMeta {
        filled: 0,
        consumed: 0,
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
}

pub struct BlockPool<'a> {
    storage: &'a mut [u8],
    meta: &'a mut [BlockMeta],
    /// Stack of free block indices. Live entries are `free[..free_len]`.
    free: &'a mut [u32],
    free_len: u32,
    block_size: usize,
}

impl<'a> BlockPool<'a> {
    /// Carve `storage` into `meta.len()` equal blocks. Length must be a multiple of the count.
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
        let blocks = meta.len();
        if blocks == 0 || !storage.len().is_multiple_of(blocks) || storage.len() / blocks == 0 {
            return Err(BufError::BadStorage {
                bytes: storage.len(),
                blocks,
            });
        }
        let block_size = storage.len() / blocks;
        let n = blocks as u32;
        // Reset so reused storage cannot leak stale cursors.
        for m in meta.iter_mut() {
            *m = BlockMeta::EMPTY;
        }
        for (i, f) in free.iter_mut().enumerate() {
            *f = n - 1 - i as u32;
        }
        Ok(BlockPool {
            storage,
            meta,
            free,
            free_len: n,
            block_size,
        })
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }
    pub fn free_blocks(&self) -> usize {
        self.free_len as usize
    }
    pub fn total_blocks(&self) -> usize {
        self.meta.len()
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
        let id = self.free[self.free_len as usize];
        self.meta[id as usize] = BlockMeta::EMPTY;
        Ok(BlockId(id))
    }

    pub fn release(&mut self, id: BlockId) {
        debug_assert!(
            !self.free[..self.free_len as usize].contains(&id.0),
            "double release of block {}",
            id.0
        );
        self.meta[id.0 as usize] = BlockMeta::EMPTY;
        self.free[self.free_len as usize] = id.0;
        self.free_len += 1;
    }

    fn start(&self, id: BlockId) -> usize {
        id.0 as usize * self.block_size
    }

    /// Written and not yet consumed bytes.
    pub fn data(&self, id: BlockId) -> &[u8] {
        let start = self.start(id);
        let m = self.meta[id.0 as usize];
        &self.storage[start + m.consumed as usize..start + m.filled as usize]
    }

    pub fn len(&self, id: BlockId) -> usize {
        let m = self.meta[id.0 as usize];
        (m.filled - m.consumed) as usize
    }

    pub fn is_empty(&self, id: BlockId) -> bool {
        self.len(id) == 0
    }

    /// Space for `append`, counting compaction.
    pub fn writable(&self, id: BlockId) -> usize {
        let m = self.meta[id.0 as usize];
        self.block_size - (m.filled - m.consumed) as usize
    }

    /// Append bytes, compacting first if needed.
    pub fn append(&mut self, id: BlockId, bytes: &[u8]) -> Result<(), BufError> {
        if bytes.len() > self.writable(id) {
            return Err(BufError::BlockFull);
        }
        let m = self.meta[id.0 as usize];
        if (self.block_size - m.filled as usize) < bytes.len() {
            self.compact(id);
        }
        let start = self.start(id);
        let m = self.meta[id.0 as usize];
        let at = start + m.filled as usize;
        self.storage[at..at + bytes.len()].copy_from_slice(bytes);
        self.meta[id.0 as usize].filled += bytes.len() as u32;
        Ok(())
    }

    /// Mark `n` bytes at the front as consumed.
    pub fn consume(&mut self, id: BlockId, n: usize) {
        let m = &mut self.meta[id.0 as usize];
        let avail = (m.filled - m.consumed) as usize;
        debug_assert!(n <= avail, "consumed {n} of {avail} available");
        let n = n.min(avail) as u32;
        m.consumed += n;
        if m.consumed == m.filled {
            *m = BlockMeta::EMPTY;
        }
    }

    /// Move live bytes to the front.
    fn compact(&mut self, id: BlockId) {
        let m = self.meta[id.0 as usize];
        if m.consumed == 0 {
            return;
        }
        let start = self.start(id);
        let live = (m.filled - m.consumed) as usize;
        self.storage.copy_within(
            start + m.consumed as usize..start + m.filled as usize,
            start,
        );
        self.meta[id.0 as usize] = BlockMeta {
            filled: live as u32,
            consumed: 0,
        };
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
        p.release(a);
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
        p.release(b);
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
    fn ceiling_is_statable() {
        let mut s = [0u8; 4096 * 10];
        let mut m = [BlockMeta::EMPTY; 10];
        let mut f = [0u32; 10];
        let p = BlockPool::new(&mut s, &mut m, &mut f).expect("valid storage");
        assert_eq!(p.bytes(), 40_960);
    }
}
