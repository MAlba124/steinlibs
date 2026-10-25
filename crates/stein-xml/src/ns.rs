//! Namespace bindings and qualified name resolution over a flat array scanned backwards.

use crate::{ErrorKind, Span, XmlError};

/// Prefix bound without a declaration.
pub const XML_PREFIX: &[u8] = b"xml";

/// Fixed binding for [`XML_PREFIX`].
pub const XML_URI: &[u8] = b"http://www.w3.org/XML/1998/namespace";

/// Reserved for declarations, not usable in names.
pub const XMLNS_PREFIX: &[u8] = b"xmlns";

/// One `xmlns` binding and its declaration depth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Binding {
    /// Empty for the default namespace.
    pub prefix: Span,
    pub uri: Span,
    pub depth: u16,
}

impl Binding {
    #[inline]
    pub fn new(prefix: Span, uri: Span, depth: u16) -> Binding {
        Binding { prefix, uri, depth }
    }
}

/// A resolved namespace. `xml` resolves to a builtin with no document span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Uri {
    /// From an `xmlns` attribute.
    Declared(Span),
    /// Bound by the Namespaces spec, not in the document.
    Builtin(&'static [u8]),
}

impl Uri {
    #[inline]
    pub fn bytes<'a>(&self, src: &'a [u8]) -> &'a [u8] {
        match *self {
            Uri::Declared(s) => s.bytes(src),
            Uri::Builtin(u) => u,
        }
    }

    /// `None` for a builtin.
    #[inline]
    pub fn span(&self) -> Option<Span> {
        match *self {
            Uri::Declared(s) => Some(s),
            Uri::Builtin(_) => None,
        }
    }
}

/// Bindings in scope, innermost last.
pub struct Names<'s> {
    bindings: &'s mut [Binding],
    len: usize,
}

impl<'s> Names<'s> {
    pub fn new(bindings: &'s mut [Binding]) -> Names<'s> {
        Names { bindings, len: 0 }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Fixed capacity, overflow is an error.
    pub fn capacity(&self) -> usize {
        self.bindings.len()
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Bindings in scope, outermost first.
    #[inline]
    pub fn bindings(&self) -> &[Binding] {
        self.bindings.get(..self.len).unwrap_or(&[])
    }

    /// Record a binding at `depth`. Unvalidated. Rebinding `xml` or `xmlns` is stored but ignored by [`Names::resolve`].
    pub fn push(&mut self, b: Binding) -> Result<(), XmlError> {
        let Some(slot) = self.bindings.get_mut(self.len) else {
            return Err(XmlError::new(ErrorKind::TooManyBindings, declared_at(&b)));
        };
        *slot = b;
        self.len += 1;
        Ok(())
    }

    /// Drop bindings deeper than `depth`. Relies on depths being non-decreasing in document order.
    pub fn pop_to(&mut self, depth: u16) {
        while deeper_than(self.bindings, self.len, depth) {
            self.len -= 1;
        }
    }

    /// Namespace for a prefix, innermost first. Empty prefix is the default. Unknown prefix is `None`, not an error.
    pub fn resolve(&self, src: &[u8], prefix: &[u8]) -> Option<Uri> {
        // Reserved prefixes first so the document cannot rebind them.
        if prefix == XML_PREFIX {
            return Some(Uri::Builtin(XML_URI));
        }
        if prefix == XMLNS_PREFIX {
            return None;
        }
        find(self.bindings(), src, prefix).map(Uri::Declared)
    }

    /// Like [`Names::resolve`] but as a span. `None` for `xml`.
    pub fn uri(&self, src: &[u8], prefix: &[u8]) -> Option<Span> {
        self.resolve(src, prefix)?.span()
    }

    /// Like [`Names::resolve`] as bytes, builtins included.
    #[inline]
    pub fn uri_bytes<'a>(&self, src: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
        Some(self.resolve(src, prefix)?.bytes(src))
    }

    /// Namespace and local part of a qualified name. Namespace is `None` for no namespace or an unknown prefix.
    pub fn resolve_name(&self, src: &[u8], name: Span) -> (Option<Uri>, Span) {
        let (prefix, local) = split(src, name);
        (self.resolve(src, prefix.bytes(src)), local)
    }

    /// Whether `name` is in namespace `uri` with local name `local`. Empty `uri` means no namespace.
    pub fn matches(&self, src: &[u8], name: Span, uri: &[u8], local: &[u8]) -> bool {
        let (prefix, want) = split(src, name);
        // Local first, cheap and rejects most.
        if want.bytes(src) != local {
            return false;
        }
        match self.resolve(src, prefix.bytes(src)) {
            Some(u) => u.bytes(src) == uri,
            // Undeclared prefix matches no namespace.
            None => uri.is_empty() && prefix.is_empty(),
        }
    }
}

/// Split at the first colon into prefix and local. No colon gives an empty prefix.
/// A leading or trailing colon keeps the whole name as local so it matches nothing.
pub fn split(src: &[u8], name: Span) -> (Span, Span) {
    let bytes = name.bytes(src);
    let at = name.at as usize;
    let Some(i) = bytes.iter().position(|b| *b == b':') else {
        return (empty_at(name.at), name);
    };
    if i == 0 || i + 1 == bytes.len() {
        return (empty_at(name.at), name);
    }
    (
        Span::of(src, at, at + i),
        Span::of(src, at + i + 1, at + bytes.len()),
    )
}

/// Prefix declared by an `xmlns` attribute, `None` for ordinary attributes. Plain `xmlns` yields an empty prefix.
pub fn declares(src: &[u8], name: Span) -> Option<Span> {
    let (prefix, local) = split(src, name);
    if prefix.bytes(src) == XMLNS_PREFIX {
        return Some(local);
    }
    if prefix.is_empty() && local.bytes(src) == XMLNS_PREFIX {
        return Some(empty_at(name.at));
    }
    None
}

/// Innermost binding for `prefix`.
fn find(live: &[Binding], src: &[u8], prefix: &[u8]) -> Option<Span> {
    for b in live.iter().rev() {
        if b.prefix.bytes(src) == prefix {
            return declared(b.uri);
        }
    }
    None
}

/// `xmlns=""` undeclares. Resolves to nothing but still shadows outer bindings.
#[inline]
fn declared(uri: Span) -> Option<Span> {
    if uri.is_empty() {
        return None;
    }
    Some(uri)
}

/// Whether the last live binding is deeper than `depth`.
#[inline]
fn deeper_than(bindings: &[Binding], len: usize, depth: u16) -> bool {
    let Some(last) = len.checked_sub(1).and_then(|i| bindings.get(i)) else {
        return false;
    };
    last.depth > depth
}

/// Error offset for an overflowing declaration.
#[inline]
fn declared_at(b: &Binding) -> usize {
    if b.prefix.is_empty() {
        return b.uri.at as usize;
    }
    b.prefix.at as usize
}

/// Empty span keeping a position for error offsets.
#[inline]
fn empty_at(at: u32) -> Span {
    Span { at, len: 0 }
}

#[cfg(test)]
mod tests;
