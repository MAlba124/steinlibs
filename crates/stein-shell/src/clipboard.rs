//! The Wayland selection, which is what everything else calls the clipboard.
//!
//! Two halves that look similar and are not. Copying means offering a
//! `wl_data_source` and answering the compositor's later request to write the
//! bytes into a pipe it hands us. Pasting means taking the `wl_data_offer` the
//! compositor last announced, handing it a pipe of our own, and reading what
//! comes back.
//!
//! The read is synchronous, inside the frame that asked for it. A paste is a
//! deliberate user action at human speed, and the alternative is a state
//! machine spanning frames to save a few milliseconds nobody notices.
//!
//! Drag and drop rides the same device. A new offer is held as fresh until
//! the compositor says what it is: the selection, or a drag entering the
//! window. A drag carrying a file list is accepted and, on drop, read the
//! same way a paste is.

use std::io::Read as _;
use std::os::fd::{AsFd, OwnedFd};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::io::Errno;

use wayland_client::Connection;

use crate::scene::{Clip, Flavour};
use wayland_client::protocol::{
    wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
};

/// What we offer and what we ask for, best first.
///
/// `text/plain;charset=utf-8` is what every modern client uses.
/// `UTF8_STRING` is the X11 atom name, still offered by anything running
/// through XWayland, and cheap to accept.
pub const MIMES: [&str; 3] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];

/// The file list flavour a file manager drags.
pub const URI_LIST: &str = "text/uri-list";

/// GNOME's file list, which also says whether a paste moves the files.
pub const GNOME_FILES: &str = "x-special/gnome-copied-files";

/// Offers held before they are claimed. Past this the oldest are destroyed,
/// so offers nobody claims cannot pile up.
const MAX_FRESH: usize = 8;

/// An offer announced but not yet claimed, and what it carries.
pub struct Fresh {
    pub offer: wl_data_offer::WlDataOffer,
    pub text: bool,
    pub uris: bool,
    pub gnome: bool,
}

/// How long to wait for the other side to write a paste.
///
/// Bounded because the source is another application: one that has died or
/// hung must not take this window with it.
const PASTE_TIMEOUT: Duration = Duration::from_millis(500);

/// Ceiling on a paste, so a hostile or broken source cannot fill memory.
const MAX_PASTE: usize = 8 << 20;

#[derive(Default)]
pub struct Clipboard {
    pub manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    pub device: Option<wl_data_device::WlDataDevice>,
    /// What we last put on the clipboard. Kept because the compositor asks
    /// for the bytes later, possibly many times, not at the moment of copying.
    pub offered: String,
    pub source: Option<wl_data_source::WlDataSource>,
    /// The offer the compositor last announced as the selection.
    pub offer: Option<wl_data_offer::WlDataOffer>,
    /// Whether that offer carries anything we can read as text.
    pub offer_is_text: bool,
    /// Whether it carries a file list, and GNOME's form of one.
    pub offer_uris: bool,
    pub offer_gnome: bool,
    /// What the scene keeps on the clipboard, while our source holds it.
    pub owned: Option<Clip>,
    /// A paste read into here, kept so a second one reuses the room.
    pub pasted: Vec<u8>,
    /// Last input event serial. `set_selection` is refused without one that
    /// the compositor believes came from real user input.
    pub serial: u32,
    /// Offers not yet claimed as the selection or a drag.
    pub fresh: Vec<Fresh>,
    /// The drag over the window, when it carries a file list.
    pub drag: Option<wl_data_offer::WlDataOffer>,
}

impl Clipboard {
    pub fn has_selection(&self) -> bool {
        self.offer.is_some() && self.offer_is_text
    }

    /// Drop the current offer, which the compositor owns and replaces.
    pub fn replace_offer(&mut self, next: Option<wl_data_offer::WlDataOffer>) {
        if let Some(old) = self.offer.take() {
            old.destroy();
        }
        self.offer = next;
        self.offer_is_text = false;
        self.offer_uris = false;
        self.offer_gnome = false;
    }

    /// Hold a new offer until it is claimed.
    pub fn announce(&mut self, offer: wl_data_offer::WlDataOffer) {
        if self.fresh.len() == MAX_FRESH {
            self.fresh.remove(0).offer.destroy();
        }
        self.fresh.push(Fresh {
            offer,
            text: false,
            uris: false,
            gnome: false,
        });
    }

    /// Record a flavour an offer carries.
    pub fn flavour(&mut self, offer: &wl_data_offer::WlDataOffer, mime: &str) {
        let Some(f) = self.fresh.iter_mut().find(|f| f.offer == *offer) else {
            return;
        };
        f.text |= is_text(mime);
        f.uris |= mime.eq_ignore_ascii_case(URI_LIST);
        f.gnome |= mime.eq_ignore_ascii_case(GNOME_FILES);
    }

    /// The best flavour the selection offers for a file list.
    pub fn files_flavour(&self) -> Option<Flavour> {
        self.offer.as_ref()?;
        if self.offer_gnome {
            Some(Flavour::GnomeFiles)
        } else if self.offer_uris {
            Some(Flavour::UriList)
        } else {
            self.offer_is_text.then_some(Flavour::Text)
        }
    }

    /// Take a fresh offer out to be claimed.
    pub fn claim(&mut self, offer: &wl_data_offer::WlDataOffer) -> Option<Fresh> {
        let at = self.fresh.iter().position(|f| f.offer == *offer)?;
        Some(self.fresh.remove(at))
    }

    /// Let go of the drag, which left or dropped.
    pub fn end_drag(&mut self) {
        if let Some(d) = self.drag.take() {
            d.destroy();
        }
    }
}

/// Read the current selection as text.
///
/// `None` when there is nothing to paste, the source offered no text, or it
/// did not answer in time.
pub fn read_selection(clip: &Clipboard, conn: &Connection) -> Option<String> {
    let offer = clip.offer.as_ref()?;
    if !clip.offer_is_text {
        return None;
    }
    read_offer(offer, MIMES[0], conn)
}

/// Read one flavour of an offer, a paste or a drop.
pub fn read_offer(
    offer: &wl_data_offer::WlDataOffer,
    mime: &str,
    conn: &Connection,
) -> Option<String> {
    let mut buf = Vec::new();
    read_offer_into(offer, mime, conn, &mut buf)?;
    Some(lossy(&buf))
}

/// Lossy rather than refused: a source claiming UTF-8 and sending something
/// else should still paste what it got right.
fn lossy(buf: &[u8]) -> String {
    String::from_utf8_lossy(buf).into_owned()
}

/// Read one flavour of an offer into `buf`, replacing what it held.
pub fn read_offer_into(
    offer: &wl_data_offer::WlDataOffer,
    mime: &str,
    conn: &Connection,
    buf: &mut Vec<u8>,
) -> Option<()> {
    let (read, write) = rustix::pipe::pipe().ok()?;
    offer.receive(mime.to_string(), write.as_fd());
    // The write end has to be closed here, or the read below never sees end
    // of file: this process would still hold a writer open.
    drop(write);
    // Nothing is sent until the connection is flushed, so without this the
    // wait below times out every time.
    conn.flush().ok()?;
    buf.clear();
    drain(read, buf);
    Some(())
}

/// The mime type a flavour is read or offered as.
pub fn mime_of(flavour: Flavour) -> &'static str {
    match flavour {
        Flavour::Text => MIMES[0],
        Flavour::UriList => URI_LIST,
        Flavour::GnomeFiles => GNOME_FILES,
    }
}

/// The flavour a mime type asks for of what we hold, if we have it.
pub fn flavour_of(clip: Clip, mime: &str) -> Option<Flavour> {
    match clip {
        _ if is_text(mime) => Some(Flavour::Text),
        Clip::Files if mime.eq_ignore_ascii_case(URI_LIST) => Some(Flavour::UriList),
        Clip::Files if mime.eq_ignore_ascii_case(GNOME_FILES) => Some(Flavour::GnomeFiles),
        _ => None,
    }
}

/// The mime types offered for what we hold, best first.
pub fn offers(clip: Clip) -> &'static [&'static str] {
    const FILES: [&str; 5] = [GNOME_FILES, URI_LIST, MIMES[0], MIMES[1], MIMES[2]];
    match clip {
        Clip::Text => &MIMES,
        Clip::Files => &FILES,
    }
}

fn drain(fd: OwnedFd, buf: &mut Vec<u8>) {
    let start = Instant::now();
    let mut file = std::fs::File::from(fd);
    loop {
        if start.elapsed() > PASTE_TIMEOUT || buf.len() >= MAX_PASTE {
            break;
        }
        if !wait_readable(file.as_fd(), start) {
            break;
        }
        let mut chunk = [0u8; 8192];
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// True when there is something to read, false on timeout or error.
fn wait_readable(fd: std::os::fd::BorrowedFd<'_>, start: Instant) -> bool {
    let left = PASTE_TIMEOUT.saturating_sub(start.elapsed());
    let ts = Timespec {
        tv_sec: left.as_secs() as i64,
        tv_nsec: left.subsec_nanos() as i64,
    };
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    loop {
        match rustix::event::poll(&mut fds, Some(&ts)) {
            Ok(0) => return false,
            Ok(_) => return true,
            Err(Errno::INTR) => continue,
            Err(_) => return false,
        }
    }
}

/// Write the offered text into the pipe the compositor handed us.
///
/// Partial writes are looped over. A source that gives up early leaves the
/// other side with a truncated paste, which looks like a terminal bug rather
/// than a clipboard one.
pub fn write_offer(text: &str, fd: OwnedFd) {
    let mut pipe = Pipe::new(fd);
    pipe.put(text.as_bytes());
}

/// The write end of a paste, taking bytes a piece at a time until the
/// receiver goes away.
pub struct Pipe {
    file: std::fs::File,
    open: bool,
}

impl Pipe {
    pub fn new(fd: OwnedFd) -> Pipe {
        Pipe {
            file: std::fs::File::from(fd),
            open: true,
        }
    }

    pub fn put(&mut self, bytes: &[u8]) {
        use std::io::Write as _;
        let mut at = 0;
        while self.open && at < bytes.len() {
            match self.file.write(&bytes[at..]) {
                Ok(0) => self.open = false,
                Ok(n) => at += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // The receiver closed the pipe, which is what happens when it
                // has read all it wanted. Not an error.
                Err(_) => self.open = false,
            }
        }
    }
}

/// The local files a `text/uri-list` names, percent-decoded. Comments and
/// anything that is not a `file:` URI on this host are skipped.
pub fn uris_to_paths(list: &str) -> Vec<std::path::PathBuf> {
    list.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let rest = l.strip_prefix("file://")?;
            // An empty host or localhost, then the absolute path.
            let path = match rest.find('/') {
                Some(0) => rest,
                Some(i) if &rest[..i] == "localhost" => &rest[i..],
                _ => return None,
            };
            percent_decode(path).map(std::path::PathBuf::from)
        })
        .collect()
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Whether a mime type is one we can read or write as text.
pub fn is_text(mime: &str) -> bool {
    MIMES.iter().any(|m| m.eq_ignore_ascii_case(mime)) || mime.starts_with("text/plain")
}

#[cfg(test)]
mod tests;
