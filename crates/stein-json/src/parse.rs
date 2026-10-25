//! The parser. One pass, an explicit stack, nodes out in document order.
//!
//! Strings and whitespace runs are scanned 32 bytes at a time: one load,
//! a few compares and a bitmask find the next quote, backslash or control
//! byte. The punctuation between values is one byte at a time, since it
//! comes one byte at a time. The SIMD level is detected once per parse and
//! the whole walk is compiled for it, falling back to plain lanes where
//! there is no vector unit.
//!
//! Nodes go to a `Sink`: a counter, a fixed slice, or a growing `Vec`.

use fearless_simd::{Level, Simd, SimdBase, dispatch, u8x32};

use crate::doc::Doc;
use crate::text;
use crate::{ErrorKind, JsonError, Kind, Node, Span};

/// Deepest nesting accepted. Sway's tree is a dozen deep at most.
pub const MAX_DEPTH: usize = 128;

/// How many nodes `src` needs, so the caller can size the slice.
pub fn count(src: &[u8]) -> Result<usize, JsonError> {
    run(src, Count { len: 0 }).map(|s| s.len)
}

/// Parse a whole document into `nodes`.
pub fn parse<'a>(src: &'a [u8], nodes: &'a mut [Node]) -> Result<Doc<'a>, JsonError> {
    let sink = run(src, Fixed { nodes, len: 0 })?;
    let len = sink.len;
    let nodes: &'a [Node] = sink.nodes;
    Ok(Doc::new(src, &nodes[..len]))
}

/// Parse a whole document into `nodes`, growing it as needed, with no
/// count first. The vector is storage: it is grown by doubling and never
/// shrunk, so a reused one stops allocating once it has held the largest
/// document, and [`Doc::nodes`] is the document, not the vector, whose
/// tail past it is stale.
#[cfg(feature = "alloc")]
pub fn parse_vec<'a>(
    src: &'a [u8],
    nodes: &'a mut alloc::vec::Vec<Node>,
) -> Result<Doc<'a>, JsonError> {
    let sink = run(src, Grow { nodes, len: 0 })?;
    let len = sink.len;
    let nodes: &'a [Node] = sink.nodes;
    Ok(Doc::new(src, &nodes[..len]))
}

fn run<K: Sink>(src: &[u8], sink: K) -> Result<K, JsonError> {
    if src.len() > u32::MAX as usize {
        return Err(err(ErrorKind::TooLarge, 0));
    }
    let level = Level::new();
    dispatch!(level, simd => Parser::new(simd, src, sink).run())
}

#[inline]
fn err(kind: ErrorKind, at: usize) -> JsonError {
    JsonError {
        kind,
        at: at as u32,
    }
}

/// Where nodes go.
trait Sink {
    fn len(&self) -> usize;
    /// Store node `n`, found at byte `at`, and return its index.
    fn push(&mut self, n: Node, at: usize) -> Result<u32, JsonError>;
    /// Container `i` ends at node `end`, its closing bracket at byte `close`.
    fn close(&mut self, i: u32, end: u32, close: usize);
}

/// Only counts.
struct Count {
    len: usize,
}

impl Sink for Count {
    #[inline(always)]
    fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    fn push(&mut self, _n: Node, _at: usize) -> Result<u32, JsonError> {
        let i = self.len;
        self.len += 1;
        Ok(i as u32)
    }

    #[inline(always)]
    fn close(&mut self, _i: u32, _end: u32, _close: usize) {}
}

/// A slice the caller sized.
struct Fixed<'n> {
    nodes: &'n mut [Node],
    len: usize,
}

impl Sink for Fixed<'_> {
    #[inline(always)]
    fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    fn push(&mut self, n: Node, at: usize) -> Result<u32, JsonError> {
        let i = self.len;
        match self.nodes.get_mut(i) {
            Some(slot) => *slot = n,
            None => return Err(err(ErrorKind::TooManyNodes, at)),
        }
        self.len = i + 1;
        Ok(i as u32)
    }

    #[inline(always)]
    fn close(&mut self, i: u32, end: u32, close: usize) {
        if let Some(n) = self.nodes.get_mut(i as usize) {
            n.end = end;
            n.span.len = close as u32 + 1 - n.span.at;
        }
    }
}

/// A vector used as storage, doubled when full. Its own length is the
/// high water mark, so `len` here is the document's, and a push is a
/// store into a slot that already exists rather than a `Vec::push` whose
/// length round trip through memory would chain one node to the next.
#[cfg(feature = "alloc")]
struct Grow<'n> {
    nodes: &'n mut alloc::vec::Vec<Node>,
    len: usize,
}

#[cfg(feature = "alloc")]
impl Grow<'_> {
    #[cold]
    #[inline(never)]
    fn grow(&mut self) {
        let want = (self.nodes.len() * 2).max(256);
        self.nodes.resize(want, Node::EMPTY);
    }
}

#[cfg(feature = "alloc")]
impl Sink for Grow<'_> {
    #[inline(always)]
    fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    fn push(&mut self, n: Node, _at: usize) -> Result<u32, JsonError> {
        let i = self.len;
        if i >= self.nodes.len() {
            self.grow();
        }
        if let Some(slot) = self.nodes.get_mut(i) {
            *slot = n;
        }
        self.len = i + 1;
        Ok(i as u32)
    }

    #[inline(always)]
    fn close(&mut self, i: u32, end: u32, close: usize) {
        if let Some(n) = self.nodes.get_mut(i as usize) {
            n.end = end;
            n.span.len = close as u32 + 1 - n.span.at;
        }
    }
}

const QUOTE: u8 = b'"';
const BACKSLASH: u8 = b'\\';

struct Parser<'a, S: Simd, K> {
    simd: S,
    src: &'a [u8],
    sink: K,
    /// The node of each open container, and whether it is an object.
    stack: [u32; MAX_DEPTH],
    kinds: [bool; MAX_DEPTH],
    depth: usize,
}

/// Where the walk goes after a value.
enum Next {
    /// Into the value at this byte.
    Value(usize),
    /// The root is closed, the rest must be whitespace.
    Done(usize),
}

impl<'a, S: Simd, K: Sink> Parser<'a, S, K> {
    #[inline(always)]
    fn new(simd: S, src: &'a [u8], sink: K) -> Self {
        Self {
            simd,
            src,
            sink,
            stack: [0; MAX_DEPTH],
            kinds: [false; MAX_DEPTH],
            depth: 0,
        }
    }

    /// The states are places in the code: a value, then the punctuation
    /// after it until the next value or the end. The position is a local
    /// through both so it lives in a register.
    #[inline(always)]
    fn run(mut self) -> Result<K, JsonError> {
        let src = self.src;
        let mut pos = 0;
        let mut in_object = false;
        loop {
            let at = self.skip_ws(pos);
            pos = match src.get(at).copied() {
                Some(QUOTE) => self.string(at)?,
                // A number in an object is one of a kind, in an array it
                // is likely one of many.
                Some(b'-' | b'0'..=b'9') if in_object => {
                    let len = number(&src[at..]).ok_or(err(ErrorKind::BadNumber, at))?;
                    self.leaf(Kind::Number, false, at, len)?;
                    at + len
                }
                Some(b'-' | b'0'..=b'9') => self.numbers(at)?,
                Some(b'{') => {
                    self.open(at, true)?;
                    in_object = true;
                    let p = self.skip_ws(at + 1);
                    if src.get(p) != Some(&b'}') {
                        pos = self.key(p)?;
                        continue;
                    }
                    p
                }
                Some(b'[') => {
                    self.open(at, false)?;
                    in_object = false;
                    let p = self.skip_ws(at + 1);
                    if src.get(p) != Some(&b']') {
                        pos = p;
                        continue;
                    }
                    p
                }
                Some(b't') => self.literal(at, b"true", Kind::True)?,
                Some(b'f') => self.literal(at, b"false", Kind::False)?,
                Some(b'n') => self.literal(at, b"null", Kind::Null)?,
                Some(_) => return Err(err(ErrorKind::BadByte, at)),
                None => return Err(err(ErrorKind::Truncated, at)),
            };
            match self.after(pos, &mut in_object)? {
                Next::Value(p) => pos = p,
                Next::Done(p) => {
                    if p < src.len() {
                        return Err(err(ErrorKind::Trailing, p));
                    }
                    return Ok(self.sink);
                }
            }
        }
    }

    /// From just after a value, or a bracket that just closed with `pos` on
    /// it: through commas, keys and closers to the next value or the end.
    #[inline(always)]
    fn after(&mut self, mut pos: usize, in_object: &mut bool) -> Result<Next, JsonError> {
        let src = self.src;
        loop {
            pos = self.skip_ws(pos);
            if self.depth == 0 {
                return Ok(Next::Done(pos));
            }
            match src.get(pos).copied() {
                Some(b',') => {
                    pos += 1;
                    if *in_object {
                        pos = self.key(self.skip_ws(pos))?;
                    }
                    return Ok(Next::Value(pos));
                }
                Some(b'}') if *in_object => {}
                Some(b']') if !*in_object => {}
                Some(_) => return Err(err(ErrorKind::BadByte, pos)),
                None => return Err(err(ErrorKind::Truncated, pos)),
            }
            self.close(pos);
            *in_object = self.depth > 0 && self.kinds[self.depth - 1];
            pos += 1;
        }
    }

    /// Thirty two bytes from `i`, zero padded past the end. The mask has a
    /// bit per byte that is a quote, a backslash or a control byte, so the
    /// padding reads as a control byte and the end of input is found by
    /// looking at the byte the mask points at.
    #[inline(always)]
    fn block(&self, i: usize) -> (u64, u8x32<S>) {
        let simd = self.simd;
        let v = match self.src.get(i..).and_then(|s| s.first_chunk::<32>()) {
            Some(c) => u8x32::load_array_ref(simd, c),
            None => {
                let mut tail = [0u8; 32];
                let rest = self.src.get(i..).unwrap_or(&[]);
                if let Some(dst) = tail.get_mut(..rest.len()) {
                    dst.copy_from_slice(rest);
                }
                u8x32::load_array_ref(simd, &tail)
            }
        };
        let quote = simd.simd_eq_u8x32(v, simd.splat_u8x32(QUOTE));
        let slash = simd.simd_eq_u8x32(v, simd.splat_u8x32(BACKSLASH));
        let ctl = simd.simd_lt_u8x32(v, simd.splat_u8x32(0x20));
        (simd.to_bitmask_mask8x32(quote | slash | ctl), v)
    }

    /// Bit per lane of `v` that is not ASCII.
    #[inline(always)]
    fn high_bits(&self, v: u8x32<S>) -> u64 {
        let simd = self.simd;
        simd.to_bitmask_mask8x32(simd.simd_ge_u8x32(v, simd.splat_u8x32(0x80)))
    }

    /// The first byte at or after `pos` that is not whitespace.
    #[inline(always)]
    fn skip_ws(&self, pos: usize) -> usize {
        match self.src.get(pos) {
            Some(&b) if is_ws(b) => self.skip_ws_run(pos + 1),
            _ => pos,
        }
    }

    /// The rest of a whitespace run, a block at a time while it lasts.
    #[inline(always)]
    fn skip_ws_run(&self, mut pos: usize) -> usize {
        let simd = self.simd;
        while let Some(c) = self.src.get(pos..).and_then(|s| s.first_chunk::<32>()) {
            let v = u8x32::load_array_ref(simd, c);
            let ws = simd.simd_eq_u8x32(v, simd.splat_u8x32(b' '))
                | simd.simd_eq_u8x32(v, simd.splat_u8x32(b'\n'))
                | simd.simd_eq_u8x32(v, simd.splat_u8x32(b'\r'))
                | simd.simd_eq_u8x32(v, simd.splat_u8x32(b'\t'));
            let m = simd.to_bitmask_mask8x32(ws) as u32;
            pos += m.trailing_ones() as usize;
            if m != u32::MAX {
                return pos;
            }
        }
        while let Some(&b) = self.src.get(pos) {
            if !is_ws(b) {
                break;
            }
            pos += 1;
        }
        pos
    }

    #[inline(always)]
    fn leaf(&mut self, kind: Kind, escaped: bool, at: usize, len: usize) -> Result<(), JsonError> {
        let end = self.sink.len() as u32 + 1;
        let span = Span {
            at: at as u32,
            len: len as u32,
        };
        self.sink.push(
            Node {
                kind,
                escaped,
                span,
                end,
            },
            at,
        )?;
        Ok(())
    }

    /// The number at `at`, and in an array every number that follows it
    /// with only a comma between, which is what a token array is. Returns
    /// the byte after the last one. Out of line so its few live values
    /// stay in registers rather than in the frame of the whole walk.
    #[inline(never)]
    fn numbers(&mut self, at: usize) -> Result<usize, JsonError> {
        let simd = self.simd;
        simd.vectorize(
            #[inline(always)]
            || self.numbers_lanes(at),
        )
    }

    #[inline(always)]
    fn numbers_lanes(&mut self, mut at: usize) -> Result<usize, JsonError> {
        let src = self.src;
        // A root number stands alone, whatever follows it.
        let run = self.depth > 0;
        // A long number was a float or a big count, so its neighbours are
        // not likely the short whole numbers the block path is for.
        let mut short = true;
        loop {
            if run
                && short
                && src.get(at).is_some_and(u8::is_ascii_digit)
                && let Some(c) = src.get(at..).and_then(|s| s.first_chunk::<32>())
                && let Some(next) = self.digit_runs(at, c)?
            {
                at = next;
                continue;
            }
            let len = number(&src[at..]).ok_or(err(ErrorKind::BadNumber, at))?;
            self.leaf(Kind::Number, false, at, len)?;
            let pos = at + len;
            let next = src.get(pos + 1).copied().unwrap_or(0);
            if !run || src.get(pos) != Some(&b',') || !(next == b'-' || next.is_ascii_digit()) {
                return Ok(pos);
            }
            short = len <= 6;
            at = pos + 1;
        }
    }

    /// The whole numbers in the block `c` at `at`, each followed by a comma
    /// and another digit, as far as the block holds nothing else. Every
    /// number is one node from the lane masks, with no dependence between
    /// one and the next. Where the next number starts, or `None` when the
    /// block starts with something for the byte at a time path.
    #[inline(always)]
    fn digit_runs(&mut self, at: usize, c: &[u8; 32]) -> Result<Option<usize>, JsonError> {
        let simd = self.simd;
        let v = u8x32::load_array_ref(simd, c);
        let ten = simd.splat_u8x32(10);
        let zero = simd.splat_u8x32(b'0');
        let digit = simd.simd_lt_u8x32(simd.sub_u8x32(v, zero), ten);
        let d = simd.to_bitmask_mask8x32(digit) as u32;
        let commas = simd.to_bitmask_mask8x32(simd.simd_eq_u8x32(v, simd.splat_u8x32(b','))) as u32;
        let zeros = simd.to_bitmask_mask8x32(simd.simd_eq_u8x32(v, zero)) as u32;
        let mut starts = d & !(d << 1);
        // Anything else, a comma without a digit after, or a leading zero
        // ends what can be trusted.
        let bad = !(d | commas) | (commas & !(d >> 1)) | (starts & zeros & (d >> 1));
        let below = ((1u64 << bad.trailing_zeros()) - 1) as u32;
        let mut ends = d & !(d >> 1) & (commas >> 1) & (d >> 2) & below;
        if ends == 0 {
            return Ok(None);
        }
        let mut last = 0;
        while ends != 0 {
            let s = starts.trailing_zeros() as usize;
            let e = ends.trailing_zeros() as usize;
            self.leaf(Kind::Number, false, at + s, e + 1 - s)?;
            starts &= starts - 1;
            ends &= ends - 1;
            last = e;
        }
        Ok(Some(at + last + 2))
    }

    /// Open a container on the bracket at `at`.
    #[inline(always)]
    fn open(&mut self, at: usize, object: bool) -> Result<(), JsonError> {
        if self.depth == MAX_DEPTH {
            return Err(err(ErrorKind::TooDeep, at));
        }
        let kind = if object { Kind::Object } else { Kind::Array };
        let node = self.sink.push(
            Node {
                kind,
                escaped: false,
                span: Span {
                    at: at as u32,
                    len: 1,
                },
                end: 0,
            },
            at,
        )?;
        self.stack[self.depth] = node;
        self.kinds[self.depth] = object;
        self.depth += 1;
        Ok(())
    }

    /// Close the innermost container on the bracket at `pos`.
    #[inline(always)]
    fn close(&mut self, pos: usize) {
        let d = self.depth.saturating_sub(1);
        let node = self.stack[d];
        self.sink.close(node, self.sink.len() as u32, pos);
        self.depth = d;
    }

    /// A key at `pos` and its colon, returning where the value starts.
    #[inline(always)]
    fn key(&mut self, pos: usize) -> Result<usize, JsonError> {
        let pos = match self.src.get(pos) {
            Some(&QUOTE) => self.string(pos)?,
            Some(_) => return Err(err(ErrorKind::BadKey, pos)),
            None => return Err(err(ErrorKind::Truncated, pos)),
        };
        let pos = self.skip_ws(pos);
        match self.src.get(pos) {
            Some(b':') => Ok(pos + 1),
            Some(_) => Err(err(ErrorKind::BadByte, pos)),
            None => Err(err(ErrorKind::Truncated, pos)),
        }
    }

    #[inline(always)]
    fn literal(&mut self, at: usize, word: &[u8], kind: Kind) -> Result<usize, JsonError> {
        let got = self.src.get(at..at + word.len());
        if got != Some(word) {
            let kind = match got {
                None if word.starts_with(&self.src[at..]) => ErrorKind::Truncated,
                _ => ErrorKind::BadLiteral,
            };
            return Err(err(kind, at));
        }
        self.leaf(kind, false, at, word.len())?;
        Ok(at + word.len())
    }

    /// A string from its opening quote at `at`, returning the byte after
    /// its closing quote. Escapes are checked here so every later decode
    /// of it cannot fail. UTF-8 is checked only when a byte past ASCII was
    /// seen, which the lanes tell for free.
    #[inline(always)]
    fn string(&mut self, at: usize) -> Result<usize, JsonError> {
        let start = at + 1;
        let mut i = start;
        let mut escaped = false;
        let mut high = false;
        let end = loop {
            let (mask, v) = self.block(i);
            if mask == 0 {
                high |= self.high_bits(v) != 0;
                i += 32;
                continue;
            }
            let k = mask.trailing_zeros() as usize;
            high |= self.high_bits(v) & ((1 << k) - 1) != 0;
            i += k;
            match self.src.get(i) {
                None => return Err(err(ErrorKind::Truncated, i)),
                Some(&QUOTE) => break i,
                Some(&BACKSLASH) => {
                    escaped = true;
                    let n =
                        text::check_escape(&self.src[i..]).ok_or(err(ErrorKind::BadEscape, i))?;
                    i += n;
                }
                Some(_) => return Err(err(ErrorKind::Control, i)),
            }
        };
        if high && core::str::from_utf8(&self.src[start..end]).is_err() {
            return Err(err(ErrorKind::Utf8, start));
        }
        self.leaf(Kind::String, escaped, start, end - start)?;
        Ok(end + 1)
    }
}

#[inline(always)]
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// How many of the eight bytes in `x` are digits, counting from the low
/// end. Eight when all are.
#[inline(always)]
fn swar_digits(x: u64) -> usize {
    let t = x ^ 0x3030_3030_3030_3030;
    // A digit's byte is 0 to 9 here, so adding 0x76 leaves its top bit
    // clear, and any other byte sets it one way or the other.
    let bad = (t.wrapping_add(0x7676_7676_7676_7676) | t) & 0x8080_8080_8080_8080;
    (bad.trailing_zeros() / 8) as usize
}

/// The run of digits at `s[i..]`, eight at a time.
#[inline(always)]
fn digits(s: &[u8], mut i: usize) -> usize {
    while let Some(c) = s.get(i..).and_then(|r| r.first_chunk::<8>()) {
        let n = swar_digits(u64::from_le_bytes(*c));
        i += n;
        if n < 8 {
            return i;
        }
    }
    while s.get(i).is_some_and(|b| b.is_ascii_digit()) {
        i += 1;
    }
    i
}

/// The length of the number at the front of `s`, if it is one. A byte past
/// the end reads as zero, which no part of a number is.
#[inline(always)]
fn number(s: &[u8]) -> Option<usize> {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut i = usize::from(at(0) == b'-');
    match at(i) {
        b'0' => i += 1,
        b'1'..=b'9' => i = digits(s, i + 1),
        _ => return None,
    }
    match at(i) {
        b'.' | b'e' | b'E' => fraction_exponent(s, i),
        _ => Some(i),
    }
}

/// The fraction and exponent from `i`, which is on a `.` or an `e`. Out of
/// line so the whole number path stays small.
#[inline(never)]
fn fraction_exponent(s: &[u8], mut i: usize) -> Option<usize> {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    if at(i) == b'.' {
        let n = digits(s, i + 1);
        if n == i + 1 {
            return None;
        }
        i = n;
    }
    if (at(i) | 0x20) == b'e' {
        i += 1;
        if matches!(at(i), b'+' | b'-') {
            i += 1;
        }
        let n = digits(s, i);
        if n == i {
            return None;
        }
        i = n;
    }
    Some(i)
}

#[cfg(test)]
mod tests;
