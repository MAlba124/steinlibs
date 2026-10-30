use super::*;
use crate::conn::{Conn, Storage};
use crate::msg::{Fields, parse, write};
use crate::value::Dict;
use std::vec::Vec;

fn call(
    member: &str,
    sig: &str,
    body: impl FnOnce(&mut Writer<'_>) -> Result<(), WireError>,
) -> Vec<u8> {
    let mut buf = [0u8; 512];
    let f = Fields {
        path: Some("/obj"),
        interface: Some(INTERFACE),
        member: Some(member),
        sig,
        ..Fields::new(Kind::Call, 7)
    };
    let n = write(&mut buf, &f, body).unwrap();
    buf[..n].to_vec()
}

#[test]
fn requests_read() {
    let b = call("Get", "ss", |w| {
        w.str("org.x.Y")?;
        w.str("Title")
    });
    let m = parse(&b).unwrap().unwrap();
    assert!(matches!(
        request(&m),
        Some(Request::Get {
            iface: "org.x.Y",
            name: "Title"
        })
    ));
    let b = call("GetAll", "s", |w| w.str("org.x.Y"));
    assert!(matches!(
        request(&parse(&b).unwrap().unwrap()),
        Some(Request::GetAll { iface: "org.x.Y" })
    ));
    let b = call("Set", "ssv", |w| {
        w.str("org.x.Y")?;
        w.str("Level")?;
        w.variant("i")?;
        w.i32(3)
    });
    let m = parse(&b).unwrap().unwrap();
    match request(&m) {
        Some(Request::Set { name, value, .. }) => {
            assert_eq!(name, "Level");
            assert_eq!(value.i32(), Some(3));
        }
        other => panic!("{other:?}"),
    }
    // Wrong signature is not a request.
    let b = call("Get", "s", |w| w.str("org.x.Y"));
    assert!(request(&parse(&b).unwrap().unwrap()).is_none());
}

/// A connection past Hello, for building replies.
fn ready<'a>(rx: &'a mut [u8], tx: &'a mut [u8]) -> Conn<'a> {
    let mut c = Conn::new(Storage { rx, tx }, 0).unwrap();
    c.received(b"OK 0123456789abcdef0123456789abcdef\r\n");
    assert!(c.poll().unwrap().is_none());
    let mut out = [0u8; 256];
    let n = c.transmit(&mut out);
    let hello = parse(&out[b"\0AUTH EXTERNAL 30\r\nBEGIN\r\n".len()..n])
        .unwrap()
        .unwrap();
    let f = Fields {
        reply_serial: hello.serial(),
        sig: "s",
        ..Fields::new(Kind::Return, 1)
    };
    let mut buf = [0u8; 128];
    let len = write(&mut buf, &f, |w| w.str(":1.9")).unwrap();
    c.received(&buf[..len]);
    assert!(c.poll().unwrap().is_none());
    c
}

#[test]
fn replies_and_signals_write_the_right_shapes() {
    let (mut rx, mut tx) = ([0u8; 1024], [0u8; 1024]);
    let mut c = ready(&mut rx, &mut tx);
    let to = ReplyTo {
        serial: 7,
        sender: crate::Name::new(":1.2").unwrap(),
        no_reply: false,
    };
    reply_get(c.out(), &to, "s", |w| w.str("hi")).unwrap();
    reply_all(c.out(), &to, |w| {
        w.sv("A", "b", |w| w.bool(true))?;
        w.sv("B", "u", |w| w.u32(5))
    })
    .unwrap();
    changed(c.out(), "/obj", "org.x.Y", |w| {
        w.sv("A", "b", |w| w.bool(false))
    })
    .unwrap();
    let mut out = [0u8; 1024];
    let n = c.transmit(&mut out);
    let mut at = 0;
    let mut msgs = Vec::new();
    while at < n {
        let m = parse(&out[at..n]).unwrap().unwrap();
        at += m.head.len as usize;
        msgs.push(m);
    }
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].signature(), "v");
    assert_eq!(Value::read(&mut msgs[0].body()).unwrap().str(), Some("hi"));
    assert_eq!(msgs[1].signature(), "a{sv}");
    let d = Dict::read(&mut msgs[1].body()).unwrap();
    assert_eq!(d.get("B").and_then(|v| v.u32()), Some(5));
    assert!(msgs[2].is(INTERFACE, "PropertiesChanged"));
    assert_eq!(msgs[2].signature(), "sa{sv}as");
}
