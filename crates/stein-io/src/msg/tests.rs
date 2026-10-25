#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn buffer(control: Option<Control<'_>>, payload: &[u8], flags: u32) -> Vec<u8> {
    let mut b = vec![0u8; HEAD + payload.len()];
    let used = match control {
        Some(c) => encode(&mut b[OUT_LEN..HEAD], c).unwrap(),
        None => 0,
    };
    b[4..8].copy_from_slice(&(used as u32).to_ne_bytes());
    b[8..12].copy_from_slice(&(payload.len() as u32).to_ne_bytes());
    b[12..16].copy_from_slice(&flags.to_ne_bytes());
    b[HEAD..].copy_from_slice(payload);
    b
}

#[test]
fn a_message_with_control_reads_back() {
    let c = Control {
        level: 282,
        kind: 2,
        data: &[22],
    };
    let b = buffer(Some(c), b"hello", 0x80);
    let m = parse(&b).unwrap();
    assert_eq!(m.payload, b"hello");
    assert_eq!(m.control, Some(c));
    assert_eq!(m.flags, 0x80);
    assert_eq!(payload_len(&b), Some(5));
}

#[test]
fn a_message_without_control_has_none() {
    let b = buffer(None, b"abc", 0);
    let m = parse(&b).unwrap();
    assert_eq!(m.payload, b"abc");
    assert_eq!(m.control, None);
}

#[test]
fn the_layout_is_the_kernels() {
    assert_eq!(CMSG_HDR, size_of::<libc::cmsghdr>());
    assert_eq!(HEAD, 80);
    let mut room = [0xffu8; CONTROL_ROOM];
    let c = Control {
        level: 1,
        kind: 2,
        data: &[9, 8, 7],
    };
    assert_eq!(encode(&mut room, c), Some(CMSG_HDR + 8));
    assert_eq!(&room[..8], &(CMSG_HDR + 3).to_ne_bytes());
    assert_eq!(&room[CMSG_HDR..CMSG_HDR + 8], &[9, 8, 7, 0, 0, 0, 0, 0]);
}

#[test]
fn data_that_does_not_fit_is_refused() {
    let mut room = [0u8; CONTROL_ROOM];
    let big = [0u8; CMSG_MAX + 1];
    let c = Control {
        level: 1,
        kind: 1,
        data: &big,
    };
    assert_eq!(encode(&mut room, c), None);
    let fits = Control {
        level: 1,
        kind: 1,
        data: &big[..CMSG_MAX],
    };
    assert_eq!(encode(&mut room, fits), Some(CONTROL_ROOM));
    assert_eq!(first(&room), Some(fits));
}

#[test]
fn a_short_or_lying_buffer_is_not_trusted() {
    assert_eq!(parse(&[0u8; 10]), None);
    assert_eq!(parse(&[0u8; HEAD - 1]), None);
    // A stated payload longer than the buffer is clamped.
    let mut b = buffer(None, b"xy", 0);
    b[8..12].copy_from_slice(&1000u32.to_ne_bytes());
    assert_eq!(parse(&b).unwrap().payload, b"xy");
    // A message longer than the room, or shorter than its own header.
    let mut room = [0u8; CONTROL_ROOM];
    room[..8].copy_from_slice(&500usize.to_ne_bytes());
    assert_eq!(first(&room), None);
    room[..8].copy_from_slice(&3usize.to_ne_bytes());
    assert_eq!(first(&room), None);
}

#[test]
fn a_plain_header_reads_as_payload_only() {
    let mut b = vec![0xaau8; HEAD + 4];
    write_plain(&mut b, 4);
    let m = parse(&b).unwrap();
    assert_eq!(m.payload, &[0xaa; 4]);
    assert_eq!(m.control, None);
    assert_eq!(m.flags, 0);
}

#[test]
fn every_control_message_is_walked() {
    let mut room = [0u8; CONTROL_ROOM];
    let tos = Control {
        level: libc::IPPROTO_IP,
        kind: libc::IP_TOS,
        data: &[2],
    };
    let gro = Control {
        level: libc::SOL_UDP,
        kind: libc::UDP_GRO,
        data: &1400i32.to_ne_bytes(),
    };
    let a = encode(&mut room, tos).unwrap();
    let b = encode(&mut room[a..], gro).unwrap();
    let all: Vec<_> = controls(&room[..a + b]).collect();
    assert_eq!(all, [tos, gro]);
    // A truncated second message ends the walk after the first.
    let cut: Vec<_> = controls(&room[..a + b - 5]).collect();
    assert_eq!(cut, [tos]);
    assert_eq!(controls(&[]).count(), 0);
}

#[test]
fn a_named_buffer_reads_sender_controls_and_payload() {
    let payload = b"named";
    let mut b = vec![0u8; HEAD_NAMED + payload.len()];
    let mut name = [0u8; 16];
    name[..2].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
    name[2..4].copy_from_slice(&4433u16.to_be_bytes());
    name[4..8].copy_from_slice(&[127, 0, 0, 1]);
    b[OUT_LEN..OUT_LEN + 16].copy_from_slice(&name);
    let c = Control {
        level: libc::IPPROTO_IP,
        kind: libc::IP_TOS,
        data: &[1],
    };
    let used = encode(&mut b[OUT_LEN + NAME_ROOM..HEAD_NAMED], c).unwrap();
    b[0..4].copy_from_slice(&16u32.to_ne_bytes());
    b[4..8].copy_from_slice(&(used as u32).to_ne_bytes());
    b[8..12].copy_from_slice(&(payload.len() as u32).to_ne_bytes());
    b[HEAD_NAMED..].copy_from_slice(payload);
    let m = parse_named(&b).unwrap();
    assert_eq!(m.payload, payload);
    assert_eq!(m.from, Some("127.0.0.1:4433".parse().unwrap()));
    assert_eq!(m.control, Some(c));
    assert_eq!(m.controls.collect::<Vec<_>>(), [c]);
    assert_eq!(payload_len_at(&b, HEAD_NAMED), Some(5));
    // The plain parse has no name and reads another offset.
    assert_eq!(parse(&b).unwrap().from, None);
}
