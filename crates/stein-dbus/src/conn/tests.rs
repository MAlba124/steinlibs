use super::*;
use crate::bus;
use crate::msg::parse;
use std::vec::Vec;

const GUID: &[u8] = b"OK 0123456789abcdef0123456789abcdef\r\n";

/// Everything queued, drained the way a driver would.
fn drain(c: &mut Conn<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 7];
    loop {
        let n = c.transmit(&mut chunk);
        if n == 0 {
            return out;
        }
        out.extend_from_slice(&chunk[..n]);
    }
}

/// A message the bus would send, built with the same writer.
fn from_bus(f: Fields<'_>, body: impl FnOnce(&mut Writer<'_>) -> Result<(), WireError>) -> Vec<u8> {
    let mut buf = [0u8; 512];
    let n = msg::write(&mut buf, &f, body).unwrap();
    buf[..n].to_vec()
}

fn hello_return(serial: u32) -> Vec<u8> {
    let f = Fields {
        reply_serial: serial,
        destination: Some(":1.5"),
        sig: "s",
        ..Fields::new(Kind::Return, 1)
    };
    from_bus(f, |w| w.str(":1.5"))
}

/// Feed everything, draining messages as they complete.
fn feed(c: &mut Conn<'_>, mut data: &[u8], seen: &mut Vec<(Kind, u32)>) {
    loop {
        let n = c.received(data);
        data = &data[n..];
        while let Some(m) = c.poll().unwrap() {
            seen.push((m.kind(), m.serial()));
        }
        if data.is_empty() {
            return;
        }
    }
}

/// Auth, then Hello answered, leaving a ready connection.
fn ready<'a>(rx: &'a mut [u8], tx: &'a mut [u8]) -> Conn<'a> {
    let mut c = Conn::new(Storage { rx, tx }, 1000).unwrap();
    let first = drain(&mut c);
    assert_eq!(first, b"\0AUTH EXTERNAL 31303030\r\n");
    let mut seen = Vec::new();
    feed(&mut c, GUID, &mut seen);
    assert_eq!(c.state(), State::Hello);
    let out = drain(&mut c);
    assert!(out.starts_with(b"BEGIN\r\n"));
    let hello = parse(&out[7..]).unwrap().unwrap();
    assert!(hello.is(bus::INTERFACE, "Hello"));
    assert_eq!(hello.destination(), Some(bus::NAME));
    feed(&mut c, &hello_return(hello.serial()), &mut seen);
    assert!(seen.is_empty(), "the Hello reply is the connection's own");
    c
}

#[test]
fn auth_and_hello_reach_ready() {
    let (mut rx, mut tx) = ([0u8; 256], [0u8; 256]);
    let c = ready(&mut rx, &mut tx);
    assert!(c.is_ready());
    assert_eq!(c.unique_name(), Some(":1.5"));
    assert_eq!(c.guid(), b"0123456789abcdef0123456789abcdef");
}

#[test]
fn calls_wait_for_auth() {
    let (mut rx, mut tx) = ([0u8; 256], [0u8; 256]);
    let mut c = Conn::new(
        Storage {
            rx: &mut rx,
            tx: &mut tx,
        },
        0,
    )
    .unwrap();
    assert_eq!(bus::list_names(c.out()), Err(DbusError::NotReady));
}

#[test]
fn a_rejected_auth_closes() {
    let (mut rx, mut tx) = ([0u8; 256], [0u8; 256]);
    let mut c = Conn::new(
        Storage {
            rx: &mut rx,
            tx: &mut tx,
        },
        0,
    )
    .unwrap();
    c.received(b"REJECTED EXTERNAL\r\n");
    assert_eq!(c.poll().err(), Some(DbusError::Auth(AuthError::Rejected)));
    assert_eq!(c.state(), State::Closed);
    assert_eq!(c.poll().err(), Some(DbusError::Closed));
}

#[test]
fn a_call_goes_out_and_its_reply_comes_back() {
    let (mut rx, mut tx) = ([0u8; 256], [0u8; 256]);
    let mut c = ready(&mut rx, &mut tx);
    let serial = bus::get_name_owner(c.out(), "org.example.X").unwrap();
    let out = drain(&mut c);
    let m = parse(&out).unwrap().unwrap();
    assert_eq!(m.serial(), serial);
    assert!(m.is(bus::INTERFACE, "GetNameOwner"));
    assert_eq!(m.body().str(), Ok("org.example.X"));

    let f = Fields {
        reply_serial: serial,
        sig: "s",
        ..Fields::new(Kind::Return, 9)
    };
    c.received(&from_bus(f, |w| w.str(":1.9")));
    let m = c.poll().unwrap().unwrap();
    assert_eq!(m.reply_serial(), Some(serial));
    assert_eq!(m.body().str(), Ok(":1.9"));
    assert!(c.poll().unwrap().is_none());
}

#[test]
fn messages_arrive_in_pieces_and_in_bursts() {
    let (mut rx, mut tx) = ([0u8; 256], [0u8; 256]);
    let mut c = ready(&mut rx, &mut tx);
    let sig = |serial| {
        let f = Fields {
            path: Some("/a"),
            interface: Some("org.x.Y"),
            member: Some("Z"),
            sig: "u",
            ..Fields::new(Kind::Signal, serial)
        };
        from_bus(f, |w| w.u32(serial))
    };
    let mut stream = Vec::new();
    for s in 1..=20 {
        stream.extend(sig(s));
    }
    // A byte at a time, then in bursts bigger than the buffer, which forces
    // the receive buffer to compact around messages still unread.
    let mut seen = Vec::new();
    for b in &stream {
        feed(&mut c, core::slice::from_ref(b), &mut seen);
    }
    feed(&mut c, &stream, &mut seen);
    let serials: Vec<u32> = seen.iter().map(|&(_, s)| s).collect();
    let want: Vec<u32> = (1..=20).chain(1..=20).collect();
    assert_eq!(serials, want);
}

#[test]
fn a_message_bigger_than_the_buffer_is_named() {
    let (mut rx, mut tx) = ([0u8; 128], [0u8; 256]);
    let mut c = ready(&mut rx, &mut tx);
    let f = Fields {
        path: Some("/a"),
        interface: Some("org.x.Y"),
        member: Some("Z"),
        sig: "s",
        ..Fields::new(Kind::Signal, 3)
    };
    let big = from_bus(f, |w| w.str(&"x".repeat(200)));
    c.received(&big);
    assert_eq!(c.poll().err(), Some(DbusError::RxTooSmall(big.len())));
}

#[test]
fn a_full_send_buffer_takes_no_serial() {
    let (mut rx, mut tx) = ([0u8; 256], [0u8; 160]);
    let mut c = ready(&mut rx, &mut tx);
    let long = "x".repeat(150);
    assert_eq!(bus::add_match(c.out(), &long), Err(DbusError::TxFull));
    let s = bus::list_names(c.out()).unwrap();
    let out = drain(&mut c);
    assert_eq!(parse(&out).unwrap().unwrap().serial(), s);
    assert_eq!(s, 2, "the failed call must not have used a serial");
}

#[test]
fn a_call_is_answered_and_errors_too() {
    let (mut rx, mut tx) = ([0u8; 512], [0u8; 512]);
    let mut c = ready(&mut rx, &mut tx);
    let f = Fields {
        path: Some("/obj"),
        interface: Some("org.x.Y"),
        member: Some("Ping"),
        destination: Some(":1.5"),
        sig: "",
        ..Fields::new(Kind::Call, 40)
    };
    c.received(&from_bus(f, |_| Ok(())));
    let to = {
        let m = c.poll().unwrap().unwrap();
        ReplyTo::of(&m)
    };
    assert_eq!(to.serial, 40);
    c.reply(&to, "s", |w| w.str("pong")).unwrap();
    c.error(&to, "org.x.Error.Nope", "no").unwrap();
    let out = drain(&mut c);
    let r = parse(&out).unwrap().unwrap();
    assert_eq!(r.kind(), Kind::Return);
    assert_eq!(r.reply_serial(), Some(40));
    let e = parse(&out[r.head.len as usize..]).unwrap().unwrap();
    assert_eq!(e.kind(), Kind::Error);
    assert_eq!(e.error_name(), Some("org.x.Error.Nope"));
    assert_eq!(e.body().str(), Ok("no"));
}

#[test]
fn no_reply_calls_get_none() {
    let (mut rx, mut tx) = ([0u8; 256], [0u8; 256]);
    let mut c = ready(&mut rx, &mut tx);
    let to = ReplyTo {
        serial: 3,
        sender: Name::new(":1.2").unwrap(),
        no_reply: true,
    };
    assert_eq!(c.reply(&to, "", |_| Ok(())), Ok(None));
    assert_eq!(c.pending(), 0);
}

#[test]
fn a_refused_hello_closes() {
    let (mut rx, mut tx) = ([0u8; 256], [0u8; 256]);
    let mut c = Conn::new(
        Storage {
            rx: &mut rx,
            tx: &mut tx,
        },
        0,
    )
    .unwrap();
    c.received(GUID);
    assert!(c.poll().unwrap().is_none());
    let f = Fields {
        reply_serial: 1,
        error: Some("org.freedesktop.DBus.Error.Failed"),
        ..Fields::new(Kind::Error, 1)
    };
    c.received(&from_bus(f, |_| Ok(())));
    assert_eq!(c.poll().err(), Some(DbusError::Hello));
}

#[test]
fn a_restart_authenticates_again_from_scratch() {
    let (mut rx, mut tx) = ([0u8; 256], [0u8; 256]);
    let mut c = ready(&mut rx, &mut tx);
    bus::list_names(c.out()).unwrap();
    c.received(&hello_return(99)[..10]);
    c.restart(1000).unwrap();
    assert_eq!(c.state(), State::Auth);
    assert_eq!(c.unique_name(), None);
    assert_eq!(
        drain(&mut c),
        b"\0AUTH EXTERNAL 31303030\r\n",
        "the queued call and half a message are gone"
    );
    let mut seen = Vec::new();
    feed(&mut c, GUID, &mut seen);
    let out = drain(&mut c);
    let hello = parse(&out[7..]).unwrap().unwrap();
    assert_eq!(hello.serial(), 1, "serials start over");
    feed(&mut c, &hello_return(hello.serial()), &mut seen);
    assert!(c.is_ready() && seen.is_empty());
}
