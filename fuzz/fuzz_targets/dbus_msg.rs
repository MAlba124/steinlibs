//! stein-dbus messages off the wire: framed and parsed without a panic, a parsed message's
//! fields lie inside its bytes and its body walks or fails cleanly. A body that walks is
//! written again, little endian, with the same header fields, and parses and walks back to
//! the same values.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stein_dbus::msg::{self, Fields};
use stein_dbus::{Item, Message, WireError, Writer};

fuzz_target!(|bytes: &[u8]| {
    let framed = msg::frame_len(bytes);
    let Ok(Some(m)) = msg::parse(bytes) else {
        return;
    };
    let len = m.bytes.len();
    assert_eq!(framed, Ok(Some(len)), "frame_len names the parsed length");
    assert!(len <= bytes.len() && len == m.head.len as usize);
    assert!(m.head.serial != 0, "serial zero is refused");
    fields(&m);
    let Some(items) = walked(&m) else {
        return;
    };
    let mut out = vec![0u8; len + 1024];
    let written = rewrite(&m, &items, &mut out);
    assert!(
        written.is_ok(),
        "a parsed message did not write again: {written:?}"
    );
    let Ok(n) = written else {
        return;
    };
    let again = msg::parse(&out[..n]);
    assert!(
        matches!(again, Ok(Some(_))),
        "a written message did not parse: {again:?}"
    );
    let Ok(Some(b)) = again else {
        return;
    };
    assert_eq!(
        (b.kind(), b.serial(), b.reply_serial()),
        (m.kind(), m.serial(), m.reply_serial())
    );
    assert_eq!(
        (b.path(), b.interface(), b.member()),
        (m.path(), m.interface(), m.member())
    );
    assert_eq!(
        (b.error_name(), b.destination(), b.signature()),
        (m.error_name(), m.destination(), m.signature())
    );
    let back = walked(&b);
    assert!(back.is_some(), "a written body did not walk");
    let Some(back) = back else {
        return;
    };
    assert_eq!(back.len(), items.len(), "values");
    assert!(
        back.iter().zip(&items).all(|(x, y)| same(x, y)),
        "values differ"
    );
});

/// Every accessor, none may panic, the strings come out of the message's bytes.
fn fields(m: &Message<'_>) {
    for s in [
        m.path(),
        m.interface(),
        m.member(),
        m.error_name(),
        m.destination(),
        m.sender(),
    ]
    .into_iter()
    .flatten()
    {
        assert!(within(m.bytes, s.as_bytes()), "a field outside the message");
    }
    assert!(within(m.bytes, m.signature().as_bytes()));
    assert!(within(m.bytes, m.body_bytes()));
}

fn within(all: &[u8], part: &[u8]) -> bool {
    let (a, p) = (all.as_ptr() as usize, part.as_ptr() as usize);
    part.is_empty() || (p >= a && p + part.len() <= a + all.len())
}

fn walked<'a>(m: &Message<'a>) -> Option<Vec<Item<'a>>> {
    let mut v = Vec::new();
    m.walk(&mut |i| v.push(i)).ok()?;
    Some(v)
}

/// NaN is equal to itself here, as its bits.
fn same(a: &Item<'_>, b: &Item<'_>) -> bool {
    match (a, b) {
        (Item::F64(x), Item::F64(y)) => x.to_bits() == y.to_bits(),
        _ => a == b,
    }
}

fn rewrite(m: &Message<'_>, items: &[Item<'_>], out: &mut [u8]) -> Result<usize, WireError> {
    let f = Fields {
        kind: m.kind(),
        flags: m.head.flags,
        serial: m.serial(),
        reply_serial: m.reply_serial().unwrap_or(0),
        path: m.path(),
        interface: m.interface(),
        member: m.member(),
        error: m.error_name(),
        destination: m.destination(),
        sig: m.signature(),
    };
    msg::write(out, &f, |w| encode(items, w))
}

fn encode(items: &[Item<'_>], w: &mut Writer<'_>) -> Result<(), WireError> {
    let mut arrays = Vec::new();
    for it in items {
        match *it {
            Item::Byte(v) => w.u8(v)?,
            Item::Bool(v) => w.bool(v)?,
            Item::I16(v) => w.i16(v)?,
            Item::U16(v) => w.u16(v)?,
            Item::I32(v) => w.i32(v)?,
            Item::U32(v) => w.u32(v)?,
            Item::I64(v) => w.i64(v)?,
            Item::U64(v) => w.u64(v)?,
            Item::F64(v) => w.f64(v)?,
            Item::Str(s) => w.str(s)?,
            Item::Path(p) => w.path(p)?,
            Item::Sig(s) => w.sig(s)?,
            Item::Fd(i) => w.fd(i)?,
            Item::Array(elem) => {
                let Some(&code) = elem.first() else {
                    return Err(WireError::Truncated);
                };
                arrays.push(w.begin_array(code)?);
            }
            Item::ArrayEnd => {
                if let Some(a) = arrays.pop() {
                    w.end_array(a)?;
                }
            }
            Item::Struct | Item::Dict => w.begin_struct()?,
            Item::Variant(s) => w.variant(s)?,
            Item::StructEnd | Item::DictEnd | Item::VariantEnd => {}
        }
    }
    Ok(())
}
