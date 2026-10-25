//! Tokenizer, namespaces and decoding together, with invariants checked per event.
//! Parses with tiny and roomy tables. The small trace must be a prefix of the big one, equal if no cap was hit.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stein_xml::ns::{self, Binding, Names, Uri};
use stein_xml::{
    Attr, ErrorKind, Event, Reader, Span, XmlError, decode, entity, is_name, is_name_start,
};

#[derive(Clone, Copy)]
struct Caps {
    stack: usize,
    attrs: usize,
    binds: usize,
}

const SMALL: Caps = Caps {
    stack: 8,
    attrs: 4,
    binds: 4,
};

impl Caps {
    /// Never fills for `n` bytes. Depth capped at u16, the binding depth type.
    fn roomy(n: usize) -> Caps {
        Caps {
            stack: (n / 2 + 1).min(u16::MAX as usize),
            attrs: n / 4 + 1,
            binds: n / 4 + 1,
        }
    }
}

/// Observed output, compared across capacities.
#[derive(Debug, PartialEq)]
enum Step {
    Ev(Event),
    Attr(Attr),
    Name(Option<Uri>, Span),
}

struct Run {
    trace: Vec<Step>,
    end: Result<(), XmlError>,
}

/// Per run state plus a naive namespace model to check `Names` against.
struct State {
    open: Vec<Span>,
    model: Vec<Binding>,
    trace: Vec<Step>,
    last: Option<Event>,
    last_at: u32,
    tight: Vec<u8>,
    roomy: Vec<u8>,
}

fuzz_target!(|doc: &[u8]| {
    let small = run(doc, SMALL);
    let big = run(doc, Caps::roomy(doc.len()));
    compare(&small, &big);

    // Decode accepts any bytes, not only tokenizer output.
    let mut s = state();
    decoded(doc, Span::of(doc, 0, doc.len()), &mut s);
});

fn state() -> State {
    State {
        open: Vec::new(),
        model: Vec::new(),
        trace: Vec::new(),
        last: None,
        last_at: 0,
        tight: Vec::new(),
        roomy: Vec::new(),
    }
}

fn is_capacity(e: &Result<(), XmlError>) -> bool {
    matches!(
        e,
        Err(XmlError {
            kind: ErrorKind::TooDeep | ErrorKind::TooManyAttrs | ErrorKind::TooManyBindings,
            ..
        })
    )
}

fn compare(small: &Run, big: &Run) {
    if is_capacity(&big.end) {
        return;
    }
    if is_capacity(&small.end) {
        assert!(
            big.trace.starts_with(&small.trace),
            "capacity changed output"
        );
        return;
    }
    assert_eq!(small.end, big.end, "capacity changed the result");
    assert_eq!(small.trace, big.trace, "capacity changed output");
}

fn run(doc: &[u8], caps: Caps) -> Run {
    let mut stack = vec![Span::EMPTY; caps.stack];
    let mut attrs = vec![Attr::default(); caps.attrs];
    let mut binds = vec![Binding::default(); caps.binds];
    let mut names = Names::new(&mut binds);
    let mut r = Reader::new(doc, &mut stack, &mut attrs);
    let mut s = state();
    let end = loop {
        match step(doc, &mut r, &mut names, &mut s) {
            Ok(true) => {}
            Ok(false) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    if let Err(e) = end {
        assert!(e.at as usize <= doc.len(), "error past the end {e:?}");
    }
    Run {
        trace: s.trace,
        end,
    }
}

/// One event. False at end of document.
fn step(
    doc: &[u8],
    r: &mut Reader<'_>,
    names: &mut Names<'_>,
    s: &mut State,
) -> Result<bool, XmlError> {
    let before = r.offset();
    let e = r.read()?;
    let after = r.offset();
    assert!(
        before <= after && after <= doc.len(),
        "cursor {before} to {after}"
    );
    // Only the deferred close of `<a/>` may make no progress.
    if after == before && e != Event::Eof {
        let paired = matches!((s.last, e), (Some(Event::Open(o)), Event::Close(c)) if o == c);
        assert!(paired, "no progress at {before} on {e:?}");
    }
    if let Some(sp) = span(e) {
        in_bounds(doc, sp);
        assert!(sp.at >= s.last_at, "events out of order");
        s.last_at = sp.at;
    }
    s.trace.push(Step::Ev(e));
    match e {
        Event::Open(name) => open(doc, r, names, s, name)?,
        Event::Close(name) => close(doc, r, names, s, name),
        Event::Text(t) => text(doc, t, s),
        Event::Cdata(t) => without(doc, t, b"]]>"),
        Event::Comment(t) => without(doc, t, b"-->"),
        Event::Pi(t) | Event::Decl(t) => without(doc, t, b"?>"),
        Event::Doctype(t) => {
            without(doc, t, b">");
            without(doc, t, b"[");
        }
        Event::Eof => {
            eof(doc, r, names, s);
            return Ok(false);
        }
    }
    s.last = Some(e);
    Ok(true)
}

fn span(e: Event) -> Option<Span> {
    match e {
        Event::Open(s)
        | Event::Close(s)
        | Event::Text(s)
        | Event::Cdata(s)
        | Event::Comment(s)
        | Event::Pi(s)
        | Event::Decl(s)
        | Event::Doctype(s) => Some(s),
        Event::Eof => None,
    }
}

fn in_bounds(doc: &[u8], s: Span) {
    let end = s.at as usize + s.len as usize;
    assert!(end <= doc.len(), "span {s:?} past {}", doc.len());
}

/// Maximal run of name bytes starting with a name start byte.
fn check_name(doc: &[u8], s: Span) {
    in_bounds(doc, s);
    let b = s.bytes(doc);
    assert!(
        b.first().is_some_and(|c| is_name_start(*c)),
        "bad name start"
    );
    assert!(b.iter().all(|c| is_name(*c)), "bad name byte");
    let next = doc.get(s.at as usize + s.len as usize);
    assert!(!next.is_some_and(|c| is_name(*c)), "name cut short");
}

fn without(doc: &[u8], s: Span, needle: &[u8]) {
    let b = s.bytes(doc);
    assert!(
        !b.windows(needle.len()).any(|w| w == needle),
        "body holds its terminator"
    );
}

fn text(doc: &[u8], t: Span, s: &mut State) {
    assert!(!t.is_empty(), "empty text");
    assert!(!matches!(s.last, Some(Event::Text(_))), "split text run");
    without(doc, t, b"<");
    let next = doc.get(t.at as usize + t.len as usize);
    assert!(next.is_none_or(|c| *c == b'<'), "text stopped early");
    decoded(doc, t, s);
}

fn open(
    doc: &[u8],
    r: &Reader<'_>,
    names: &mut Names<'_>,
    s: &mut State,
    name: Span,
) -> Result<(), XmlError> {
    check_name(doc, name);
    s.open.push(name);
    let depth = s.open.len();
    // A self closing element counts as open until its close.
    assert_eq!(r.depth(), depth, "depth after open");
    assert_eq!(r.parent(), Some(name), "wrong parent");

    let attrs = r.attrs();
    for (i, a) in attrs.iter().enumerate() {
        attr(doc, name, a, attrs.get(..i).unwrap_or(&[]));
        decoded(doc, a.value, s);
        s.trace.push(Step::Attr(*a));
    }
    let d = depth as u16;
    for a in attrs {
        let Some(prefix) = ns::declares(doc, a.name) else {
            continue;
        };
        let b = Binding::new(prefix, a.value, d);
        names.push(b)?;
        s.model.push(b);
    }
    assert_eq!(names.bindings(), &s.model[..], "bindings diverged on open");
    resolve(doc, names, s, name);
    for a in attrs {
        resolve(doc, names, s, a.name);
    }
    Ok(())
}

fn attr(doc: &[u8], el: Span, a: &Attr, before: &[Attr]) {
    check_name(doc, a.name);
    in_bounds(doc, a.value);
    assert!(a.name.at > el.at, "attribute before its element");
    let dup = before
        .iter()
        .any(|p| p.name.bytes(doc) == a.name.bytes(doc));
    assert!(!dup, "duplicate attribute passed");
    let at = a.value.at as usize;
    let q = at.checked_sub(1).and_then(|i| doc.get(i)).copied();
    assert!(matches!(q, Some(b'"' | b'\'')), "value not quoted");
    assert_eq!(
        doc.get(at + a.value.len as usize).copied(),
        q,
        "quote mismatch"
    );
    assert!(
        !a.value.bytes(doc).iter().any(|c| Some(*c) == q),
        "quote inside value"
    );
}

fn close(doc: &[u8], r: &Reader<'_>, names: &mut Names<'_>, s: &mut State, name: Span) {
    check_name(doc, name);
    let open = s.open.pop();
    assert!(
        open.is_some_and(|o| o.bytes(doc) == name.bytes(doc)),
        "close does not match open"
    );
    assert_eq!(r.depth(), s.open.len(), "depth after close");
    let d = s.open.len() as u16;
    names.pop_to(d);
    s.model.retain(|b| b.depth <= d);
    assert_eq!(names.bindings(), &s.model[..], "bindings diverged on close");
}

fn eof(doc: &[u8], r: &mut Reader<'_>, names: &Names<'_>, s: &State) {
    assert!(s.open.is_empty(), "eof with elements open");
    assert_eq!(r.depth(), 0);
    assert_eq!(r.offset(), doc.len(), "eof before the end");
    assert!(names.is_empty(), "bindings outlived the root");
    assert_eq!(r.read(), Ok(Event::Eof), "eof is not sticky");
}

/// Model resolution, innermost first, reserved prefixes ahead of the table.
fn model_resolve(doc: &[u8], model: &[Binding], prefix: &[u8]) -> Option<Uri> {
    if prefix == ns::XML_PREFIX {
        return Some(Uri::Builtin(ns::XML_URI));
    }
    if prefix == ns::XMLNS_PREFIX {
        return None;
    }
    let b = model.iter().rev().find(|b| b.prefix.bytes(doc) == prefix)?;
    (!b.uri.is_empty()).then_some(Uri::Declared(b.uri))
}

fn resolve(doc: &[u8], names: &Names<'_>, s: &mut State, name: Span) {
    let (prefix, local) = ns::split(doc, name);
    in_bounds(doc, prefix);
    in_bounds(doc, local);
    let name_end = name.at + name.len;
    if prefix.is_empty() {
        assert_eq!(local, name, "unprefixed name split");
    } else {
        assert_eq!(prefix.at, name.at);
        assert_eq!(doc.get((prefix.at + prefix.len) as usize), Some(&b':'));
        assert_eq!(local.at, prefix.at + prefix.len + 1);
        assert_eq!(local.at + local.len, name_end);
        assert!(!local.is_empty(), "empty local part");
    }

    let (uri, l) = names.resolve_name(doc, name);
    assert_eq!(l, local);
    assert_eq!(
        uri,
        model_resolve(doc, &s.model, prefix.bytes(doc)),
        "resolve diverged"
    );
    if let Some(Uri::Declared(u)) = uri {
        in_bounds(doc, u);
    }
    let want = uri.map_or(&[][..], |u| u.bytes(doc));
    let hit = names.matches(doc, name, want, local.bytes(doc));
    assert_eq!(
        hit,
        uri.is_some() || prefix.is_empty(),
        "matches disagrees with resolve"
    );
    s.trace.push(Step::Name(uri, local));
}

/// An input-sized buffer always suffices, so buffer size must not change the result.
fn decoded(doc: &[u8], sp: Span, s: &mut State) {
    let raw = sp.bytes(doc);
    s.tight.clear();
    s.tight.resize(raw.len(), 0);
    let tight = decode(raw, &mut s.tight);
    if !entity::needs_decode(raw) {
        assert_eq!(tight, Ok(raw.len()));
        assert_eq!(&s.tight[..], raw, "plain text changed");
        return;
    }
    s.roomy.clear();
    s.roomy.resize(raw.len() * 4 + 8, 0);
    let roomy = decode(raw, &mut s.roomy);
    assert_eq!(tight, roomy, "buffer size changed the result");
    match tight {
        Ok(n) => {
            assert!(n <= raw.len(), "decode grew the text");
            assert_eq!(s.tight.get(..n), s.roomy.get(..n));
            if core::str::from_utf8(raw).is_ok() {
                let out = s.tight.get(..n).unwrap_or(&[]);
                assert!(core::str::from_utf8(out).is_ok(), "decode broke utf8");
            }
        }
        Err(e) => assert!(e.at as usize <= raw.len(), "decode error past the end"),
    }
}
