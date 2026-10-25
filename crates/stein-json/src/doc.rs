//! Reading a parsed document. Everything is an index into the node table.

use crate::text::Text;
use crate::{Kind, Node};

/// The root value is always the first node.
pub const ROOT: u32 = 0;

/// A parsed document: the bytes, plus where everything is in them.
#[derive(Clone, Copy, Debug)]
pub struct Doc<'a> {
    src: &'a [u8],
    nodes: &'a [Node],
}

impl<'a> Doc<'a> {
    pub(crate) fn new(src: &'a [u8], nodes: &'a [Node]) -> Doc<'a> {
        Doc { src, nodes }
    }

    pub fn src(&self) -> &'a [u8] {
        self.src
    }

    pub fn nodes(&self) -> &'a [Node] {
        self.nodes
    }

    #[inline]
    pub fn node(&self, i: u32) -> Option<Node> {
        self.nodes.get(i as usize).copied()
    }

    #[inline]
    pub fn kind(&self, i: u32) -> Option<Kind> {
        self.node(i).map(|n| n.kind)
    }

    pub fn is_null(&self, i: u32) -> bool {
        self.kind(i) == Some(Kind::Null)
    }

    pub fn bool(&self, i: u32) -> Option<bool> {
        match self.kind(i)? {
            Kind::True => Some(true),
            Kind::False => Some(false),
            _ => None,
        }
    }

    /// A string value, escapes unresolved.
    pub fn text(&self, i: u32) -> Option<Text<'a>> {
        let n = self.node(i)?;
        if n.kind != Kind::String {
            return None;
        }
        // Checked UTF-8 at parse time.
        let raw = core::str::from_utf8(n.span.of(self.src)).ok()?;
        Some(Text::new(raw, n.escaped))
    }

    /// A string that needs no decoding. `None` for one with escapes, which
    /// [`Doc::text`] reads.
    pub fn str(&self, i: u32) -> Option<&'a str> {
        self.text(i)?.as_str()
    }

    /// A number's literal.
    pub fn number(&self, i: u32) -> Option<&'a str> {
        let n = self.node(i)?;
        if n.kind != Kind::Number {
            return None;
        }
        core::str::from_utf8(n.span.of(self.src)).ok()
    }

    /// A number with no fraction or exponent that fits.
    pub fn i64(&self, i: u32) -> Option<i64> {
        self.number(i)?.parse().ok()
    }

    pub fn u64(&self, i: u32) -> Option<u64> {
        self.number(i)?.parse().ok()
    }

    pub fn f64(&self, i: u32) -> Option<f64> {
        self.number(i)?.parse().ok()
    }

    /// Every element of array `i` as a `u32` into `out`, the shape of an
    /// LSP semantic token array. How many were written, or `None` when `i`
    /// is not an array, an element is not a whole number that fits, or
    /// `out` is too short.
    pub fn u32s_into(&self, i: u32, out: &mut [u32]) -> Option<usize> {
        if self.kind(i)? != Kind::Array {
            return None;
        }
        let mut n = 0;
        for c in self.children(i) {
            let node = self.node(c)?;
            if node.kind != Kind::Number {
                return None;
            }
            *out.get_mut(n)? = digits(node.span.of(self.src))?;
            n += 1;
        }
        Some(n)
    }

    /// [`Doc::u32s_into`] into a vector, which replaces its contents. On
    /// `None` the vector holds whatever was decoded before the trouble.
    #[cfg(feature = "alloc")]
    pub fn u32s(&self, i: u32, out: &mut alloc::vec::Vec<u32>) -> Option<usize> {
        out.clear();
        let n = self.node(i)?;
        if n.kind != Kind::Array {
            return None;
        }
        // Every element is one node when they are all numbers.
        out.resize((n.end as usize).saturating_sub(i as usize + 1), 0);
        let written = self.u32s_into(i, out)?;
        out.truncate(written);
        Some(written)
    }

    /// The whole encoding of a value as written.
    pub fn raw(&self, i: u32) -> &'a [u8] {
        let Some(n) = self.node(i) else {
            return &[];
        };
        let span = match n.kind {
            // The quotes around the contents.
            Kind::String => crate::Span {
                at: n.span.at - 1,
                len: n.span.len + 2,
            },
            _ => n.span,
        };
        span.of(self.src)
    }

    /// The elements of an array, or the keys and values of an object in
    /// turn. Empty for anything else.
    pub fn children(&self, i: u32) -> Children<'a> {
        let end = match self.node(i) {
            Some(n) if matches!(n.kind, Kind::Array | Kind::Object) => n.end,
            _ => i + 1,
        };
        Children {
            nodes: self.nodes,
            at: i + 1,
            end,
        }
    }

    /// How many elements an array has, or entries an object.
    pub fn len(&self, i: u32) -> usize {
        match self.kind(i) {
            Some(Kind::Object) => self.children(i).count() / 2,
            _ => self.children(i).count(),
        }
    }

    /// Element `n` of an array.
    pub fn index(&self, i: u32, n: usize) -> Option<u32> {
        match self.kind(i)? {
            Kind::Array => self.children(i).nth(n),
            _ => None,
        }
    }

    /// The key and value pairs of an object. Empty for anything else.
    pub fn entries(&self, i: u32) -> Entries<'a> {
        let children = match self.kind(i) {
            Some(Kind::Object) => self.children(i),
            _ => Children {
                nodes: self.nodes,
                at: i,
                end: i,
            },
        };
        Entries { children }
    }

    /// The value under `key` in object `i`. A linear scan; the last match
    /// wins when a key repeats.
    pub fn get(&self, i: u32, key: &str) -> Option<u32> {
        self.entries(i)
            .filter(|&(k, _)| self.text(k).is_some_and(|t| t.eq_str(key)))
            .last()
            .map(|(_, v)| v)
    }

    pub fn get_text(&self, i: u32, key: &str) -> Option<Text<'a>> {
        self.text(self.get(i, key)?)
    }

    pub fn get_str(&self, i: u32, key: &str) -> Option<&'a str> {
        self.str(self.get(i, key)?)
    }

    pub fn get_i64(&self, i: u32, key: &str) -> Option<i64> {
        self.i64(self.get(i, key)?)
    }

    pub fn get_bool(&self, i: u32, key: &str) -> Option<bool> {
        self.bool(self.get(i, key)?)
    }
}

/// A number literal as a `u32`: digits only, and it must fit.
#[inline]
fn digits(s: &[u8]) -> Option<u32> {
    if s.is_empty() || s.len() > 10 {
        return None;
    }
    let mut v: u64 = 0;
    for &b in s {
        let d = b.wrapping_sub(b'0');
        if d > 9 {
            return None;
        }
        v = v * 10 + u64::from(d);
    }
    u32::try_from(v).ok()
}

/// Sibling walk: each step jumps over a whole subtree.
#[derive(Clone, Copy)]
pub struct Children<'a> {
    nodes: &'a [Node],
    at: u32,
    end: u32,
}

impl Iterator for Children<'_> {
    type Item = u32;

    #[inline]
    fn next(&mut self) -> Option<u32> {
        if self.at >= self.end {
            return None;
        }
        let i = self.at;
        self.at = self.nodes.get(i as usize).map_or(self.end, |n| n.end);
        Some(i)
    }
}

/// Key and value indexes, in the order written.
#[derive(Clone, Copy)]
pub struct Entries<'a> {
    children: Children<'a>,
}

impl Iterator for Entries<'_> {
    type Item = (u32, u32);

    #[inline]
    fn next(&mut self) -> Option<(u32, u32)> {
        let k = self.children.next()?;
        let v = self.children.next()?;
        Some((k, v))
    }
}
