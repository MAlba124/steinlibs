#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn drain_text(fd: OwnedFd) -> Option<String> {
    let mut buf = Vec::new();
    drain(fd, &mut buf);
    Some(lossy(&buf))
}

#[test]
fn the_mime_types_we_accept_are_the_ones_we_offer() {
    for m in MIMES {
        assert!(is_text(m), "{m} is offered but not accepted");
    }
}

#[test]
fn any_plain_text_flavour_is_accepted() {
    assert!(is_text("text/plain"));
    assert!(is_text("text/plain;charset=utf-8"));
    assert!(is_text("text/plain;charset=US-ASCII"));
    assert!(is_text("UTF8_STRING"));
    // Case, because X11 clients are inconsistent about it.
    assert!(is_text("utf8_string"));
}

#[test]
fn a_non_text_flavour_is_refused() {
    assert!(!is_text("image/png"));
    assert!(!is_text("text/html"));
    assert!(!is_text(""));
}

/// The write side has to loop over partial writes, or a paste larger than a
/// pipe buffer arrives truncated. A pipe buffer is 64 KiB by default, so this
/// is comfortably past it.
#[test]
fn a_payload_larger_than_a_pipe_buffer_is_written_whole() {
    let text: String = std::iter::repeat_n("abcdefghij", 40_000).collect();
    let (read, write) = rustix::pipe::pipe().expect("a pipe");
    let want = text.clone();
    let writer = std::thread::spawn(move || write_offer(&text, write));

    let mut got = Vec::new();
    let mut file = std::fs::File::from(read);
    std::io::Read::read_to_end(&mut file, &mut got).expect("a read");
    writer.join().expect("the writer");
    assert_eq!(got.len(), want.len());
    assert_eq!(String::from_utf8_lossy(&got), want);
}

#[test]
fn a_receiver_that_stops_reading_does_not_hang_the_writer() {
    let text: String = std::iter::repeat_n("x", 4 << 20).collect();
    let (read, write) = rustix::pipe::pipe().expect("a pipe");
    let writer = std::thread::spawn(move || write_offer(&text, write));
    // Take a little and close, which is what a receiver wanting a preview
    // does. The writer must return rather than block forever.
    let mut file = std::fs::File::from(read);
    let mut head = [0u8; 16];
    let _ = std::io::Read::read(&mut file, &mut head);
    drop(file);
    let done = std::time::Instant::now();
    writer.join().expect("the writer");
    assert!(done.elapsed() < Duration::from_secs(5), "the writer hung");
}

/// A source that never writes must not take the window with it.
#[test]
fn a_silent_source_times_out_rather_than_blocking_forever() {
    let (read, write) = rustix::pipe::pipe().expect("a pipe");
    // Held open and never written to, which is what a hung application looks
    // like from here.
    let start = Instant::now();
    let got = drain_text(read);
    let elapsed = start.elapsed();
    drop(write);
    assert_eq!(got.as_deref(), Some(""));
    assert!(elapsed >= PASTE_TIMEOUT, "it did not wait at all");
    assert!(
        elapsed < PASTE_TIMEOUT * 4,
        "it waited {elapsed:?}, far past the timeout"
    );
}

#[test]
fn a_source_that_closes_immediately_yields_nothing() {
    let (read, write) = rustix::pipe::pipe().expect("a pipe");
    drop(write);
    assert_eq!(drain_text(read).as_deref(), Some(""));
}

#[test]
fn what_is_written_is_what_is_read_back() {
    let (read, write) = rustix::pipe::pipe().expect("a pipe");
    let writer = std::thread::spawn(move || write_offer("café naïve 日本語\nsecond line", write));
    let got = drain_text(read).expect("a paste");
    writer.join().expect("the writer");
    assert_eq!(got, "café naïve 日本語\nsecond line");
}

/// A source claiming UTF-8 and sending something else should still paste what
/// it got right, rather than pasting nothing.
#[test]
fn invalid_utf8_pastes_lossily_rather_than_not_at_all() {
    let (read, write) = rustix::pipe::pipe().expect("a pipe");
    let writer = std::thread::spawn(move || {
        use std::io::Write as _;
        let mut f = std::fs::File::from(write);
        let _ = f.write_all(b"good \xff\xfe bad");
    });
    let got = drain_text(read).expect("a paste");
    writer.join().expect("the writer");
    assert!(got.starts_with("good "), "got {got:?}");
    assert!(got.ends_with(" bad"), "got {got:?}");
}

#[test]
fn an_empty_clipboard_has_no_selection() {
    let c = Clipboard::default();
    assert!(!c.has_selection());
}

#[test]
fn a_uri_list_becomes_local_paths() {
    let list = "# dragged from a file manager\r\n\
                file:///home/u/perf.data\r\n\
                file://localhost/tmp/a%20b.zst\r\n\
                https://example.com/x\r\n\
                file://otherhost/etc/passwd\r\n";
    let got = uris_to_paths(list);
    assert_eq!(
        got,
        vec![
            std::path::PathBuf::from("/home/u/perf.data"),
            std::path::PathBuf::from("/tmp/a b.zst"),
        ]
    );
}

#[test]
fn a_broken_escape_is_skipped_not_mangled() {
    assert!(uris_to_paths("file:///bad%zzname").is_empty());
    assert!(uris_to_paths("file:///cut%4").is_empty());
}

#[test]
fn file_lists_offer_their_own_flavours_and_text() {
    assert_eq!(
        flavour_of(Clip::Files, GNOME_FILES),
        Some(Flavour::GnomeFiles)
    );
    assert_eq!(flavour_of(Clip::Files, URI_LIST), Some(Flavour::UriList));
    assert_eq!(flavour_of(Clip::Files, "text/plain"), Some(Flavour::Text));
    assert_eq!(flavour_of(Clip::Text, URI_LIST), None);
    assert_eq!(offers(Clip::Files).first(), Some(&GNOME_FILES));
    for m in offers(Clip::Files) {
        assert!(
            flavour_of(Clip::Files, m).is_some(),
            "{m} offered but not served"
        );
    }
}
