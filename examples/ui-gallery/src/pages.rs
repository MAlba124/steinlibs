//! The sidebar and one function per page, plain emitters over [`Demo`].

use stein_layout::{Align, Axis, BoxSpec, Rect, SizeSpec};
use stein_ui::draw::{Quad, Shape};
use stein_ui::key::{child, hash_str, seeded};
use stein_ui::{Response, Ui};

use crate::palette::*;
use crate::text::{Style, Text};
use crate::{Demo, GalleryError, Page};

type Out = Result<(), GalleryError>;

/// Where the focused field's caret and selection go, found while emitting
/// and painted after the solve.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaretPaint {
    /// The field's content box, whose rect is known after the solve.
    pub content_index: u32,
    pub scroll: f32,
    pub caret: usize,
    pub anchor: usize,
    pub focused: bool,
}

const FIELD_TEXT: Style = Style::sans(2, TEXT);
const PLACEHOLDER: &str = "Songs, artists, shows, episodes";

fn key(name: &str) -> u64 {
    hash_str(name)
}

// Strict, so a page taller than the window overflows into its scroll area
// instead of being squeezed.
fn row(gap: f32) -> BoxSpec {
    BoxSpec::container(Axis::X)
        .gap(gap)
        .align(Align::Center)
        .w(SizeSpec::grow())
        .h(SizeSpec::children_sum().strict())
}

fn column(gap: f32) -> BoxSpec {
    BoxSpec::container(Axis::Y)
        .gap(gap)
        .w(SizeSpec::children_sum().strict())
        .h(SizeSpec::children_sum().strict())
}

fn sized(w: f32, h: f32) -> BoxSpec {
    BoxSpec::leaf().w(SizeSpec::px(w)).h(SizeSpec::px(h))
}

/// Channel wise mix of two ARGB colours, `t` from 0 to 1.
pub fn mix(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |shift: u32| {
        let x = ((a >> shift) & 0xff) as f32;
        let y = ((b >> shift) & 0xff) as f32;
        ((x + (y - x) * t + 0.5) as u32) << shift
    };
    lerp(24) | lerp(16) | lerp(8) | lerp(0)
}

pub fn root(ui: &mut Ui<'_>, text: &mut Text, demo: &mut Demo) -> Out {
    let spec = BoxSpec::container(Axis::X).fill().pad(8.0).gap(8.0);
    ui.open(key("root"), spec, BG)?;
    sidebar(ui, text, demo)?;
    let main = BoxSpec::container(Axis::Y).fill().pad_xy(28.0, 24.0).gap(22.0).clip();
    ui.open(key("main"), main, PANEL)?;
    ui.shape(Shape::round(12));
    text.label(ui, key("heading"), demo.page.name(), Style::sans(5, TEXT))?;
    if demo.page == Page::List {
        list(ui, text, demo)?;
    } else {
        scrolled(ui, text, demo)?;
    }
    ui.close()?;
    ui.close()?;
    Ok(())
}

/// A page that scrolls when the window is shorter than it. The list scrolls
/// itself, so it is not one of these.
fn scrolled(ui: &mut Ui<'_>, text: &mut Text, demo: &mut Demo) -> Out {
    use stein_widgets::ScrollArea;
    let k = seeded(key("page"), 0, demo.page as u64);
    let area = ScrollArea::begin(ui, k, Axis::Y, [SizeSpec::grow(), SizeSpec::grow()], scroll_style())?;
    ui.open(child(k, 1), column(22.0).w(SizeSpec::grow()).pad_xy(0.0, 2.0), 0)?;
    match demo.page {
        Page::Shapes => shapes(ui, text)?,
        Page::Type => type_page(ui, text)?,
        Page::Input => input_page(ui, text, demo)?,
        Page::Motion | Page::List => motion(ui, text, demo)?,
    }
    ui.close()?;
    area.end(ui)?;
    Ok(())
}

fn scroll_style() -> stein_widgets::ScrollStyle {
    stein_widgets::ScrollStyle {
        bar_size: 6.0,
        track_fill: 0,
        thumb_fill: LINE,
        thumb_hot_fill: MUTED,
        ..stein_widgets::ScrollStyle::default()
    }
}

fn sidebar(ui: &mut Ui<'_>, text: &mut Text, demo: &mut Demo) -> Out {
    let spec = BoxSpec::container(Axis::Y)
        .w(SizeSpec::px(220.0))
        .h(SizeSpec::grow())
        .pad(16.0)
        .gap(4.0);
    ui.open(key("sidebar"), spec, PANEL)?;
    ui.shape(Shape::round(12));
    ui.open(key("brand"), row(10.0).pad_xy(8.0, 10.0), 0)?;
    logo(ui, demo.clock)?;
    text.label(ui, key("brand.name"), "gallery", Style::sans(4, TEXT))?;
    ui.close()?;
    for page in Page::ALL {
        let item = seeded(key("nav"), 0, page as u64);
        let hot = ui.animate(child(item, 1), f32::from(u8::from(ui.peek_hovered(item))), 18.0);
        let fill = if page == demo.page { CHIP } else { mix(PANEL, RAISED, hot) };
        let spec = row(12.0).h(SizeSpec::px(40.0)).pad_xy(12.0, 0.0);
        if ui.open(item, spec, fill)?.clicked {
            demo.page = page;
        }
        ui.shape(Shape::round(8));
        let color = if page == demo.page { TEXT } else { MUTED };
        text.label(ui, child(item, 2), page.name(), Style::sans(2, color))?;
        ui.close()?;
    }
    ui.close()?;
    Ok(())
}

/// The three bar mark from the design, its bars breathing with the clock.
fn logo(ui: &mut Ui<'_>, clock: f32) -> Out {
    let spec = row(3.0).w(SizeSpec::px(30.0)).h(SizeSpec::px(30.0)).justify(Align::Center);
    ui.open(key("logo"), spec, ACCENT)?;
    ui.shape(Shape::round(9));
    for (i, base) in [10.0f32, 16.0, 8.0].into_iter().enumerate() {
        let h = base + 3.0 * (clock * 3.0 + i as f32 * 1.7).sin();
        ui.leaf(seeded(key("logo.bar"), 0, i as u64), sized(3.0, h.round()), BG)?;
        ui.shape(Shape::round(2));
    }
    ui.close()?;
    Ok(())
}

fn section(ui: &mut Ui<'_>, text: &mut Text, name: &str) -> Result<u64, GalleryError> {
    let k = key(name);
    ui.open(k, column(12.0).w(SizeSpec::grow()), 0)?;
    text.label(ui, child(k, 1), name, Style::sans(1, MUTED))?;
    Ok(k)
}

fn shapes(ui: &mut Ui<'_>, text: &mut Text) -> Out {
    let k = section(ui, text, "Corner radius")?;
    ui.open(child(k, 2), row(16.0), 0)?;
    for (i, r) in [0u8, 2, 4, 8, 12, 18, 26, 36].into_iter().enumerate() {
        let item = seeded(k, 3, i as u64);
        ui.open(item, column(6.0).align(Align::Center), 0)?;
        ui.leaf(child(item, 1), sized(72.0, 72.0), ARTS[i % ARTS.len()])?;
        ui.shape(Shape::round(r));
        text.label(ui, child(item, 2), RADII[i], Style::mono(0, MUTED))?;
        ui.close()?;
    }
    ui.close()?;
    ui.close()?;

    let k = section(ui, text, "Corners, strokes and pills")?;
    ui.open(child(k, 2), row(16.0), 0)?;
    let samples: [(Shape, u32, f32); 8] = [
        (Shape::corners([24, 0, 24, 0]), ARTS[1], 72.0),
        (Shape::corners([36, 36, 0, 0]), ARTS[2], 72.0),
        (Shape::corners([0, 0, 0, 36]), ARTS[4], 72.0),
        (Shape::round(12).stroked(1), LINE, 72.0),
        (Shape::round(12).stroked(3), ACCENT, 72.0),
        (Shape::PILL.stroked(2), SOFT, 72.0),
        (Shape::PILL, TEXT, 44.0),
        (Shape::PILL, CHIP, 120.0),
    ];
    for (i, (shape, fill, w)) in samples.into_iter().enumerate() {
        let h = if w == 44.0 { 44.0 } else if w == 120.0 { 36.0 } else { 72.0 };
        ui.leaf(seeded(k, 3, i as u64), sized(w, h), fill)?;
        ui.shape(shape);
    }
    ui.close()?;
    ui.close()?;
    overlaps(ui, text)?;
    chips(ui, text)
}

const RADII: [&str; 8] = ["0", "2", "4", "8", "12", "18", "26", "36"];

/// Translucent circles over each other, a floating stack.
fn overlaps(ui: &mut Ui<'_>, text: &mut Text) -> Out {
    let k = section(ui, text, "Translucent and overlapping")?;
    ui.open(child(k, 2), sized(360.0, 110.0).w(SizeSpec::px(360.0)), 0)?;
    let circles = [
        ([0.0, 10.0], 0xb0ff_8a4c),
        ([60.0, 10.0], 0xb07d_d3fc),
        ([120.0, 10.0], 0xb0c4_b5fd),
        ([180.0, 10.0], 0xb0fd_e047),
    ];
    for (i, (at, color)) in circles.into_iter().enumerate() {
        ui.leaf(seeded(k, 3, i as u64), sized(90.0, 90.0).floating(at), color)?;
        ui.shape(Shape::PILL);
    }
    ui.leaf(child(k, 4), sized(80.0, 50.0).floating([280.0, 30.0]), 0x60ff_ffff)?;
    ui.shape(Shape::round(10).stroked(2));
    ui.close()?;
    ui.close()?;
    Ok(())
}

/// The filter chips and badges of the hum design.
fn chips(ui: &mut Ui<'_>, text: &mut Text) -> Out {
    let k = section(ui, text, "Chips and badges")?;
    ui.open(child(k, 2), row(8.0), 0)?;
    for (i, name) in ["All", "Music", "Podcasts", "Downloaded"].into_iter().enumerate() {
        let chip = seeded(k, 3, i as u64);
        let (fill, color) = if i == 0 { (TEXT, BG) } else { (CHIP, TEXT) };
        ui.open(chip, row(0.0).w(SizeSpec::children_sum()).pad_xy(16.0, 8.0), fill)?;
        ui.shape(Shape::PILL);
        text.label(ui, child(chip, 1), name, Style::sans(1, color))?;
        ui.close()?;
    }
    for (i, n) in ["3", "1", "12"].into_iter().enumerate() {
        let badge = seeded(k, 4, i as u64);
        let spec = row(0.0).w(SizeSpec::children_sum().at_least(22.0)).h(SizeSpec::px(22.0)).pad_xy(7.0, 0.0);
        ui.open(badge, spec.justify(Align::Center), ACCENT)?;
        ui.shape(Shape::PILL);
        text.label(ui, child(badge, 1), n, Style::sans(0, BG))?;
        ui.close()?;
    }
    ui.close()?;
    ui.close()?;
    Ok(())
}

const PARAGRAPH: &str = "Text is shaped with harfrust, broken into lines where the script allows, and every glyph lands on a whole pixel from a shared atlas. This paragraph wraps to the width of its card, so resizing the window flows it again without shaping anything twice.";

fn type_page(ui: &mut Ui<'_>, text: &mut Text) -> Out {
    let k = section(ui, text, "Sizes")?;
    for (i, sample) in SIZE_SAMPLES.into_iter().enumerate() {
        text.label(ui, seeded(k, 2, i as u64), sample, Style::sans(i, TEXT))?;
    }
    ui.close()?;
    let k = section(ui, text, "Colour and family")?;
    ui.open(child(k, 2), row(20.0), 0)?;
    for (i, color) in [TEXT, SOFT, MUTED, ACCENT].into_iter().enumerate() {
        text.label(ui, seeded(k, 3, i as u64), "Good evening", Style::sans(3, color))?;
    }
    ui.close()?;
    text.label(ui, child(k, 4), "fn main() { let mono = \"DejaVu Sans Mono\"; }", Style::mono(1, SOFT))?;
    text.label(ui, child(k, 5), "Gr\u{fc}\u{df}e, na\u{ef}ve caf\u{e9}, \u{395}\u{3bb}\u{3bb}\u{3b7}\u{3bd}\u{3b9}\u{3ba}\u{3ac}, \u{2014} \u{2192} \u{2713}", Style::sans(2, TEXT))?;
    ui.close()?;
    let k = section(ui, text, "Wrapping and eliding")?;
    let card = BoxSpec::container(Axis::Y).w(SizeSpec::grow().at_most(640.0)).h(SizeSpec::children_sum()).pad(16.0);
    ui.open(child(k, 2), card, CARD)?;
    ui.shape(Shape::round(10));
    text.paragraph(ui, child(k, 3), PARAGRAPH, Style::sans(1, SOFT), 600.0)?;
    ui.close()?;
    let narrow = BoxSpec::container(Axis::X).w(SizeSpec::px(220.0)).h(SizeSpec::children_sum()).pad(10.0).clip();
    ui.open(child(k, 4), narrow, RAISED)?;
    ui.shape(Shape::round(8));
    text.label(ui, child(k, 5), "Ep. 141 \u{2014} The people who keep the internet's clocks", Style::sans(1, TEXT))?;
    ui.close()?;
    ui.close()?;
    Ok(())
}

const SIZE_SAMPLES: [&str; 6] = [
    "12 px  Playlist \u{b7} 412 songs",
    "14 px  The Long Signal \u{b7} Today \u{b7} 1 h 12 min",
    "16 px  Walking the old postal road",
    "20 px  New from your shows",
    "26 px  Continue listening",
    "34 px  Good evening",
];

fn input_page(ui: &mut Ui<'_>, text: &mut Text, demo: &mut Demo) -> Out {
    text_field(ui, text, demo)?;
    let k = section(ui, text, "Buttons and switches")?;
    ui.open(child(k, 2), row(16.0), 0)?;
    if button(ui, text, child(k, 3), "Click me", true)?.clicked {
        demo.clicks += 1;
    }
    let count = format_count(demo.clicks);
    text.label(ui, child(k, 4), &count, Style::sans(1, MUTED))?;
    if toggle(ui, child(k, 5), demo.toggle)?.clicked {
        demo.toggle = !demo.toggle;
    }
    text.label(ui, child(k, 6), if demo.toggle { "Auto-add on" } else { "Auto-add off" }, Style::sans(1, MUTED))?;
    ui.close()?;
    ui.close()?;
    let k = section(ui, text, "Slider")?;
    demo.slider = slider(ui, child(k, 2), demo.slider)?;
    ui.close()?;
    let k = section(ui, text, "Pointer and keys")?;
    let input = ui.input();
    let pointer = format!(
        "pointer {:.0}, {:.0}{}   wheel {:.1}",
        input.pointer[0],
        input.pointer[1],
        if input.pointer_down { "  down" } else { "" },
        input.wheel[1]
    );
    text.label(ui, child(k, 2), &pointer, Style::mono(1, SOFT))?;
    let keys = demo.keys[..demo.key_count]
        .iter()
        .map(|sym| format!("{sym:#06x}"))
        .collect::<Vec<_>>()
        .join(" ");
    let keys = if keys.is_empty() { "press keys".to_string() } else { keys };
    text.label(ui, child(k, 3), &keys, Style::mono(1, MUTED))?;
    ui.close()?;
    Ok(())
}

/// The search box of the design: stein-widgets' field, our glyphs.
fn text_field(ui: &mut Ui<'_>, text: &mut Text, demo: &mut Demo) -> Out {
    use stein_widgets::{FieldStyle, field};
    let k = section(ui, text, "Text field")?;
    let style = FieldStyle {
        fill: RAISED,
        focused_fill: RAISED,
        border: RAISED,
        focused_border: ACCENT,
        caret: ACCENT,
        selection: 0x80ff_8a4c,
        border_width: 1.0,
        radius: 22.0,
        // Both axes, so it leaves the content room for a line.
        pad: 11.0,
        height: 44.0,
        caret_width: 2.0,
    };
    let mut stops = std::mem::take(&mut demo.field_stops);
    text.stops(FIELD_TEXT, demo.field_text(), &mut stops);
    let width = SizeSpec::grow().at_most(420.0);
    let result = field(ui, child(k, 2), width, &mut demo.field, &mut demo.field_len, &stops, style);
    demo.field_stops = stops;
    let f = result?;
    if f.submitted {
        demo.submitted += 1;
    }
    demo.caret = Some(CaretPaint {
        content_index: f.content_index,
        scroll: f.scroll,
        caret: f.cursor.caret as usize,
        anchor: f.cursor.anchor as usize,
        focused: f.focused,
    });
    let status = format!("{} bytes, submitted {} times, Enter submits", demo.field_len, demo.submitted);
    text.label(ui, child(k, 3), &status, Style::sans(0, MUTED))?;
    ui.close()?;
    Ok(())
}

fn format_count(clicks: u32) -> String {
    match clicks {
        0 => "not clicked yet".to_string(),
        1 => "clicked once".to_string(),
        n => format!("clicked {n} times"),
    }
}

/// A pill button whose fill eases toward its hover and press colours.
pub fn button(ui: &mut Ui<'_>, text: &mut Text, k: u64, label: &str, primary: bool) -> Result<Response, GalleryError> {
    let hot = ui.animate(child(k, 1), f32::from(u8::from(ui.peek_hovered(k))), 16.0);
    let held = ui.animate(child(k, 2), f32::from(u8::from(ui.peek_active(k))), 30.0);
    let (rest, lit, ink) = if primary { (TEXT, 0xffff_ffff, BG) } else { (CHIP, LINE, TEXT) };
    let fill = mix(mix(rest, lit, hot), ACCENT, held * 0.6);
    let spec = row(0.0).w(SizeSpec::children_sum()).h(SizeSpec::px(40.0)).pad_xy(20.0, 0.0);
    let response = ui.open(k, spec, fill)?;
    ui.shape(Shape::PILL);
    text.label(ui, child(k, 3), label, Style::sans(1, ink))?;
    ui.close()?;
    Ok(response)
}

/// The design's switch, its knob easing between the ends.
fn toggle(ui: &mut Ui<'_>, k: u64, on: bool) -> Result<Response, GalleryError> {
    let t = ui.animate(child(k, 1), f32::from(u8::from(on)), 14.0);
    let response = ui.open(k, sized(44.0, 26.0), mix(LINE, ACCENT, t))?;
    ui.shape(Shape::PILL);
    let x = 3.0 + t * (44.0 - 20.0 - 6.0);
    ui.leaf(child(k, 2), sized(20.0, 20.0).floating([x, 3.0]).passive(), BG)?;
    ui.shape(Shape::PILL);
    ui.close()?;
    Ok(response)
}

/// A seek bar with a draggable knob. Returns the new value.
fn slider(ui: &mut Ui<'_>, k: u64, value: f32) -> Result<f32, GalleryError> {
    let rect = ui.state(k).map_or(Rect::default(), |s| s.last_rect);
    let spec = BoxSpec::container(Axis::X).w(SizeSpec::grow().at_most(520.0)).h(SizeSpec::px(24.0));
    let response = ui.open(k, spec, 0)?;
    let width = rect.width().max(1.0);
    let mut value = value;
    if response.active {
        value = ((ui.input().pointer[0] - rect.min[0]) / width).clamp(0.0, 1.0);
    }
    let lit = ui.animate(child(k, 1), f32::from(u8::from(response.hovered || response.active)), 16.0);
    ui.leaf(child(k, 2), sized(width, 4.0).floating([0.0, 10.0]).passive(), LINE)?;
    ui.shape(Shape::PILL);
    ui.leaf(child(k, 3), sized((value * width).max(4.0), 4.0).floating([0.0, 10.0]).passive(), mix(SOFT, ACCENT, lit))?;
    ui.shape(Shape::PILL);
    let knob = 10.0 + 4.0 * lit;
    let at = [value * width - knob * 0.5, 12.0 - knob * 0.5];
    ui.leaf(child(k, 4), sized(knob, knob).floating(at).passive(), TEXT)?;
    ui.shape(Shape::PILL);
    ui.close()?;
    Ok(value)
}

fn motion(ui: &mut Ui<'_>, text: &mut Text, demo: &mut Demo) -> Out {
    let k = section(ui, text, "Easing")?;
    let clock = demo.clock;
    ui.open(child(k, 2), sized(420.0, 30.0).w(SizeSpec::px(420.0)), 0)?;
    ui.leaf(child(k, 3), sized(420.0, 4.0).floating([0.0, 13.0]), LINE)?;
    ui.shape(Shape::PILL);
    let phase = (clock / 1.6).fract();
    let swing = if phase < 0.5 { phase * 2.0 } else { 2.0 - phase * 2.0 };
    let eased = swing * swing * (3.0 - 2.0 * swing);
    ui.leaf(child(k, 4), sized(24.0, 24.0).floating([eased * 396.0, 3.0]), ACCENT)?;
    ui.shape(Shape::PILL);
    ui.close()?;
    ui.close()?;
    let k = section(ui, text, "Equalizer, spinner and pulse")?;
    ui.open(child(k, 2), row(40.0), 0)?;
    equalizer(ui, child(k, 3), clock)?;
    spinner(ui, child(k, 4), clock)?;
    let pulse = 40.0 + 14.0 * (clock * 4.0).sin().abs();
    ui.open(child(k, 5), sized(72.0, 72.0).w(SizeSpec::px(72.0)), 0)?;
    let inset = (72.0 - pulse) * 0.5;
    ui.leaf(child(k, 6), sized(pulse, pulse).floating([inset, inset]), mix(ACCENT, 0x00ff_8a4c, (pulse - 40.0) / 28.0))?;
    ui.shape(Shape::PILL);
    ui.close()?;
    ui.close()?;
    ui.close()?;
    let k = section(ui, text, "Hover to lift")?;
    hover_cards(ui, text, k)?;
    ui.close()?;
    let label = if demo.playing { "Pause" } else { "Play" };
    if button(ui, text, key("motion.play"), label, false)?.clicked {
        demo.playing = !demo.playing;
    }
    Ok(())
}

fn equalizer(ui: &mut Ui<'_>, k: u64, clock: f32) -> Out {
    let spec = row(5.0).w(SizeSpec::children_sum()).h(SizeSpec::px(64.0)).align(Align::End);
    ui.open(k, spec, 0)?;
    for i in 0..7 {
        let wave = (clock * (2.0 + i as f32 * 0.37) + i as f32).sin() * 0.5 + 0.5;
        let h = 10.0 + 54.0 * wave;
        ui.leaf(seeded(k, 1, i), sized(8.0, h.round()), ARTS[i as usize % ARTS.len()])?;
        ui.shape(Shape::round(4));
    }
    ui.close()?;
    Ok(())
}

fn spinner(ui: &mut Ui<'_>, k: u64, clock: f32) -> Out {
    ui.open(k, sized(64.0, 64.0).w(SizeSpec::px(64.0)), 0)?;
    let head = (clock * 1.5).fract() * 8.0;
    for i in 0..8 {
        let angle = i as f32 / 8.0 * std::f32::consts::TAU;
        let behind = (head - i as f32).rem_euclid(8.0) / 8.0;
        let at = [28.0 + 24.0 * angle.cos() - 4.0, 28.0 + 24.0 * angle.sin() - 4.0];
        ui.leaf(seeded(k, 1, i), sized(10.0, 10.0).floating(at), mix(TEXT, PANEL, behind))?;
        ui.shape(Shape::PILL);
    }
    ui.close()?;
    Ok(())
}

fn hover_cards(ui: &mut Ui<'_>, text: &mut Text, k: u64) -> Out {
    ui.open(child(k, 2), row(12.0), 0)?;
    for (i, title) in ["Glass Harbour", "Late Night Focus", "Low Tide Radio"].into_iter().enumerate() {
        let card = seeded(k, 3, i as u64);
        let lift = ui.animate(child(card, 1), f32::from(u8::from(ui.peek_hovered(card))), 12.0);
        let spec = row(14.0).w(SizeSpec::px(240.0)).h(SizeSpec::px(64.0 + 8.0 * lift));
        ui.open(card, spec.pad_xy(0.0, 0.0), mix(CARD, RAISED, lift))?;
        ui.shape(Shape::round(8));
        ui.leaf(child(card, 2), sized(64.0 + 8.0 * lift, 64.0 + 8.0 * lift).passive(), ARTS[(i + 2) % ARTS.len()])?;
        ui.shape(Shape::corners([8, 0, 0, 8]));
        text.label(ui, child(card, 3), title, Style::sans(1, mix(SOFT, TEXT, lift)))?;
        ui.close()?;
    }
    ui.close()?;
    Ok(())
}

const ROWS: u32 = 500;
const ROW_H: f32 = 64.0;

fn list(ui: &mut Ui<'_>, text: &mut Text, demo: &mut Demo) -> Out {
    use stein_widgets::{ScrollArea, VirtualList};
    let area = ScrollArea::begin(ui, key("list"), Axis::Y, [SizeSpec::grow(), SizeSpec::grow()], scroll_style())?;
    let list = VirtualList::begin(ui, key("list.rows"), area.offset(), area.view_len(), ROWS, ROW_H)?;
    for i in list.range().first..list.range().end {
        let id = u64::from(i);
        let k = list.row_key(id);
        let hot = ui.animate(child(k, 9), f32::from(u8::from(ui.peek_hovered(k))), 20.0);
        let fill = if i == demo.selected { RAISED } else { mix(PANEL, CARD, hot) };
        let spec = row(12.0).h(SizeSpec::px(ROW_H)).pad_xy(8.0, 0.0);
        if ui.open(k, spec, fill)?.clicked {
            demo.selected = i;
        }
        ui.shape(Shape::round(8));
        ui.leaf(child(k, 1), sized(48.0, 48.0).passive(), ARTS[i as usize % ARTS.len()])?;
        ui.shape(Shape::round(6));
        ui.open(child(k, 2), column(3.0).passive(), 0)?;
        let title = TITLES[i as usize % TITLES.len()];
        text.label(ui, child(k, 3), title, Style::sans(1, TEXT))?;
        let sub = format!("Episode {} \u{b7} {} min", ROWS - i, 18 + i % 50);
        text.label(ui, child(k, 4), &sub, Style::sans(0, MUTED))?;
        ui.close()?;
        ui.close()?;
    }
    list.end(ui)?;
    area.end(ui)?;
    Ok(())
}

const TITLES: [&str; 6] = [
    "Why batteries are the new oil",
    "The people who keep the internet's clocks",
    "Reindeer, radar and a very cold night",
    "Walking the old postal road across Filefjell",
    "The quiet math of index funds",
    "Northern Lights, Southern Heart",
];

/// The focused field's selection and caret, over the glyphs. After the solve.
pub fn paint_caret(ui: &mut Ui<'_>, text: &mut Text, demo: &mut Demo) -> Out {
    let Some(paint) = demo.caret else {
        return Ok(());
    };
    let Some((rect, clip)) = ui.geometry_of(paint.content_index) else {
        return Ok(());
    };
    let line_h = text.line_height(FIELD_TEXT);
    let top = rect.min[1] + ((rect.height() - line_h) * 0.5).round();
    let left = rect.min[0] - paint.scroll;
    let mut stops = std::mem::take(&mut demo.field_stops);
    let body = std::str::from_utf8(&demo.field[..demo.field_len]).unwrap_or("");
    text.stops(FIELD_TEXT, body, &mut stops);
    let x_of = |byte: usize| {
        let boundary = stein_widgets::field::byte_to_boundary(body.as_bytes(), byte);
        left + stops.get(boundary).copied().unwrap_or(0.0)
    };
    if paint.focused && paint.caret != paint.anchor {
        let (a, b) = (x_of(paint.caret.min(paint.anchor)), x_of(paint.caret.max(paint.anchor)));
        let sel = Rect { min: [a, top], max: [b, top + line_h] };
        ui.push_quad(Quad::solid(sel, clip, 0x80ff_8a4c).shaped(Shape::round(3)));
    }
    let caret_x = x_of(paint.caret);
    demo.field_stops = stops;
    let wide = Rect { min: [left, top], max: [rect.max[0].max(left + 4096.0), top + line_h] };
    if body.is_empty() {
        text.draw_line(ui, PLACEHOLDER, Style { color: MUTED, ..FIELD_TEXT }, wide, clip)?;
    } else {
        text.draw_line(ui, body, FIELD_TEXT, wide, clip)?;
    }
    if paint.focused {
        let bar = Rect { min: [caret_x.round(), top], max: [caret_x.round() + 2.0, top + line_h] };
        ui.push_quad(Quad::solid(bar, clip, ACCENT).shaped(Shape::round(1)));
    }
    Ok(())
}
