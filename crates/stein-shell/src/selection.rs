//! The Wayland selection and drag and drop, between the scene and the
//! compositor. The protocol halves live in crate::clipboard.

use wayland_client::protocol::{
    wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, delegate_noop};

use crate::clipboard;
use crate::scene::{Clip, Flavour};
use crate::shell::{Shell, wake};

pub(crate) fn bind_data_device(shell: &mut Shell, qh: &QueueHandle<Shell>) {
    let (Some(mgr), Some(seat)) = (shell.clip.manager.clone(), shell.seat.clone()) else {
        // Not every compositor advertises one, and a terminal without a
        // clipboard is worse than one that says so once.
        trace!("no wl_data_device_manager, the clipboard will be dead");
        return;
    };
    shell.clip.device = Some(mgr.get_data_device(&seat, qh, ()));
}

/// Offer a fresh source for `mimes` as the selection, replacing ours.
fn offer(shell: &mut Shell, qh: &QueueHandle<Shell>, mimes: &[&str]) -> bool {
    let (Some(mgr), Some(device)) = (shell.clip.manager.clone(), shell.clip.device.clone()) else {
        return false;
    };
    // The previous source is replaced rather than reused: a source can only
    // be offered once, and the compositor will cancel the old one anyway.
    if let Some(old) = shell.clip.source.take() {
        old.destroy();
    }
    let source = mgr.create_data_source(qh, ());
    for m in mimes {
        source.offer((*m).to_string());
    }
    device.set_selection(Some(&source), shell.clip.serial);
    shell.clip.source = Some(source);
    true
}

/// Hand the scene's text to the compositor as the selection.
fn copy(shell: &mut Shell, qh: &QueueHandle<Shell>, text: String) {
    if offer(shell, qh, &clipboard::MIMES) {
        shell.clip.offered = text;
        shell.clip.owned = None;
    }
}

/// Everything the clipboard does in one frame, after the scene has built.
pub(crate) fn pump_clipboard(shell: &mut Shell, qh: &QueueHandle<Shell>, conn: &Connection) {
    if let Some(text) = shell.scene.take_copy() {
        copy(shell, qh, text);
    }
    if let Some(clip) = shell.scene.take_clip()
        && offer(shell, qh, clipboard::offers(clip))
    {
        shell.clip.offered.clear();
        shell.clip.owned = Some(clip);
    }
    if shell.scene.wants_paste_files() {
        paste_files(shell, conn);
    }
    if !shell.scene.wants_paste() {
        return;
    }
    // Our own selection is answered here: asking the compositor would have
    // it ask us, and we would not hear it while waiting on the pipe.
    if shell.clip.source.is_some() {
        let mut text = core::mem::take(&mut shell.clip.offered);
        if shell.clip.owned.is_some() {
            let mut buf = Vec::new();
            shell
                .scene
                .write_clip(Flavour::Text, &mut |b| buf.extend_from_slice(b));
            text = String::from_utf8_lossy(&buf).into_owned();
        }
        shell.scene.paste(&text);
        if shell.clip.owned.is_none() {
            shell.clip.offered = text;
        }
        return;
    }
    if let Some(text) = clipboard::read_selection(&shell.clip, conn) {
        shell.scene.paste(&text);
    }
}

/// Hand the scene the clipboard's file list, in the best flavour it has.
fn paste_files(shell: &mut Shell, conn: &Connection) {
    let mut buf = core::mem::take(&mut shell.clip.pasted);
    buf.clear();
    let own = shell
        .clip
        .source
        .is_some()
        .then_some(shell.clip.owned)
        .flatten();
    let flavour = match own {
        Some(Clip::Files) => Some(Flavour::GnomeFiles),
        Some(Clip::Text) => Some(Flavour::Text),
        None => shell.clip.files_flavour(),
    };
    let read = match (own, flavour, shell.clip.offer.as_ref()) {
        (Some(_), Some(f), _) => {
            shell.scene.write_clip(f, &mut |b| buf.extend_from_slice(b));
            true
        }
        (None, Some(f), Some(o)) => {
            clipboard::read_offer_into(o, clipboard::mime_of(f), conn, &mut buf).is_some()
        }
        _ => false,
    };
    let bytes = if read { buf.as_slice() } else { &[] };
    shell
        .scene
        .paste_files(flavour.unwrap_or(Flavour::Text), bytes);
    shell.clip.pasted = buf;
}

/// A drag entered the window: take it when it carries files.
fn drag_enter(shell: &mut Shell, serial: u32, id: Option<wl_data_offer::WlDataOffer>) {
    shell.clip.end_drag();
    let Some(f) = id.as_ref().and_then(|o| shell.clip.claim(o)) else {
        return;
    };
    if !f.uris {
        // Nothing we can take: say so, and let it go.
        f.offer.accept(serial, None);
        f.offer.destroy();
        return;
    }
    f.offer
        .accept(serial, Some(clipboard::URI_LIST.to_string()));
    if f.offer.version() >= 3 {
        use wl_data_device_manager::DndAction;
        f.offer.set_actions(DndAction::Copy, DndAction::Copy);
    }
    shell.clip.drag = Some(f.offer);
}

/// The drag was let go over the window: read its files and hand them over.
fn drag_drop(shell: &mut Shell, conn: &Connection) {
    let Some(offer) = shell.clip.drag.take() else {
        return;
    };
    let paths = clipboard::read_offer(&offer, clipboard::URI_LIST, conn)
        .map(|l| clipboard::uris_to_paths(&l))
        .unwrap_or_default();
    if offer.version() >= 3 {
        offer.finish();
    }
    offer.destroy();
    trace!("wl_data_device.drop {} path(s)", paths.len());
    if !paths.is_empty() {
        let key = shell.drag_key.unwrap_or(0);
        shell.scene.dropped(key, &paths);
        wake(shell);
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for Shell {
    fn event(
        shell: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        conn: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // A new offer, whose mime types arrive as separate events before
            // it is announced as the selection or a drag.
            wl_data_device::Event::DataOffer { id } => shell.clip.announce(id),
            wl_data_device::Event::Selection { id } => {
                // A null id means the clipboard was cleared.
                let fresh = id.as_ref().and_then(|o| shell.clip.claim(o));
                let flags = fresh
                    .as_ref()
                    .map_or((false, false, false), |f| (f.text, f.uris, f.gnome));
                shell.clip.replace_offer(fresh.map(|f| f.offer));
                (
                    shell.clip.offer_is_text,
                    shell.clip.offer_uris,
                    shell.clip.offer_gnome,
                ) = flags;
                trace!("wl_data_device.selection text={}", shell.clip.offer_is_text);
            }
            wl_data_device::Event::Enter {
                serial,
                surface,
                id,
                ..
            } => {
                shell.drag_key = surface.data::<u32>().copied();
                drag_enter(shell, serial, id);
            }
            wl_data_device::Event::Leave => shell.clip.end_drag(),
            wl_data_device::Event::Drop => drag_drop(shell, conn),
            _ => {}
        }
    }

    wayland_client::event_created_child!(Shell, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for Shell {
    fn event(
        shell: &mut Self,
        offer: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            shell.clip.flavour(offer, &mime_type);
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for Shell {
    fn event(
        shell: &mut Self,
        source: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // Someone is pasting what we copied. The bytes are written now,
            // not at the time of the copy.
            wl_data_source::Event::Send { mime_type, fd } => send(shell, &mime_type, fd),
            // Another application took the clipboard.
            wl_data_source::Event::Cancelled => {
                source.destroy();
                if shell.clip.source.as_ref() == Some(source) {
                    shell.clip.source = None;
                    shell.clip.offered.clear();
                    if shell.clip.owned.take().is_some() {
                        shell.scene.clip_lost();
                    }
                }
            }
            _ => {}
        }
    }
}

/// Write what we hold as `mime` into the pipe a paste handed us.
fn send(shell: &Shell, mime: &str, fd: std::os::fd::OwnedFd) {
    match shell.clip.owned {
        Some(clip) => {
            let Some(f) = clipboard::flavour_of(clip, mime) else {
                return;
            };
            let mut pipe = clipboard::Pipe::new(fd);
            shell.scene.write_clip(f, &mut |b| pipe.put(b));
        }
        None if clipboard::is_text(mime) => clipboard::write_offer(&shell.clip.offered, fd),
        None => {}
    }
}

delegate_noop!(Shell: ignore wl_data_device_manager::WlDataDeviceManager);
