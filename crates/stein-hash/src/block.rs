//! 64 byte block buffer and Merkle-Damgard padding of SHA-1.

pub const BLOCK: usize = 64;

#[derive(Clone, Copy)]
pub struct Block {
    pub buf: [u8; BLOCK],
    /// Used bytes of `buf`, always below `BLOCK`.
    pub len: u8,
    /// Total bytes fed.
    pub total: u64,
}

impl Block {
    pub const fn new() -> Block {
        Block {
            buf: [0; BLOCK],
            len: 0,
            total: 0,
        }
    }

    /// Calls `compress` per full block. Only the tail is copied.
    #[inline]
    pub fn feed(&mut self, bytes: &[u8], mut compress: impl FnMut(&[u8; BLOCK])) {
        self.total = self.total.wrapping_add(bytes.len() as u64);
        let Some(bytes) = self.absorb(bytes, &mut compress) else {
            return;
        };
        let (blocks, tail) = bytes.as_chunks::<BLOCK>();
        for block in blocks {
            compress(block);
        }
        self.buf[..tail.len()].copy_from_slice(tail);
        self.len = tail.len() as u8;
    }

    /// Top up a partial block. `None` if all fit, else the rest after compressing it.
    #[inline]
    fn absorb<'b>(
        &mut self,
        bytes: &'b [u8],
        compress: &mut impl FnMut(&[u8; BLOCK]),
    ) -> Option<&'b [u8]> {
        let used = self.len as usize;
        if used == 0 {
            return Some(bytes);
        }
        let room = BLOCK - used;
        if bytes.len() < room {
            self.buf[used..used + bytes.len()].copy_from_slice(bytes);
            self.len += bytes.len() as u8;
            return None;
        }
        let (head, rest) = bytes.split_at(room);
        self.buf[used..].copy_from_slice(head);
        compress(&self.buf);
        self.len = 0;
        Some(rest)
    }

    /// Pad and compress the final block or two.
    #[inline]
    pub fn finish(mut self, mut compress: impl FnMut(&[u8; BLOCK])) {
        let used = self.len as usize;
        let bits = self.total.wrapping_mul(8);
        self.buf[used] = 0x80;
        self.buf[used + 1..].fill(0);
        // No room for the length, so it goes in an extra zero block.
        if used + 1 > BLOCK - 8 {
            compress(&self.buf);
            self.buf.fill(0);
        }
        self.buf[BLOCK - 8..].copy_from_slice(&bits.to_be_bytes());
        compress(&self.buf);
    }
}
