#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Grammar, errors, smuggling cases and a seeded random sweep.

use super::*;
use crate::h1::chunk;

/// Step bound per piece before a hang is declared.
const STEPS: usize = 100_000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> usize {
        (self.next() % n) as usize
    }
}

/// Decoded body and end offset, if reached.
#[derive(Debug, PartialEq, Eq)]
struct Fed {
    body: Vec<u8>,
    end: Option<usize>,
}

/// Feed `input` cut at `cuts`, the rest in one piece. Stops at body end.
fn feed_with(dec: &mut Decoder, input: &[u8], cuts: &[usize]) -> Result<Fed, Error> {
    let mut body = Vec::new();
    let mut pos = 0;
    let mut cuts = cuts.iter().copied();
    loop {
        let left = input.len() - pos;
        let piece = cuts.next().unwrap_or(left).clamp(1, left.max(1)).min(left);
        let src = &input[pos..pos + piece];
        let mut at = 0;
        let mut dry = false;
        for _ in 0..STEPS {
            let s = dec.step(&src[at..])?;
            assert!(s.consumed <= src.len() - at);
            match s.event {
                Event::Data { at: from, len } => {
                    assert!(len > 0, "an empty run of data");
                    assert_eq!(from + len, s.consumed);
                    body.extend_from_slice(&src[at + from..at + from + len]);
                }
                Event::End => {
                    assert!(dec.is_done());
                    assert_eq!(dec.body(), body.len() as u64);
                    return Ok(Fed {
                        body,
                        end: Some(pos + at + s.consumed),
                    });
                }
                Event::NeedMore => {
                    assert_eq!(at + s.consumed, src.len(), "NeedMore takes it all");
                    dry = true;
                    break;
                }
            }
            at += s.consumed;
        }
        assert!(dry, "a piece never ran dry");
        pos += piece;
        if pos == input.len() {
            return Ok(Fed { body, end: None });
        }
    }
}

fn feed(input: &[u8]) -> Result<Fed, Error> {
    feed_with(&mut Decoder::new(Limits::default()), input, &[])
}

/// Must decode whole and byte at a time.
fn ok(input: &[u8]) -> Vec<u8> {
    let whole = feed(input).unwrap();
    assert_eq!(whole.end, Some(input.len()), "{:?}", show(input));
    let ones = vec![1; input.len()];
    let bytes = feed_with(&mut Decoder::new(Limits::default()), input, &ones).unwrap();
    assert_eq!(bytes, whole, "a byte at a time differs");
    whole.body
}

/// Must fail the same way however it is cut.
fn err(input: &[u8]) -> Error {
    let whole = feed(input).expect_err(&show(input));
    let ones = vec![1; input.len()];
    let bytes = feed_with(&mut Decoder::new(Limits::default()), input, &ones);
    assert_eq!(bytes, Err(whole), "a byte at a time differs");
    whole
}

fn show(input: &[u8]) -> String {
    String::from_utf8_lossy(input).escape_debug().to_string()
}

// grammar

#[test]
fn chunks_and_the_last_one() {
    assert_eq!(ok(b"0\r\n\r\n"), b"");
    assert_eq!(ok(b"5\r\nhello\r\n0\r\n\r\n"), b"hello");
    assert_eq!(ok(b"3\r\nabc\r\n2\r\nde\r\n1\r\nf\r\n0\r\n\r\n"), b"abcdef");
    // data may contain line ends
    assert_eq!(ok(b"6\r\n\r\n0\r\n\r\r\n0\r\n\r\n"), b"\r\n0\r\n\r");
}

#[test]
fn sizes_are_hex_of_either_case_with_leading_zeros() {
    let data = [b'x'; 0xab];
    for size in ["ab", "AB", "aB", "0ab", "00000000000000ab"] {
        let mut wire = format!("{size}\r\n").into_bytes();
        wire.extend_from_slice(&data);
        wire.extend_from_slice(b"\r\n0\r\n\r\n");
        assert_eq!(ok(&wire), data, "{size}");
    }
    // the last chunk is one or more zeros
    assert_eq!(ok(b"1\r\na\r\n000\r\n\r\n"), b"a");
    assert_eq!(ok(b"0000000000000000\r\n\r\n"), b"");
}

#[test]
fn extensions_are_parsed_and_dropped() {
    let cases: [&[u8]; 12] = [
        b"1;a\r\nx\r\n0\r\n\r\n",
        b"1;a=b\r\nx\r\n0\r\n\r\n",
        b"1;a=\"b c\"\r\nx\r\n0\r\n\r\n",
        b"1;a=\"\"\r\nx\r\n0\r\n\r\n",
        b"1;a=\"q\\\"q\\\\\"\r\nx\r\n0\r\n\r\n",
        b"1;a=\"\xc3\xa9;=\"\r\nx\r\n0\r\n\r\n",
        b"1;a;b=c;d=\"e\";f\r\nx\r\n0\r\n\r\n",
        // BWS around ; and =
        b"1 ; a = b ; c\r\nx\r\n0\r\n\r\n",
        b"1\t;\ta\t=\t\"b\"\t;c\r\nx\r\n0\r\n\r\n",
        b"1;a=\"b\" ;c\r\nx\r\n0\r\n\r\n",
        // on the last chunk too
        b"1\r\nx\r\n0;last=yes\r\n\r\n",
        b"1;!#$%&'*+-.^_`|~=09azAZ\r\nx\r\n0\r\n\r\n",
    ];
    for wire in cases {
        assert_eq!(ok(wire), b"x", "{}", show(wire));
    }
}

#[test]
fn trailers_are_checked_and_dropped() {
    let cases: [&[u8]; 6] = [
        b"1\r\nx\r\n0\r\nA: b\r\n\r\n",
        b"1\r\nx\r\n0\r\nA:b\r\nC-D:  e f \r\n\r\n",
        b"1\r\nx\r\n0\r\nEmpty:\r\n\r\n",
        b"1\r\nx\r\n0\r\nTab:\tv\t\r\n\r\n",
        b"1\r\nx\r\n0\r\nObs: \xff\x80\r\n\r\n",
        b"1\r\nx\r\n0;e\r\nDigest: sha-256=:abc=:\r\nX: y\r\nZ: 1\r\n\r\n",
    ];
    for wire in cases {
        assert_eq!(ok(wire), b"x", "{}", show(wire));
    }
}

#[test]
fn what_follows_the_body_is_not_touched() {
    let mut wire = b"4\r\nbody\r\n0\r\nT: v\r\n\r\n".to_vec();
    let end = wire.len();
    wire.extend_from_slice(b"GET /next HTTP/1.1\r\n\r\n");
    for cut in 0..=wire.len() {
        let fed = feed_with(&mut Decoder::new(Limits::default()), &wire, &[cut.max(1)]).unwrap();
        assert_eq!(fed.body, b"body");
        assert_eq!(fed.end, Some(end), "cut {cut}");
    }
}

#[test]
fn a_finished_decoder_stays_finished() {
    let mut dec = Decoder::new(Limits::default());
    let s = dec.step(b"0\r\n\r\nGET").unwrap();
    assert_eq!((s.consumed, s.event), (5, Event::End));
    for src in [&b"GET"[..], b"", b"5\r\nhello"] {
        let s = dec.step(src).unwrap();
        assert_eq!((s.consumed, s.event), (0, Event::End));
    }
    assert!(dec.is_done());
}

#[test]
fn a_failed_decoder_stays_failed() {
    let mut dec = Decoder::new(Limits::default());
    assert_eq!(dec.step(b"5\nhello"), Err(Error::BadLineEnd));
    assert_eq!(dec.step(b"0\r\n\r\n"), Err(Error::BadLineEnd));
    assert_eq!(dec.step(b""), Err(Error::BadLineEnd));
    assert!(!dec.is_done());
}

#[test]
fn one_event_a_step_and_the_count_that_goes_with_it() {
    let mut dec = Decoder::new(Limits::default());
    let src = b"3;x\r\nabc\r\n2\r\nde\r\n0\r\n\r\nrest";
    let s = dec.step(src).unwrap();
    assert_eq!((s.consumed, s.event), (8, Event::Data { at: 5, len: 3 }));
    let src = &src[8..];
    let s = dec.step(src).unwrap();
    assert_eq!((s.consumed, s.event), (7, Event::Data { at: 5, len: 2 }));
    let src = &src[7..];
    let s = dec.step(src).unwrap();
    assert_eq!((s.consumed, s.event), (7, Event::End));
    assert_eq!(&src[7..], b"rest");
    assert_eq!(dec.body(), 5);
    // a chunk cut in the middle is two runs
    let mut dec = Decoder::new(Limits::default());
    let s = dec.step(b"5\r\nhe").unwrap();
    assert_eq!((s.consumed, s.event), (5, Event::Data { at: 3, len: 2 }));
    assert_eq!(dec.body(), 2);
    let s = dec.step(b"").unwrap();
    assert_eq!((s.consumed, s.event), (0, Event::NeedMore));
    let s = dec.step(b"llo\r").unwrap();
    assert_eq!((s.consumed, s.event), (3, Event::Data { at: 0, len: 3 }));
    let s = dec.step(b"\r").unwrap();
    assert_eq!((s.consumed, s.event), (1, Event::NeedMore));
}

// errors

#[test]
fn every_error_has_a_cause() {
    let cases: [(&[u8], Error); 22] = [
        (b"\r\n", Error::BadSize),
        (b"g\r\n", Error::BadSize),
        (b"1g\r\n", Error::BadSize),
        (b"11111111111111111\r\n", Error::SizeOverflow),
        (b"00000000000000001\r\n", Error::SizeOverflow),
        (b"1\n", Error::BadLineEnd),
        (b"1\r\r\n", Error::BadLineEnd),
        (b"1\rx", Error::BadLineEnd),
        (b"1;a\r\r", Error::BadLineEnd),
        (b"1;\r\n", Error::BadExtension),
        (b"1;a=\r\n", Error::BadExtension),
        (b"1;a\n", Error::BadExtension),
        (b"1;a=\"b\nc\"\r\n", Error::BadExtension),
        (b"1;a=\"b\\\n\"\r\n", Error::BadExtension),
        (b"1;a=\"b\"c\r\n", Error::BadExtension),
        (b"1\r\nab\r\n", Error::MissingCrlf),
        (b"1\r\na\n", Error::MissingCrlf),
        (b"1\r\na\r\r", Error::MissingCrlf),
        (b"0\r\n: v\r\n\r\n", Error::BadTrailer),
        (b"0\r\nname\r\n\r\n", Error::BadTrailer),
        (b"0\r\nA: b\n\r\n", Error::BadTrailer),
        (b"0\r\nA: b\r\r\n", Error::BadLineEnd),
    ];
    for (wire, want) in cases {
        assert_eq!(err(wire), want, "{}", show(wire));
    }
    assert_eq!(err(b"0\r\n\rX"), Error::BadLineEnd);
}

#[test]
fn errors_name_a_status() {
    assert_eq!(Error::BodyTooLarge.status(), 413);
    assert_eq!(Error::TrailerTooLarge.status(), 431);
    for e in [
        Error::BadSize,
        Error::SizeOverflow,
        Error::BadExtension,
        Error::BadLineEnd,
        Error::MissingCrlf,
        Error::BadTrailer,
        Error::ExtensionTooLarge,
    ] {
        assert_eq!(e.status(), 400, "{e:?}");
    }
}

fn limited(limits: Limits, input: &[u8]) -> Result<Fed, Error> {
    let whole = feed_with(&mut Decoder::new(limits), input, &[]);
    let ones = vec![1; input.len()];
    let bytes = feed_with(&mut Decoder::new(limits), input, &ones);
    assert_eq!(bytes, whole, "a byte at a time differs");
    whole
}

#[test]
fn the_body_limit_is_judged_on_the_size_line() {
    let limits = Limits {
        max_body: 8,
        ..Limits::default()
    };
    let fits = b"5\r\nhello\r\n3\r\nabc\r\n0\r\n\r\n";
    assert_eq!(limited(limits, fits).unwrap().body, b"helloabc");
    let over = b"5\r\nhello\r\n4\r\nabcd\r\n0\r\n\r\n";
    assert_eq!(limited(limits, over), Err(Error::BodyTooLarge));
    // refused before a byte of the chunk is reported
    let mut dec = Decoder::new(limits);
    assert_eq!(dec.step(b"9\r\n"), Err(Error::BodyTooLarge));
    assert_eq!(dec.body(), 0);
    // u64 overflow is too large, not a wrap
    let mut dec = Decoder::new(Limits::default());
    let s = dec.step(b"1\r\na\r\nffffffffffffffff\r\n").unwrap();
    assert_eq!(s.event, Event::Data { at: 3, len: 1 });
    assert_eq!(
        dec.step(b"\r\nffffffffffffffff\r\n"),
        Err(Error::BodyTooLarge)
    );
    // max size starts a chunk without a limit
    let mut dec = Decoder::new(Limits::default());
    let s = dec.step(b"ffffffffffffffff\r\nab").unwrap();
    assert_eq!(s.event, Event::Data { at: 18, len: 2 });
}

#[test]
fn extension_bytes_are_bounded_over_the_whole_body() {
    let limits = Limits {
        max_ext: 6,
        ..Limits::default()
    };
    // three bytes per chunk, CR not counted
    let fits = b"1;ab\r\nx\r\n1;cd\r\ny\r\n0\r\n\r\n";
    assert_eq!(limited(limits, fits).unwrap().body, b"xy");
    let over = b"1;ab\r\nx\r\n1;cd\r\ny\r\n0;e\r\n\r\n";
    assert_eq!(limited(limits, over), Err(Error::ExtensionTooLarge));
    let spaces = b"1      ;a\r\nx\r\n0\r\n\r\n";
    assert_eq!(limited(limits, spaces), Err(Error::ExtensionTooLarge));
    let none = Limits {
        max_ext: 0,
        ..Limits::default()
    };
    assert_eq!(limited(none, b"1\r\nx\r\n0\r\n\r\n").unwrap().body, b"x");
    assert_eq!(
        limited(none, b"1;a\r\nx\r\n0\r\n\r\n"),
        Err(Error::ExtensionTooLarge)
    );
}

#[test]
fn trailer_bytes_are_bounded() {
    let limits = Limits {
        max_trailer: 12,
        ..Limits::default()
    };
    // two lines of six bytes, line ends included
    let fits = b"0\r\nA: b\r\nC: d\r\n\r\n";
    assert_eq!(limited(limits, fits).unwrap().end, Some(fits.len()));
    let over = b"0\r\nA: b\r\nC: de\r\n\r\n";
    assert_eq!(limited(limits, over), Err(Error::TrailerTooLarge));
    let none = Limits {
        max_trailer: 0,
        ..Limits::default()
    };
    assert_eq!(limited(none, b"0\r\n\r\n").unwrap().end, Some(5));
    assert_eq!(
        limited(none, b"0\r\nA: b\r\n\r\n"),
        Err(Error::TrailerTooLarge)
    );
}

// smuggling

/// Lenient-parser ambiguities. Each is refused at the offending byte.
#[test]
fn malformed_bodies_from_the_smuggling_literature_are_refused() {
    let cases: [(&[u8], &str); 34] = [
        (b"5\nhello\r\n0\r\n\r\n", "bare LF after the size"),
        (b"5\r\nhello\n0\r\n\r\n", "bare LF after the data"),
        (b"5\r\nhello\r\n0\n\r\n", "bare LF after the last chunk"),
        (b"5\r\nhello\r\n0\r\n\n", "bare LF as the final line"),
        (b"5\r\nhello\r\n0\n\n", "bare LF twice"),
        (b"5\rhello\r\n0\r\n\r\n", "bare CR after the size"),
        (b"5\r\nhello\r0\r\n\r\n", "bare CR after the data"),
        (b"0x5\r\nhello\r\n0\r\n\r\n", "0x prefix"),
        (b"0X5\r\nhello\r\n0\r\n\r\n", "0X prefix"),
        (b"+5\r\nhello\r\n0\r\n\r\n", "plus sign"),
        (b"-5\r\nhello\r\n0\r\n\r\n", "minus sign"),
        (b"-0\r\n\r\n", "minus zero"),
        (b" 5\r\nhello\r\n0\r\n\r\n", "space before the size"),
        (b"\t5\r\nhello\r\n0\r\n\r\n", "tab before the size"),
        (b"5 \r\nhello\r\n0\r\n\r\n", "space after the size"),
        (b"5\t\r\nhello\r\n0\r\n\r\n", "tab after the size"),
        (b"5 5\r\nhello\r\n0\r\n\r\n", "space inside the size"),
        (b"\r\n5\r\nhello\r\n0\r\n\r\n", "empty line before the size"),
        (b"5_0\r\nhello\r\n0\r\n\r\n", "digit separator"),
        (b"5h\r\nhello\r\n0\r\n\r\n", "hex suffix"),
        (b"5.0\r\nhello\r\n0\r\n\r\n", "a fraction"),
        (b"5,\r\nhello\r\n0\r\n\r\n", "trailing junk"),
        (b"5abcxyz\r\nhello\r\n0\r\n\r\n", "junk after valid digits"),
        (b"\xef\xbc\x95\r\nhello\r\n0\r\n\r\n", "a fullwidth digit"),
        (b"5\x00\r\nhello\r\n0\r\n\r\n", "NUL after the size"),
        (
            b"10000000000000005\r\nhello\r\n0\r\n\r\n",
            "a size that wraps to 5",
        ),
        (b"ffffffffffffffffff\r\n", "oversized hex"),
        (b"5\r\nhello0\r\n\r\n", "no CRLF after the data"),
        (b"5\r\nhelloX\r\n0\r\n\r\n", "data longer than its size"),
        (
            b"5\r\nhello\r\n0\r\nGET / HTTP/1.1\r\n\r\n",
            "a request where a trailer goes",
        ),
        (b"5\r\nhello\r\n0\r\n A: b\r\n\r\n", "a folded trailer line"),
        (
            b"5\r\nhello\r\n0\r\nA : b\r\n\r\n",
            "space before a trailer's colon",
        ),
        (
            b"5;a=\"\r\n\"\r\nhello\r\n0\r\n\r\n",
            "a line end inside a quoted extension",
        ),
        (b"5;\nhello\r\n0\r\n\r\n", "LF ending an extension"),
    ];
    for (wire, what) in cases {
        let whole = feed(wire);
        assert!(whole.is_err(), "{what} was accepted: {whole:?}");
        let ones = vec![1; wire.len()];
        let bytes = feed_with(&mut Decoder::new(Limits::default()), wire, &ones);
        assert_eq!(bytes, whole, "{what} differs a byte at a time");
    }
}

#[test]
fn a_body_missing_its_end_is_not_complete() {
    let wire = b"5;e=\"v\"\r\nhello\r\n0\r\nT: v\r\n\r\n";
    for cut in 0..wire.len() {
        let fed = feed(&wire[..cut]).unwrap();
        assert_eq!(fed.end, None, "complete at {cut}");
    }
    assert_eq!(feed(wire).unwrap().end, Some(wire.len()));
}

// sweep

const EXTENSIONS: [&str; 6] = [
    "",
    ";a",
    ";name=value",
    ";q=\"a b\\\"c\"",
    " ; x = y ;z",
    ";a;b;c=\"\"",
];
const TRAILERS: [&str; 4] = ["", "A: b\r\n", "Digest: xyz\r\nX-Empty:\r\n", "T:\t é \r\n"];

/// Random `(wire, body)`.
fn random_body(rng: &mut Rng) -> (Vec<u8>, Vec<u8>) {
    let mut wire = Vec::new();
    let mut body = Vec::new();
    for _ in 0..rng.below(12) {
        let len = match rng.below(4) {
            0 => 1 + rng.below(4),
            1 => 1 + rng.below(64),
            2 => 1 + rng.below(700),
            _ => 1 + rng.below(5000),
        };
        let data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let size = match rng.below(4) {
            0 => format!("{len:X}"),
            1 => format!("{len:08x}"),
            _ => format!("{len:x}"),
        };
        wire.extend_from_slice(size.as_bytes());
        wire.extend_from_slice(EXTENSIONS[rng.below(6)].as_bytes());
        wire.extend_from_slice(b"\r\n");
        wire.extend_from_slice(&data);
        wire.extend_from_slice(b"\r\n");
        body.extend_from_slice(&data);
    }
    wire.extend_from_slice(["0", "00", "0;end"][rng.below(3)].as_bytes());
    wire.extend_from_slice(b"\r\n");
    wire.extend_from_slice(TRAILERS[rng.below(4)].as_bytes());
    wire.extend_from_slice(b"\r\n");
    (wire, body)
}

fn random_cuts(rng: &mut Rng, len: usize) -> Vec<usize> {
    match rng.below(5) {
        0 => vec![1; len],
        1 => Vec::new(),
        2 => (0..len).map(|_| 1 + rng.below(3)).collect(),
        3 => (0..len).map(|_| 1 + rng.below(40)).collect(),
        _ => (0..len).map(|_| 1 + rng.below(3000)).collect(),
    }
}

/// Random bodies cut at random places decode whole and end at the message end.
#[test]
fn random_bodies_survive_any_cut() {
    let limits = Limits {
        max_ext: 4096,
        ..Limits::default()
    };
    let mut bytes = 0usize;
    for seed in 0..2000u64 {
        let mut rng = Rng(seed ^ 0xc0de);
        let (mut wire, body) = random_body(&mut rng);
        let end = wire.len();
        let next: &[u8] = [b"", &b"POST /next HTTP/1.1\r\n"[..], b"0\r\n\r\n"][rng.below(3)];
        wire.extend_from_slice(next);
        let cuts = random_cuts(&mut rng, wire.len());
        let fed = feed_with(&mut Decoder::new(limits), &wire, &cuts).unwrap();
        assert!(fed.body == body, "seed {seed}: body differs");
        assert_eq!(fed.end, Some(end), "seed {seed}");
        bytes += end;
    }
    assert!(bytes > 4_000_000, "the sweep shrank to {bytes} bytes");
}

/// Round trip with the `chunk` encoder, both framing paths.
#[test]
fn what_the_encoder_writes_is_read_back() {
    let mut rng = Rng(7);
    for _ in 0..200 {
        let mut wire = Vec::new();
        let mut body = Vec::new();
        for _ in 0..rng.below(8) {
            let n = 1 + rng.below(3000);
            let data: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
            body.extend_from_slice(&data);
            if rng.below(2) == 0 {
                let mut out = vec![0u8; chunk::framed_len(n as u64) as usize];
                chunk::encode(&mut out, &data).unwrap();
                wire.extend_from_slice(&out);
            } else {
                let gap = chunk::gap(65_536);
                let mut buf = vec![0u8; gap + n + chunk::TAIL];
                buf[gap..gap + n].copy_from_slice(&data);
                chunk::frame(&mut buf, gap, n, false).unwrap();
                wire.extend_from_slice(&buf);
            }
        }
        wire.extend_from_slice(chunk::LAST);
        let cuts = random_cuts(&mut rng, wire.len());
        let fed = feed_with(&mut Decoder::new(Limits::default()), &wire, &cuts).unwrap();
        assert!(fed.body == body);
        assert_eq!(fed.end, Some(wire.len()));
    }
}

// fuzz

/// Any input must not panic, keep offsets in bounds and limits, and terminate.
fn run_any(input: &[u8], cuts: &[usize], what: &str) {
    let limits = Limits {
        max_body: 4096,
        max_ext: 32,
        max_trailer: 64,
    };
    let mut dec = Decoder::new(limits);
    match feed_with(&mut dec, input, cuts) {
        Ok(fed) => {
            assert!(fed.body.len() as u64 <= limits.max_body, "{what}");
            assert_eq!(fed.end.is_some(), dec.is_done(), "{what}");
        }
        Err(e) => {
            assert_eq!(dec.step(b"0\r\n\r\n"), Err(e), "{what}");
            assert!([400, 413, 431].contains(&e.status()), "{what}");
        }
    }
    assert!(dec.body() <= limits.max_body, "{what}");
}

#[test]
fn random_bytes_never_panic() {
    let alphabet = b"0123456789abcdefABCDEFxg;= \t\"\\\r\n:-";
    for seed in 0..3000u64 {
        let mut rng = Rng(seed ^ 0xfade);
        let len = rng.below(200);
        let input: Vec<u8> = match seed % 2 {
            0 => (0..len).map(|_| rng.next() as u8).collect(),
            // grammar bytes, which get further
            _ => (0..len)
                .map(|_| alphabet[rng.below(alphabet.len() as u64)])
                .collect(),
        };
        let cuts = random_cuts(&mut rng, input.len());
        run_any(&input, &cuts, &format!("seed {seed}"));
    }
}

#[test]
fn a_mutated_body_never_panics() {
    for seed in 0..3000u64 {
        let mut rng = Rng(seed ^ 0xbeef);
        let (mut wire, _) = random_body(&mut rng);
        wire.truncate(1500);
        for _ in 0..1 + rng.below(3) {
            let at = rng.below(wire.len() as u64);
            match rng.below(4) {
                0 => wire[at] ^= 1 << rng.below(8),
                1 => wire[at] = rng.next() as u8,
                2 => {
                    wire.remove(at);
                }
                _ => wire.insert(at, b"\r\n;0 "[rng.below(5)]),
            }
            if wire.is_empty() {
                break;
            }
        }
        let cuts = random_cuts(&mut rng, wire.len());
        run_any(&wire, &cuts, &format!("seed {seed}"));
    }
}
