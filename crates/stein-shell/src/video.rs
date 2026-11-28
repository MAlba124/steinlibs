//! A video layer under a surface: a desync wl_subsurface placed below its
//! parent, showing client dmabufs at a rect the scene picks.
//!
//! ```text
//! scene.video() ──► rect changed ──► set_position (rides the parent's commit)
//!               └─► new frame ──► params.create_immed ──► attach, viewport, commit
//! wl_buffer.release ──► destroy ──► scene.video_released(id)
//! ```
//!
//! The layer commits on its own, so a frame is shown when it comes and the
//! parent repaints only when what it draws changed. The parent draws over
//! the layer, so its backdrop is clear where the video is to show. A frame
//! stays the scene's until its buffer is released, which is when the
//! decoder may write it again.

use std::os::fd::BorrowedFd;

use wayland_client::protocol::{wl_buffer, wl_region, wl_subcompositor, wl_subsurface, wl_surface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::{
    self, ZwpLinuxBufferParamsV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_feedback_v1::{
    self, ZwpLinuxDmabufFeedbackV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1};
use wayland_protocols::wp::viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter};

use crate::shell::{Shell, ShellError};
use crate::surface::Phase;

/// Planes a dmabuf has at most.
pub const MAX_PLANES: usize = 4;

/// One plane of a dmabuf.
#[derive(Clone, Copy, Debug)]
pub struct DmabufPlane<'a> {
    pub fd: BorrowedFd<'a>,
    pub offset: u32,
    pub stride: u32,
}

/// A client buffer for a video layer, a picture a decoder exported.
#[derive(Clone, Copy, Debug)]
pub struct Dmabuf<'a> {
    /// The scene's name for it, what [`crate::Scene::video_released`] says.
    pub id: u64,
    /// The buffer's size in pixels, the size the planes are laid out at.
    pub size: [u32; 2],
    /// DRM FourCC and modifier.
    pub fourcc: u32,
    pub modifier: u64,
    /// In plane order, the first `None` ends them.
    pub planes: [Option<DmabufPlane<'a>>; MAX_PLANES],
    /// x, y, w, h of the buffer that is the picture.
    pub crop: [u32; 4],
}

/// What a scene says of its video layer each time it is asked.
#[derive(Clone, Copy, Debug)]
pub struct VideoLayer<'a> {
    /// x, y, w, h in the surface's pixels the picture is scaled to. Empty
    /// hides the layer.
    pub rect: [i32; 4],
    /// A frame not shown yet, or none to keep what is shown.
    pub frame: Option<Dmabuf<'a>>,
}

/// The layer of one surface.
pub(crate) struct Layer {
    wl: wl_surface::WlSurface,
    sub: wl_subsurface::WlSubsurface,
    viewport: Option<WpViewport>,
    /// What was last placed, so an unchanged rect sends nothing.
    rect: [i32; 4],
    crop: [u32; 4],
    /// Buffers the compositor holds, by the scene's id.
    held: Vec<(wl_buffer::WlBuffer, u64)>,
    /// Something is attached.
    showing: bool,
}

/// What a video buffer's events find their way back by.
pub(crate) struct VideoBuf {
    key: u32,
    id: u64,
}

/// Ask the scene about surface `i`'s layer and make it so: placed, sized,
/// and a new frame attached and committed. A missing global leaves the
/// surface without a layer, which the scene sees as frames never released.
pub(crate) fn sync(shell: &mut Shell, i: usize, qh: &QueueHandle<Shell>) {
    let key = shell.surfs[i].key;
    if shell.surfs[i].phase == Phase::Closed {
        return;
    }
    let Some(want) = shell.scene.video(key) else {
        if let Some(layer) = shell.surfs[i].video.take() {
            hide(shell, key, layer);
        }
        return;
    };
    let (Some(compositor), Some(subs)) = (shell.compositor.as_ref(), shell.subcompositor.as_ref()) else {
        return;
    };
    let s = &mut shell.surfs[i];
    let layer = s.video.get_or_insert_with(|| {
        make(compositor, subs, shell.viewporter.as_ref(), &s.wl, key, qh)
    });
    let mut commit = false;
    let mut moved = false;
    if want.rect != layer.rect {
        let [x, y, w, h] = want.rect;
        if [x, y] != [layer.rect[0], layer.rect[1]] {
            layer.sub.set_position(x, y);
            moved = true;
        }
        if let Some(v) = layer.viewport.as_ref()
            && w > 0
            && h > 0
        {
            v.set_destination(w, h);
        }
        layer.rect = want.rect;
        commit = true;
    }
    let empty = want.rect[2] <= 0 || want.rect[3] <= 0;
    let mut unshown = None;
    if let Some(frame) = want.frame {
        match (empty, shell.dmabuf.as_ref()) {
            (false, Some(dmabuf)) => {
                attach(layer, dmabuf, &frame, key, qh);
                commit = true;
            }
            _ => unshown = Some(frame.id),
        }
    }
    if empty && layer.showing {
        layer.wl.attach(None, 0, 0);
        layer.showing = false;
        commit = true;
    }
    if commit {
        layer.wl.commit();
    }
    // The position is the parent's state, so the parent commits once more.
    if moved {
        s.repaint = true;
        if s.phase == Phase::Idle {
            s.phase = Phase::NeedsPaint;
        }
    }
    // Never attached, so the scene has it back at once.
    if let Some(id) = unshown {
        shell.scene.video_released(key, id);
    }
}

/// Every surface's layer, after the scene drained new frames.
pub(crate) fn sync_all(shell: &mut Shell, qh: &QueueHandle<Shell>) {
    for i in 0..shell.surfs.len() {
        sync(shell, i, qh);
    }
}

fn make(
    compositor: &wayland_client::protocol::wl_compositor::WlCompositor,
    subs: &wl_subcompositor::WlSubcompositor,
    viewporter: Option<&WpViewporter>,
    parent: &wl_surface::WlSurface,
    key: u32,
    qh: &QueueHandle<Shell>,
) -> Layer {
    let wl = compositor.create_surface(qh, key);
    let sub = subs.get_subsurface(&wl, parent, qh, ());
    sub.place_below(parent);
    sub.set_desync();
    // Input goes to the parent, which is over it everywhere anyway.
    let region = compositor.create_region(qh, ());
    wl.set_input_region(Some(&region));
    region.destroy();
    let viewport = viewporter.map(|v| v.get_viewport(&wl, qh, ()));
    trace!("surface {key} video layer made");
    Layer {
        wl,
        sub,
        viewport,
        rect: [0; 4],
        crop: [0; 4],
        held: Vec::with_capacity(8),
        showing: false,
    }
}

/// Import `frame` as a wl_buffer and attach it, cropped and scaled.
fn attach(layer: &mut Layer, dmabuf: &ZwpLinuxDmabufV1, frame: &Dmabuf<'_>, key: u32, qh: &QueueHandle<Shell>) {
    let params = dmabuf.create_params(qh, ());
    let (hi, lo) = ((frame.modifier >> 32) as u32, frame.modifier as u32);
    for (at, plane) in frame.planes.iter().map_while(|p| p.as_ref()).enumerate() {
        params.add(plane.fd, at as u32, plane.offset, plane.stride, hi, lo);
    }
    let [w, h] = frame.size;
    let flags = zwp_linux_buffer_params_v1::Flags::empty();
    let user = VideoBuf { key, id: frame.id };
    let buffer = params.create_immed(w as i32, h as i32, frame.fourcc, flags, qh, user);
    params.destroy();
    if let Some(v) = layer.viewport.as_ref()
        && frame.crop != layer.crop
        && frame.crop[2] > 0
        && frame.crop[3] > 0
    {
        let [x, y, cw, ch] = frame.crop.map(f64::from);
        v.set_source(x, y, cw, ch);
        layer.crop = frame.crop;
    }
    layer.wl.attach(Some(&buffer), 0, 0);
    layer.wl.damage_buffer(0, 0, i32::MAX, i32::MAX);
    trace!("surface {key} video frame {} attached, {} held", frame.id, layer.held.len() + 1);
    layer.held.push((buffer, frame.id));
    layer.showing = true;
}

/// The scene wants no layer: everything goes, and every frame the
/// compositor held is the scene's again.
fn hide(shell: &mut Shell, key: u32, layer: Layer) {
    for id in destroy(layer) {
        shell.scene.video_released(key, id);
    }
}

/// Destroy a layer, children first, and the ids of the frames it held.
pub(crate) fn destroy(layer: Layer) -> impl Iterator<Item = u64> {
    let Layer { wl, sub, viewport, held, .. } = layer;
    if let Some(v) = viewport {
        v.destroy();
    }
    sub.destroy();
    wl.destroy();
    held.into_iter().map(|(buffer, id)| {
        buffer.destroy();
        id
    })
}

impl Dispatch<wl_buffer::WlBuffer, VideoBuf> for Shell {
    fn event(
        shell: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        data: &VideoBuf,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if !matches!(event, wl_buffer::Event::Release) {
            return;
        }
        trace!("surface {} video frame {} released", data.key, data.id);
        buffer.destroy();
        if let Some(layer) = shell.surfs.iter_mut().find(|s| s.key == data.key).and_then(|s| s.video.as_mut()) {
            layer.held.retain(|(b, _)| b != buffer);
        }
        shell.scene.video_released(data.key, data.id);
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for Shell {
    fn event(
        _: &mut Self,
        _: &ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // create_immed reports a bad import as a protocol error, which ends
        // the connection, so failed only comes from the two-step create.
        if let zwp_linux_buffer_params_v1::Event::Failed = event {
            trace!("dmabuf import failed");
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for Shell {
    fn event(_: &mut Self, _: &ZwpLinuxDmabufV1, _: zwp_linux_dmabuf_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        // Bound at version 4, which sends nothing here.
    }
}

delegate_noop!(Shell: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(Shell: ignore wl_subsurface::WlSubsurface);
delegate_noop!(Shell: ignore wl_region::WlRegion);
delegate_noop!(Shell: ignore WpViewporter);
delegate_noop!(Shell: ignore WpViewport);

// ---------------------------------------------------------------------------
// what the compositor imports
// ---------------------------------------------------------------------------

/// The `(fourcc, modifier)` pairs the compositor imports as dmabufs, from
/// its default linux-dmabuf feedback, or from the modifier events of a
/// compositor before version 4. Every tranche counts, repeats once. Over
/// its own connection, so it can be asked before a shell runs.
pub fn dmabuf_formats() -> Result<Vec<(u32, u64)>, ShellError> {
    let conn = Connection::connect_to_env().map_err(ShellError::Connect)?;
    dmabuf_formats_on(&conn)
}

/// [`dmabuf_formats`] on `conn`, on an event queue of its own.
pub fn dmabuf_formats_on(conn: &Connection) -> Result<Vec<(u32, u64)>, ShellError> {
    let mut queue = conn.new_event_queue::<FormatProbe>();
    let qh = queue.handle();
    let registry = conn.display().get_registry(&qh, ());
    let mut probe = FormatProbe::default();
    queue.roundtrip(&mut probe).map_err(ShellError::Dispatch)?;
    let (name, version) = probe.global.ok_or(ShellError::MissingGlobal("zwp_linux_dmabuf_v1"))?;
    let dmabuf: ZwpLinuxDmabufV1 = registry.bind(name, version.min(4), &qh, ());
    let feedback = (version >= 4).then(|| dmabuf.get_default_feedback(&qh, ()));
    // One roundtrip normally carries everything, a few more cover a
    // compositor that splits it.
    for _ in 0..4 {
        queue.roundtrip(&mut probe).map_err(ShellError::Dispatch)?;
        if feedback.is_none() || probe.done {
            break;
        }
    }
    if let Some(f) = feedback {
        f.destroy();
    }
    dmabuf.destroy();
    Ok(probe.pairs)
}

/// What the format probe has heard.
#[derive(Default)]
pub struct FormatProbe {
    global: Option<(u32, u32)>,
    /// The feedback's format table, mapped.
    table: Option<memmap2::Mmap>,
    pairs: Vec<(u32, u64)>,
    done: bool,
}

/// A format table entry: format u32, padding u32, modifier u64, native endian.
const ENTRY: usize = 16;

/// The pairs `indices`, native endian u16s, name in `table`, appended to
/// `out` without repeats. An index past the table is skipped.
pub(crate) fn table_pairs(table: &[u8], indices: &[u8], out: &mut Vec<(u32, u64)>) {
    for ix in indices.chunks_exact(2) {
        let at = usize::from(u16::from_ne_bytes([ix[0], ix[1]])) * ENTRY;
        let Some(e) = table.get(at..at + ENTRY) else {
            continue;
        };
        let format = u32::from_ne_bytes([e[0], e[1], e[2], e[3]]);
        let modifier = u64::from_ne_bytes([e[8], e[9], e[10], e[11], e[12], e[13], e[14], e[15]]);
        push_pair(out, (format, modifier));
    }
}

fn push_pair(out: &mut Vec<(u32, u64)>, pair: (u32, u64)) {
    if !out.contains(&pair) {
        out.push(pair);
    }
}

impl Dispatch<wayland_client::protocol::wl_registry::WlRegistry, ()> for FormatProbe {
    fn event(
        probe: &mut Self,
        _: &wayland_client::protocol::wl_registry::WlRegistry,
        event: wayland_client::protocol::wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_registry::Event::Global { name, interface, version } = event
            && interface == ZwpLinuxDmabufV1::interface().name
            && version >= 3
        {
            probe.global = Some((name, version));
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for FormatProbe {
    fn event(
        probe: &mut Self,
        _: &ZwpLinuxDmabufV1,
        event: zwp_linux_dmabuf_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_linux_dmabuf_v1::Event::Modifier { format, modifier_hi, modifier_lo } = event {
            push_pair(&mut probe.pairs, (format, (u64::from(modifier_hi) << 32) | u64::from(modifier_lo)));
        }
    }
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for FormatProbe {
    fn event(
        probe: &mut Self,
        _: &ZwpLinuxDmabufFeedbackV1,
        event: zwp_linux_dmabuf_feedback_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_linux_dmabuf_feedback_v1::Event;
        match event {
            Event::FormatTable { fd, size } => {
                let file = std::fs::File::from(fd);
                // SAFETY: the compositor's table is read only and sealed
                // against shrinking, the protocol requires it, and the map
                // is only read.
                probe.table = unsafe { memmap2::MmapOptions::new().len(size as usize).map(&file) }.ok();
            }
            Event::TrancheFormats { indices } => {
                if let Some(table) = probe.table.as_ref() {
                    table_pairs(table, &indices, &mut probe.pairs);
                }
            }
            Event::Done => probe.done = true,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(format: u32, modifier: u64) -> [u8; ENTRY] {
        let mut e = [0u8; ENTRY];
        e[..4].copy_from_slice(&format.to_ne_bytes());
        e[8..].copy_from_slice(&modifier.to_ne_bytes());
        e
    }

    fn indices(ix: &[u16]) -> Vec<u8> {
        ix.iter().flat_map(|i| i.to_ne_bytes()).collect()
    }

    const NV12: u32 = 0x3231_564e;
    const XR24: u32 = 0x3432_5258;
    const Y_TILED: u64 = 0x0100_0000_0000_0002;

    #[test]
    fn a_tranche_names_pairs_of_the_table() {
        let table: Vec<u8> = [entry(XR24, 0), entry(NV12, 0), entry(NV12, Y_TILED)].concat();
        let mut out = Vec::new();
        table_pairs(&table, &indices(&[2, 1]), &mut out);
        assert_eq!(out, [(NV12, Y_TILED), (NV12, 0)]);
    }

    #[test]
    fn tranches_add_up_without_repeats_and_a_bad_index_is_skipped() {
        let table: Vec<u8> = [entry(XR24, 0), entry(NV12, Y_TILED)].concat();
        let mut out = Vec::new();
        table_pairs(&table, &indices(&[0, 1]), &mut out);
        table_pairs(&table, &indices(&[1, 7, 0]), &mut out);
        assert_eq!(out, [(XR24, 0), (NV12, Y_TILED)]);
        // An odd trailing byte is no index.
        table_pairs(&table, &[1], &mut out);
        assert_eq!(out.len(), 2);
    }
}
