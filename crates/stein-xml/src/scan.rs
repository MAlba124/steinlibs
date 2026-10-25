//! Tokenizer from bytes to events in document order. Checks element nesting.

use crate::{Attr, ErrorKind, Span, XmlError, is_name, is_name_start, is_space};

/// Parser event. Spans index the document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Attributes in [`Reader::attrs`]. `<a/>` yields `Open` then `Close`.
    Open(Span),
    Close(Span),
    /// Raw text, references unresolved.
    Text(Span),
    /// `CDATA` contents.
    Cdata(Span),
    /// Comment body without delimiters.
    Comment(Span),
    /// Without `<?` and `?>`.
    Pi(Span),
    /// Without `<?xml` and `?>`.
    Decl(Span),
    /// Without `<!DOCTYPE` and `>`.
    Doctype(Span),
    Eof,
}

pub struct Reader<'a> {
    src: &'a [u8],
    at: usize,
    /// Open element names, outermost first.
    stack: &'a mut [Span],
    depth: usize,
    attrs: &'a mut [Attr],
    attr_len: usize,
    /// A self closing element owes a `Close` before anything else is read.
    pending_close: Option<Span>,
    /// Root element closed, only trailing misc remains.
    done: bool,
}

impl<'a> Reader<'a> {
    /// `stack` caps nesting depth and `attrs` caps attributes per element. Neither grows.
    pub fn new(src: &'a [u8], stack: &'a mut [Span], attrs: &'a mut [Attr]) -> Reader<'a> {
        Reader {
            src,
            at: 0,
            stack,
            depth: 0,
            attrs,
            attr_len: 0,
            pending_close: None,
            done: false,
        }
    }

    pub fn offset(&self) -> usize {
        self.at
    }

    /// Open element count, including a self closing element until its `Close`.
    pub fn depth(&self) -> usize {
        self.depth + usize::from(self.pending_close.is_some())
    }

    /// Attributes of the element from the last [`Event::Open`].
    pub fn attrs(&self) -> &[Attr] {
        self.attrs.get(..self.attr_len).unwrap_or(&[])
    }

    /// Innermost open element, a self closing one included until its `Close`.
    pub fn parent(&self) -> Option<Span> {
        if let Some(name) = self.pending_close {
            return Some(name);
        }
        self.stack.get(self.depth.checked_sub(1)?).copied()
    }

    pub fn src(&self) -> &'a [u8] {
        self.src
    }

    /// The next event, or [`Event::Eof`]. Not an `Iterator` since attrs borrow the reader.
    pub fn read(&mut self) -> Result<Event, XmlError> {
        if let Some(name) = self.pending_close.take() {
            return Ok(Event::Close(name));
        }
        if self.at >= self.src.len() {
            return self.finish();
        }
        if self.src[self.at] == b'<' {
            return self.markup();
        }
        Ok(self.text())
    }

    /// Unclosed elements at end are [`ErrorKind::Truncated`], so a cut-off feed is not mistaken for valid.
    fn finish(&mut self) -> Result<Event, XmlError> {
        if self.depth > 0 {
            return Err(XmlError::new(ErrorKind::Truncated, self.at));
        }
        self.done = true;
        Ok(Event::Eof)
    }

    /// Text up to the next `<`. Never empty.
    fn text(&mut self) -> Event {
        let from = self.at;
        let rest = self.src.get(from..).unwrap_or(&[]);
        let n = rest.iter().position(|b| *b == b'<').unwrap_or(rest.len());
        self.at = from + n;
        Event::Text(Span::of(self.src, from, self.at))
    }

    fn markup(&mut self) -> Result<Event, XmlError> {
        match self.peek(1) {
            Some(b'/') => self.close_tag(),
            Some(b'?') => self.pi(),
            Some(b'!') => self.bang(),
            Some(b) if is_name_start(b) => self.open_tag(),
            _ => Err(XmlError::new(ErrorKind::BadTag, self.at)),
        }
    }

    #[inline]
    fn peek(&self, ahead: usize) -> Option<u8> {
        self.src.get(self.at + ahead).copied()
    }
}

impl<'a> Reader<'a> {
    fn open_tag(&mut self) -> Result<Event, XmlError> {
        self.at += 1;
        let name = self.name()?;
        self.attr_len = 0;
        let closed = self.attributes()?;
        if closed {
            self.pending_close = Some(name);
        } else {
            self.open(name)?;
        }
        Ok(Event::Open(name))
    }

    fn close_tag(&mut self) -> Result<Event, XmlError> {
        self.at += 2;
        let name = self.name()?;
        self.skip_space();
        if self.peek(0) != Some(b'>') {
            return Err(XmlError::new(ErrorKind::BadTag, self.at));
        }
        self.at += 1;
        self.close(name)?;
        Ok(Event::Close(name))
    }

    fn open(&mut self, name: Span) -> Result<(), XmlError> {
        let Some(slot) = self.stack.get_mut(self.depth) else {
            return Err(XmlError::new(ErrorKind::TooDeep, self.at));
        };
        *slot = name;
        self.depth += 1;
        Ok(())
    }

    fn close(&mut self, name: Span) -> Result<(), XmlError> {
        let Some(open) = self.depth.checked_sub(1).and_then(|d| self.stack.get(d)) else {
            return Err(XmlError::new(ErrorKind::Unbalanced, self.at));
        };
        if open.bytes(self.src) != name.bytes(self.src) {
            return Err(XmlError::new(ErrorKind::Mismatched, self.at));
        }
        self.depth -= 1;
        Ok(())
    }

    /// Name at the cursor.
    fn name(&mut self) -> Result<Span, XmlError> {
        let from = self.at;
        match self.peek(0) {
            Some(b) if is_name_start(b) => {}
            _ => return Err(XmlError::new(ErrorKind::BadName, from)),
        }
        while self.peek(0).is_some_and(is_name) {
            self.at += 1;
        }
        Ok(Span::of(self.src, from, self.at))
    }

    fn skip_space(&mut self) {
        while self.peek(0).is_some_and(is_space) {
            self.at += 1;
        }
    }

    /// Attributes to the end of the tag. Returns whether it self closed.
    fn attributes(&mut self) -> Result<bool, XmlError> {
        loop {
            self.skip_space();
            match tag_end(self)? {
                Some(closed) => return Ok(closed),
                None => self.attribute()?,
            }
        }
    }

    fn attribute(&mut self) -> Result<(), XmlError> {
        let name = self.name()?;
        self.skip_space();
        if self.peek(0) != Some(b'=') {
            return Err(XmlError::new(ErrorKind::BadAttr, self.at));
        }
        self.at += 1;
        self.skip_space();
        let value = self.quoted()?;
        self.add_attr(Attr { name, value })
    }

    fn quoted(&mut self) -> Result<Span, XmlError> {
        let quote = match self.peek(0) {
            Some(q @ (b'"' | b'\'')) => q,
            _ => return Err(XmlError::new(ErrorKind::BadAttr, self.at)),
        };
        self.at += 1;
        let from = self.at;
        let rest = self.src.get(from..).unwrap_or(&[]);
        let Some(n) = rest.iter().position(|b| *b == quote) else {
            return Err(XmlError::new(ErrorKind::Truncated, from));
        };
        self.at = from + n + 1;
        Ok(Span::of(self.src, from, from + n))
    }

    /// Refuses duplicate names.
    fn add_attr(&mut self, a: Attr) -> Result<(), XmlError> {
        let seen = self
            .attrs()
            .iter()
            .any(|p| p.name.bytes(self.src) == a.name.bytes(self.src));
        if seen {
            return Err(XmlError::new(ErrorKind::DuplicateAttr, a.name.at as usize));
        }
        let Some(slot) = self.attrs.get_mut(self.attr_len) else {
            return Err(XmlError::new(ErrorKind::TooManyAttrs, self.at));
        };
        *slot = a;
        self.attr_len += 1;
        Ok(())
    }
}

impl<'a> Reader<'a> {
    fn pi(&mut self) -> Result<Event, XmlError> {
        let from = self.at + 2;
        let end =
            find(self.src, from, b"?>").ok_or_else(|| XmlError::new(ErrorKind::Truncated, from))?;
        self.at = end + 2;
        let body = Span::of(self.src, from, end);
        if starts_with_xml(body.bytes(self.src)) {
            return Ok(Event::Decl(body));
        }
        Ok(Event::Pi(body))
    }

    fn bang(&mut self) -> Result<Event, XmlError> {
        let rest = self.src.get(self.at..).unwrap_or(&[]);
        if rest.starts_with(b"<!--") {
            return self.comment();
        }
        if rest.starts_with(b"<![CDATA[") {
            return self.cdata();
        }
        if rest.len() >= 9 && rest[..9].eq_ignore_ascii_case(b"<!DOCTYPE") {
            return self.doctype();
        }
        Err(XmlError::new(ErrorKind::BadTag, self.at))
    }

    fn comment(&mut self) -> Result<Event, XmlError> {
        let from = self.at + 4;
        let end = find(self.src, from, b"-->")
            .ok_or_else(|| XmlError::new(ErrorKind::Truncated, from))?;
        self.at = end + 3;
        Ok(Event::Comment(Span::of(self.src, from, end)))
    }

    fn cdata(&mut self) -> Result<Event, XmlError> {
        let from = self.at + 9;
        let end = find(self.src, from, b"]]>")
            .ok_or_else(|| XmlError::new(ErrorKind::Truncated, from))?;
        self.at = end + 3;
        Ok(Event::Cdata(Span::of(self.src, from, end)))
    }

    /// Doctype is skipped. An internal subset is refused to prevent entity expansion attacks.
    fn doctype(&mut self) -> Result<Event, XmlError> {
        let from = self.at + 9;
        let rest = self.src.get(from..).unwrap_or(&[]);
        let end = rest
            .iter()
            .position(|b| *b == b'>' || *b == b'[')
            .ok_or_else(|| XmlError::new(ErrorKind::Truncated, from))?;
        if rest[end] == b'[' {
            return Err(XmlError::new(ErrorKind::Unsupported, from + end));
        }
        self.at = from + end + 1;
        Ok(Event::Doctype(Span::of(self.src, from, from + end)))
    }
}

/// Whether the tag ends at the cursor, and if so whether it self closed. Free fn for the nesting lint.
fn tag_end(r: &mut Reader<'_>) -> Result<Option<bool>, XmlError> {
    match r.peek(0) {
        Some(b'>') => {
            r.at += 1;
            Ok(Some(false))
        }
        Some(b'/') if r.peek(1) == Some(b'>') => {
            r.at += 2;
            Ok(Some(true))
        }
        Some(b) if is_name_start(b) => Ok(None),
        Some(_) => Err(XmlError::new(ErrorKind::BadAttr, r.at)),
        None => Err(XmlError::new(ErrorKind::Truncated, r.at)),
    }
}

/// Offset of `needle` at or after `from`.
fn find(src: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let hay = src.get(from..)?;
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let first = *needle.first()?;
    let last = hay.len() - needle.len();
    (0..=last)
        .filter(|i| hay.get(*i) == Some(&first))
        .find(|i| hay.get(*i..*i + needle.len()) == Some(needle))
        .map(|i| from + i)
}

/// Whether a PI target is `xml`, making it the declaration.
fn starts_with_xml(body: &[u8]) -> bool {
    let Some(head) = body.get(..3) else {
        return false;
    };
    if !head.eq_ignore_ascii_case(b"xml") {
        return false;
    }
    body.get(3).is_none_or(|b| is_space(*b))
}

#[cfg(test)]
mod tests;
