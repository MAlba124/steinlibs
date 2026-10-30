use super::*;

fn hello(out: &mut [u8]) -> usize {
    let f = Fields {
        path: Some("/org/freedesktop/DBus"),
        interface: Some("org.freedesktop.DBus"),
        member: Some("Hello"),
        destination: Some("org.freedesktop.DBus"),
        ..Fields::new(Kind::Call, 1)
    };
    write(out, &f, |_| Ok(())).unwrap()
}

#[test]
fn a_call_round_trips() {
    let mut buf = [0u8; 256];
    let n = hello(&mut buf);
    assert_eq!(n % 8, 0, "fields pad to 8 and the body is empty");
    let m = parse(&buf[..n]).unwrap().unwrap();
    assert_eq!(m.kind(), Kind::Call);
    assert_eq!(m.serial(), 1);
    assert_eq!(m.path(), Some("/org/freedesktop/DBus"));
    assert!(m.is("org.freedesktop.DBus", "Hello"));
    assert_eq!(m.destination(), Some("org.freedesktop.DBus"));
    assert_eq!(m.signature(), "");
    assert_eq!(m.body_bytes(), b"");
    assert_eq!(m.head.len as usize, n);
}

#[test]
fn the_fixed_header_is_laid_out_as_the_spec_says() {
    let mut buf = [0u8; 256];
    let n = hello(&mut buf);
    assert_eq!(&buf[..4], &[b'l', 1, 0, 1]);
    assert_eq!(&buf[4..8], &0u32.to_le_bytes(), "empty body");
    assert_eq!(&buf[8..12], &1u32.to_le_bytes(), "serial");
    let fields = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]) as usize;
    assert_eq!((16 + fields).next_multiple_of(8), n);
    // The first field is the path: code 1, variant "o".
    assert_eq!(&buf[16..20], &[PATH, 1, b'o', 0]);
}

#[test]
fn a_return_with_a_body_round_trips() {
    let mut buf = [0u8; 256];
    let f = Fields {
        reply_serial: 9,
        destination: Some(":1.7"),
        sig: "su",
        ..Fields::new(Kind::Return, 3)
    };
    let n = write(&mut buf, &f, |w| {
        w.str("hi")?;
        w.u32(5)
    })
    .unwrap();
    let m = parse(&buf[..n]).unwrap().unwrap();
    assert_eq!(m.reply_serial(), Some(9));
    assert_eq!(m.signature(), "su");
    let mut b = m.body();
    assert_eq!(b.str(), Ok("hi"));
    assert_eq!(b.u32(), Ok(5));
    assert!(b.is_empty());
}

#[test]
fn a_body_that_does_not_match_its_signature_is_refused() {
    let mut buf = [0u8; 256];
    let f = Fields {
        reply_serial: 1,
        sig: "u",
        ..Fields::new(Kind::Return, 2)
    };
    assert_eq!(
        write(&mut buf, &f, |w| w.str("oops")),
        Err(WireError::Trailing)
    );
    assert_eq!(write(&mut buf, &f, |_| Ok(())), Err(WireError::Truncated));
}

#[test]
fn bad_names_never_reach_the_wire() {
    let mut buf = [0u8; 256];
    let mut f = Fields {
        path: Some("/x"),
        member: Some("Go"),
        ..Fields::new(Kind::Call, 1)
    };
    f.interface = Some("nodots");
    assert_eq!(write(&mut buf, &f, |_| Ok(())), Err(WireError::Name));
    f.interface = None;
    f.path = Some("x");
    assert_eq!(write(&mut buf, &f, |_| Ok(())), Err(WireError::Path));
    f.path = Some("/x");
    f.serial = 0;
    assert_eq!(write(&mut buf, &f, |_| Ok(())), Err(WireError::Serial));
}

#[test]
fn required_fields_are_enforced() {
    let mut buf = [0u8; 256];
    // A signal without an interface.
    let f = Fields {
        path: Some("/x"),
        member: Some("Changed"),
        ..Fields::new(Kind::Signal, 1)
    };
    let n = write(&mut buf, &f, |_| Ok(())).unwrap();
    assert_eq!(parse(&buf[..n]).err(), Some(WireError::Missing(INTERFACE)));
    // A return without a reply serial.
    let n = write(&mut buf, &Fields::new(Kind::Return, 1), |_| Ok(())).unwrap();
    assert_eq!(
        parse(&buf[..n]).err(),
        Some(WireError::Missing(REPLY_SERIAL))
    );
}

#[test]
fn partial_input_waits() {
    let mut buf = [0u8; 256];
    let n = hello(&mut buf);
    assert_eq!(frame_len(&buf[..15]), Ok(None));
    assert_eq!(frame_len(&buf[..16]), Ok(Some(n)));
    assert!(parse(&buf[..n - 1]).unwrap().is_none());
    assert!(parse(&buf[..n]).unwrap().is_some());
}

#[test]
fn bad_names_coming_in_are_refused() {
    let mut buf = [0u8; 256];
    let n = hello(&mut buf);
    // "org.freedesktop.DBus" is the interface and the destination, the member is "Hello"
    let at = |s: &[u8]| buf[..n].windows(s.len()).position(|w| w == s).unwrap();
    let mut bad = buf;
    let i = at(b"Hello");
    bad[i] = b'1';
    assert_eq!(
        parse(&bad[..n]).err(),
        Some(WireError::Name),
        "a member starting with a digit"
    );
    let mut bad = buf;
    let i = at(b"org.freedesktop.DBus\0\0\0\0\x03");
    bad[i + 3] = b'\n';
    assert_eq!(
        parse(&bad[..n]).err(),
        Some(WireError::Name),
        "an interface with a newline"
    );
    assert!(
        parse(&buf[..n]).unwrap().is_some(),
        "the good one still parses"
    );
}

#[test]
fn garbage_is_refused() {
    let mut buf = [0u8; 256];
    let n = hello(&mut buf);
    let mut bad = buf;
    bad[0] = b'x';
    assert_eq!(frame_len(&bad[..n]), Err(WireError::Endian(b'x')));
    let mut bad = buf;
    bad[3] = 2;
    assert_eq!(parse(&bad[..n]).err(), Some(WireError::Version(2)));
    let mut bad = buf;
    bad[1] = 9;
    assert_eq!(parse(&bad[..n]).err(), Some(WireError::Kind(9)));
    // A body length that pushes the message past the cap.
    let mut bad = buf;
    bad[4..8].copy_from_slice(&(1u32 << 28).to_le_bytes());
    assert!(matches!(frame_len(&bad[..n]), Err(WireError::TooLarge(_))));
}

/// A big endian return by hand: reply serial 3, signature "u", body 42.
#[test]
fn big_endian_messages_parse() {
    let mut m = [0u8; 36];
    m[..4].copy_from_slice(&[b'B', 2, 0, 1]);
    m[4..8].copy_from_slice(&4u32.to_be_bytes());
    m[8..12].copy_from_slice(&7u32.to_be_bytes());
    m[12..16].copy_from_slice(&15u32.to_be_bytes());
    m[16..20].copy_from_slice(&[REPLY_SERIAL, 1, b'u', 0]);
    m[20..24].copy_from_slice(&3u32.to_be_bytes());
    m[24..31].copy_from_slice(&[SIGNATURE, 1, b'g', 0, 1, b'u', 0]);
    m[32..36].copy_from_slice(&42u32.to_be_bytes());
    let msg = parse(&m).unwrap().unwrap();
    assert!(msg.head.big);
    assert_eq!(msg.serial(), 7);
    assert_eq!(msg.reply_serial(), Some(3));
    assert_eq!(msg.body().u32(), Ok(42));
}

#[test]
fn unknown_header_fields_are_skipped() {
    let mut buf = [0u8; 256];
    let n = hello(&mut buf);
    // Rename the member field's code to an unknown one: still a string,
    // so it is stepped over, and then the member is missing.
    let at = buf[..n]
        .windows(4)
        .position(|w| w == [MEMBER, 1, b's', 0])
        .unwrap();
    buf[at] = 200;
    assert_eq!(parse(&buf[..n]).err(), Some(WireError::Missing(MEMBER)));
}
