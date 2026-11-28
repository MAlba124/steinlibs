#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::vec::Vec;

use stein_layout::{Axis, Insets, LayoutStorage, Options, Rect, SizeSpec};
use stein_ui::draw::{CustomRegion, Quad, Shape};
use stein_ui::{Input, KeyEvent, Mods, Ui, UiStorage, WidgetState, keys};

use super::*;

// Pure editing core.

const CAP: usize = 32;

struct Buf {
    bytes: [u8; CAP],
    len: usize,
    cursor: Cursor,
}

impl Buf {
    fn new(text: &str) -> Buf {
        let mut bytes = [0u8; CAP];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        Buf {
            bytes,
            len: text.len(),
            cursor: Cursor::at(text.len() as u32),
        }
    }
    fn text(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).expect("valid utf8")
    }
    fn run(&mut self, input: &Input) -> Edit {
        edit(&mut self.bytes, &mut self.len, &mut self.cursor, input)
    }
    fn typed(&mut self, text: &str) -> Edit {
        let mut input = Input::default();
        assert!(input.push_text(text));
        self.run(&input)
    }
    fn key(&mut self, sym: u32, mods: Mods) -> Edit {
        self.run(&key_input(sym, mods))
    }
    fn plain(&mut self, sym: u32) -> Edit {
        self.key(sym, Mods::default())
    }
}

fn key_input(sym: u32, mods: Mods) -> Input {
    let mut input = Input::default();
    input.push_key(KeyEvent {
        sym,
        pressed: true,
        repeat: false,
        mods,
    });
    input
}

const SHIFT: Mods = Mods {
    ctrl: false,
    shift: true,
    alt: false,
    logo: false,
};
const CTRL: Mods = Mods {
    ctrl: true,
    shift: false,
    alt: false,
    logo: false,
};
const CTRL_SHIFT: Mods = Mods {
    ctrl: true,
    shift: true,
    alt: false,
    logo: false,
};

// "aé漢b" is 1 + 2 + 3 + 1 bytes, boundaries at 0, 1, 3, 6, 7.
const MIXED: &str = "aé漢b";

#[test]
fn typing_multibyte_text_advances_the_caret_by_bytes() {
    let mut buf = Buf::new("");
    let out = buf.typed(MIXED);
    assert!(out.changed && out.moved);
    assert_eq!(buf.text(), MIXED);
    assert_eq!(buf.cursor, Cursor::at(7));
    buf.plain(keys::LEFT);
    buf.plain(keys::LEFT);
    assert_eq!(buf.cursor, Cursor::at(3));
    buf.typed("x");
    assert_eq!(buf.text(), "aéx漢b");
    assert_eq!(buf.cursor, Cursor::at(4));
}

#[test]
fn backspace_removes_whole_multibyte_chars() {
    let mut buf = Buf::new(MIXED);
    buf.cursor = Cursor::at(6);
    let out = buf.plain(keys::BACKSPACE);
    assert!(out.changed);
    assert_eq!(buf.text(), "aéb");
    assert_eq!(buf.cursor, Cursor::at(3));
    buf.plain(keys::BACKSPACE);
    assert_eq!(buf.text(), "ab");
    assert_eq!(buf.cursor, Cursor::at(1));
    buf.cursor = Cursor::at(0);
    let out = buf.plain(keys::BACKSPACE);
    assert!(!out.changed && !out.moved);
    assert_eq!(buf.text(), "ab");
}

#[test]
fn delete_removes_whole_multibyte_chars() {
    let mut buf = Buf::new(MIXED);
    buf.cursor = Cursor::at(1);
    assert!(buf.plain(keys::DELETE).changed);
    assert_eq!(buf.text(), "a漢b");
    assert!(buf.plain(keys::DELETE).changed);
    assert_eq!(buf.text(), "ab");
    assert_eq!(buf.cursor, Cursor::at(1));
    buf.cursor = Cursor::at(2);
    assert!(!buf.plain(keys::DELETE).changed);
    assert_eq!(buf.text(), "ab");
}

#[test]
fn typing_replaces_the_selection() {
    let mut buf = Buf::new(MIXED);
    buf.cursor = Cursor {
        caret: 6,
        anchor: 1,
    };
    assert!(buf.typed("Z").changed);
    assert_eq!(buf.text(), "aZb");
    assert_eq!(buf.cursor, Cursor::at(2));
}

#[test]
fn backspace_and_delete_remove_a_selection_whichever_way_it_points() {
    let mut buf = Buf::new(MIXED);
    buf.cursor = Cursor {
        caret: 1,
        anchor: 6,
    };
    buf.plain(keys::BACKSPACE);
    assert_eq!(buf.text(), "ab");
    assert_eq!(buf.cursor, Cursor::at(1));
    let mut buf = Buf::new(MIXED);
    buf.cursor = Cursor {
        caret: 6,
        anchor: 3,
    };
    buf.plain(keys::DELETE);
    assert_eq!(buf.text(), "aéb");
    assert_eq!(buf.cursor, Cursor::at(3));
}

#[test]
fn shift_extends_and_a_plain_arrow_collapses_onto_the_edge() {
    let mut buf = Buf::new(MIXED);
    buf.cursor = Cursor::at(0);
    buf.key(keys::RIGHT, SHIFT);
    buf.key(keys::RIGHT, SHIFT);
    assert_eq!(
        buf.cursor,
        Cursor {
            caret: 3,
            anchor: 0
        }
    );
    let out = buf.plain(keys::LEFT);
    assert!(out.moved);
    assert_eq!(buf.cursor, Cursor::at(0));
    buf.key(keys::RIGHT, SHIFT);
    buf.key(keys::RIGHT, SHIFT);
    buf.plain(keys::RIGHT);
    assert_eq!(buf.cursor, Cursor::at(3));
    buf.key(keys::LEFT, SHIFT);
    assert_eq!(
        buf.cursor,
        Cursor {
            caret: 1,
            anchor: 3
        }
    );
    // At the edges a move stays put and reports nothing.
    let mut buf = Buf::new("ab");
    assert!(!buf.plain(keys::RIGHT).moved);
    buf.cursor = Cursor::at(0);
    assert!(!buf.plain(keys::LEFT).moved);
}

#[test]
fn ctrl_moves_by_word() {
    // h0 .. o4 ,5 space6 w7 ö8 r10 l11 d12 space13 f14 o15 o16, len 17
    let mut buf = Buf::new("hello, wörld foo");
    buf.key(keys::LEFT, CTRL);
    assert_eq!(buf.cursor, Cursor::at(14));
    buf.key(keys::LEFT, CTRL);
    assert_eq!(buf.cursor, Cursor::at(7));
    buf.key(keys::LEFT, CTRL);
    assert_eq!(buf.cursor, Cursor::at(0));
    buf.key(keys::RIGHT, CTRL);
    assert_eq!(buf.cursor, Cursor::at(5));
    buf.key(keys::RIGHT, CTRL);
    assert_eq!(buf.cursor, Cursor::at(13));
    buf.key(keys::LEFT, CTRL_SHIFT);
    assert_eq!(
        buf.cursor,
        Cursor {
            caret: 7,
            anchor: 13
        }
    );
    buf.key(keys::RIGHT, CTRL);
    buf.key(keys::RIGHT, CTRL);
    assert_eq!(buf.cursor, Cursor::at(17));
}

#[test]
fn ctrl_backspace_and_delete_remove_a_word() {
    let mut buf = Buf::new("one two three");
    buf.key(keys::BACKSPACE, CTRL);
    assert_eq!(buf.text(), "one two ");
    buf.cursor = Cursor::at(0);
    buf.key(keys::DELETE, CTRL);
    assert_eq!(buf.text(), " two ");
    assert_eq!(buf.cursor, Cursor::at(0));
}

#[test]
fn home_and_end_move_and_shift_selects_to_them() {
    let mut buf = Buf::new(MIXED);
    buf.cursor = Cursor::at(3);
    buf.plain(keys::HOME);
    assert_eq!(buf.cursor, Cursor::at(0));
    buf.plain(keys::END);
    assert_eq!(buf.cursor, Cursor::at(7));
    buf.cursor = Cursor::at(3);
    buf.key(keys::HOME, SHIFT);
    assert_eq!(
        buf.cursor,
        Cursor {
            caret: 0,
            anchor: 3
        }
    );
    buf.key(keys::END, SHIFT);
    assert_eq!(
        buf.cursor,
        Cursor {
            caret: 7,
            anchor: 3
        }
    );
}

#[test]
fn ctrl_a_selects_everything_and_typing_replaces_it() {
    let mut buf = Buf::new(MIXED);
    buf.cursor = Cursor::at(3);
    let out = buf.key(keys::A, CTRL);
    assert!(out.moved && !out.changed);
    assert_eq!(
        buf.cursor,
        Cursor {
            caret: 7,
            anchor: 0
        }
    );
    assert_eq!(selection(&buf.bytes, buf.len, buf.cursor), MIXED.as_bytes());
    buf.typed("new");
    assert_eq!(buf.text(), "new");
}

#[test]
fn capacity_truncates_on_a_char_boundary() {
    let mut bytes = [0u8; 8];
    let mut len = 0;
    let mut cursor = Cursor::default();
    assert!(insert(&mut bytes, &mut len, &mut cursor, "abcdef"));
    // Three bytes do not fit in two, so nothing is written.
    assert!(!insert(&mut bytes, &mut len, &mut cursor, "漢"));
    assert_eq!(len, 6);
    // é fits exactly, and the x after it is cut.
    assert!(insert(&mut bytes, &mut len, &mut cursor, "éx"));
    assert_eq!(core::str::from_utf8(&bytes[..len]), Ok("abcdefé"));
    assert_eq!(cursor, Cursor::at(8));
    // A replacement truncates against the room the selection freed.
    cursor = Cursor {
        caret: 8,
        anchor: 4,
    };
    assert!(insert(&mut bytes, &mut len, &mut cursor, "漢字"));
    assert_eq!(core::str::from_utf8(&bytes[..len]), Ok("abcd漢"));
    assert_eq!(cursor, Cursor::at(7));
}

#[test]
fn typing_mid_text_shifts_the_tail_and_respects_capacity() {
    let mut bytes = [0u8; 6];
    let mut len = 0;
    let mut cursor = Cursor::default();
    insert(&mut bytes, &mut len, &mut cursor, "ab");
    cursor = Cursor::at(1);
    insert(&mut bytes, &mut len, &mut cursor, "éé");
    assert_eq!(core::str::from_utf8(&bytes[..len]), Ok("aééb"));
    assert_eq!(cursor, Cursor::at(5));
    assert!(!insert(&mut bytes, &mut len, &mut cursor, "z"));
}

#[test]
fn insert_drops_control_chars() {
    let mut buf = Buf::new("");
    insert(
        &mut buf.bytes,
        &mut buf.len,
        &mut buf.cursor,
        "a\nb\tc\u{7f}",
    );
    assert_eq!(buf.text(), "abc");
}

#[test]
fn typed_text_applies_before_the_keys_of_the_same_frame() {
    let mut buf = Buf::new("");
    let mut input = key_input(keys::BACKSPACE, Mods::default());
    input.push_text("ab");
    buf.run(&input);
    assert_eq!(buf.text(), "a");
}

#[test]
fn return_submits_and_escape_cancels() {
    let mut buf = Buf::new("x");
    let out = buf.plain(keys::RETURN);
    assert!(out.submitted && !out.cancelled && !out.changed);
    assert!(buf.plain(KP_ENTER).submitted);
    let out = buf.plain(keys::ESCAPE);
    assert!(out.cancelled && !out.submitted);
    // Releases do nothing.
    let mut input = Input::default();
    input.push_key(KeyEvent {
        sym: keys::RETURN,
        pressed: false,
        repeat: false,
        mods: Mods::default(),
    });
    assert_eq!(buf.run(&input), Edit::default());
}

#[test]
fn a_stale_cursor_is_snapped_into_the_text() {
    let bytes = MIXED.as_bytes();
    let stale = Cursor {
        caret: 99,
        anchor: 2,
    };
    assert_eq!(
        stale.snapped(bytes),
        Cursor {
            caret: 7,
            anchor: 1
        }
    );
    // Inside 漢 snaps back to its start.
    assert_eq!(Cursor::at(5).snapped(bytes), Cursor::at(3));
}

#[test]
fn caret_at_picks_the_nearest_boundary() {
    let stops = [0.0, 8.0, 16.0, 24.0];
    assert_eq!(caret_at(&stops, -5.0), 0);
    assert_eq!(caret_at(&stops, 0.0), 0);
    assert_eq!(caret_at(&stops, 3.9), 0);
    // A tie goes to the later boundary.
    assert_eq!(caret_at(&stops, 4.0), 1);
    assert_eq!(caret_at(&stops, 4.1), 1);
    assert_eq!(caret_at(&stops, 11.9), 1);
    assert_eq!(caret_at(&stops, 12.1), 2);
    assert_eq!(caret_at(&stops, 23.0), 3);
    assert_eq!(caret_at(&stops, 24.0), 3);
    assert_eq!(caret_at(&stops, 1000.0), 3);
    assert_eq!(caret_at(&[], 5.0), 0);
    assert_eq!(caret_at(&[0.0], 5.0), 0);
    assert_eq!(caret_at(&stops, f32::NAN), 0);
}

#[test]
fn caret_at_handles_uneven_widths() {
    // A narrow char, then a wide one.
    let stops = [0.0, 4.0, 20.0];
    assert_eq!(caret_at(&stops, 11.0), 1);
    assert_eq!(caret_at(&stops, 13.0), 2);
}

#[test]
fn boundaries_and_bytes_convert_both_ways() {
    let bytes = MIXED.as_bytes();
    let pairs = [(0, 0), (1, 1), (2, 3), (3, 6), (4, 7)];
    for (boundary, byte) in pairs {
        assert_eq!(
            boundary_to_byte(bytes, boundary),
            byte,
            "boundary {boundary}"
        );
        assert_eq!(byte_to_boundary(bytes, byte), boundary, "byte {byte}");
    }
    assert_eq!(boundary_to_byte(bytes, 99), 7);
    assert_eq!(byte_to_boundary(bytes, 99), 4);
    // Mid char rounds down to the char's start.
    assert_eq!(byte_to_boundary(bytes, 4), 2);
    assert_eq!(boundary_to_byte(b"", 0), 0);
}

#[test]
fn reveal_scrolls_the_least_to_show_the_caret() {
    // Fits already.
    assert_eq!(reveal(0.0, 50.0, 200.0, 100.0, 1.0), 0.0);
    // Past the right edge.
    assert_eq!(reveal(0.0, 150.0, 200.0, 100.0, 1.0), 51.0);
    // Left of the view.
    assert_eq!(reveal(80.0, 30.0, 200.0, 100.0, 1.0), 30.0);
    // Text shrank under the scroll.
    assert_eq!(reveal(80.0, 40.0, 50.0, 100.0, 1.0), 0.0);
    // No view yet.
    assert_eq!(reveal(7.0, 150.0, 200.0, 0.0, 1.0), 7.0);
}

// Ui level, over the same stack harness as the crate tests.

const N: usize = 64;
const S: usize = 32;
const Q: usize = 64;
const FKEY: u64 = 0xf1e1d;
const ADVANCE: f32 = 8.0;
const VIEW: [f32; 2] = [200.0, 100.0];

macro_rules! ui {
    ($u:ident) => {
        let mut lkey = [0u64; N];
        let mut lparent = [0u32; N];
        let mut lsubtree = [0u32; N];
        let mut lsize = [[SizeSpec::px(0.0); 2]; N];
        let mut laxis = [Axis::X; N];
        let mut lmeasured = [[0.0f32; 2]; N];
        let mut lpad = [Insets::ZERO; N];
        let mut lopts = [Options::default(); N];
        let mut lcomputed = [[0.0f32; 2]; N];
        let mut lcontent = [[0.0f32; 2]; N];
        let mut lrect = [Rect::default(); N];
        let mut lclip = [Rect::default(); N];
        let mut lstack = [0u32; 32];
        let mut skeys = [0u64; S];
        let mut svalues: [Option<WidgetState>; S] = [const { None }; S];
        let mut stouched = [0u32; S];
        let mut quads = [Quad::solid(Rect::default(), Rect::default(), 0); Q];
        let mut prev = quads;
        let mut custom = [CustomRegion {
            key: 0,
            rect: Rect::default(),
            clip: Rect::default(),
        }; 2];
        let mut kstack = [0u64; 32];
        let mut fill = [0u32; N];
        let mut shape = [Shape::NONE; N];
        let storage = UiStorage {
            layout: LayoutStorage {
                key: &mut lkey,
                parent: &mut lparent,
                subtree_end: &mut lsubtree,
                size: &mut lsize,
                axis: &mut laxis,
                measured: &mut lmeasured,
                pad: &mut lpad,
                opts: &mut lopts,
                computed: &mut lcomputed,
                content: &mut lcontent,
                rect: &mut lrect,
                clip: &mut lclip,
                stack: &mut lstack,
            },
            state_keys: &mut skeys,
            state_values: &mut svalues,
            state_touched: &mut stouched,
            quads: &mut quads,
            prev_quads: &mut prev,
            custom: &mut custom,
            key_stack: &mut kstack,
            fill: &mut fill,
            shape: &mut shape,
        };
        let mut $u = Ui::new(storage).expect("storage agrees");
    };
}

fn style() -> FieldStyle {
    FieldStyle {
        fill: 0xff30_3030,
        focused_fill: 0xff40_4040,
        border: 0xff10_1010,
        focused_border: 0xff20_20f0,
        caret: 0xffff_ffff,
        selection: 0x8020_20f0,
        border_width: 1.0,
        radius: 4.0,
        pad: 6.0,
        height: 26.0,
        caret_width: 1.0,
    }
}

// The field is the root, so its content box spans x 7..153, y 7..19.
const CONTENT_X: f32 = 7.0;
const MID_Y: f32 = 13.0;

struct Doc {
    bytes: [u8; 64],
    len: usize,
}

impl Doc {
    fn new(text: &str) -> Doc {
        let mut bytes = [0u8; 64];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        Doc {
            bytes,
            len: text.len(),
        }
    }
    fn text(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).expect("valid utf8")
    }
    /// Monospaced stops for the current text.
    fn stops(&self) -> Vec<f32> {
        let chars = self.text().chars().count();
        (0..=chars).map(|index| index as f32 * ADVANCE).collect()
    }
}

fn pointer(x: f32, y: f32, down: bool) -> Input {
    Input {
        pointer: [x, y],
        pointer_down: down,
        dt: 1.0 / 60.0,
        ..Input::default()
    }
}

fn idle() -> Input {
    pointer(-1.0, -1.0, false)
}

fn frame(u: &mut Ui, input: Input, doc: &mut Doc) -> (Field, Vec<Quad>) {
    let stops = doc.stops();
    u.begin_frame(input, VIEW);
    let out = field(
        u,
        FKEY,
        SizeSpec::px(160.0),
        &mut doc.bytes,
        &mut doc.len,
        &stops,
        style(),
    )
    .expect("field");
    u.end_frame().expect("balanced");
    let quads = u.quads().to_vec();
    u.finish();
    (out, quads)
}

fn settle(u: &mut Ui, doc: &mut Doc) -> Field {
    frame(u, idle(), doc);
    frame(u, idle(), doc).0
}

fn quad_of(quads: &[Quad], color: u32) -> Quad {
    *quads
        .iter()
        .find(|quad| quad.color == color)
        .unwrap_or_else(|| panic!("no quad in {color:08x}"))
}

/// Press at boundary x, then release.
fn click(u: &mut Ui, doc: &mut Doc, x: f32) -> Field {
    frame(u, pointer(x, MID_Y, true), doc);
    frame(u, pointer(x, MID_Y, false), doc).0
}

#[test]
fn clicking_focuses_and_typing_edits_at_the_caret() {
    ui!(u);
    let mut doc = Doc::new("abc");
    let out = settle(&mut u, &mut doc);
    assert!(!out.focused);
    assert_eq!(
        out.content,
        Rect {
            min: [7.0, 7.0],
            max: [153.0, 19.0]
        }
    );
    // Nine pixels in is nearest boundary 1.
    let (out, _) = frame(&mut u, pointer(CONTENT_X + 9.0, MID_Y, true), &mut doc);
    assert!(out.focused && out.response.pressed && out.moved && !out.changed);
    assert_eq!(out.cursor, Cursor::at(1));
    assert_eq!(u.focused(), Some(FKEY));

    let mut input = pointer(CONTENT_X + 9.0, MID_Y, false);
    input.push_text("é");
    let (out, quads) = frame(&mut u, input, &mut doc);
    assert!(out.changed && out.moved && out.focused);
    assert_eq!(out.cursor, Cursor::at(3));
    assert_eq!(doc.text(), "aébc");

    let border = quad_of(&quads, style().focused_border);
    assert_eq!(border.shape, Shape::round(4));
    assert_eq!(border.rect.min, [0.0, 0.0]);
    let fill = quad_of(&quads, style().focused_fill);
    assert_eq!(fill.shape, Shape::round(3));
    assert_eq!(fill.rect.min, [1.0, 1.0]);
    // The content box paints nothing, the caller owns it.
    assert_eq!(quads.len(), 2);
}

#[test]
fn an_unfocused_field_ignores_keys_and_paints_the_idle_colours() {
    ui!(u);
    let mut doc = Doc::new("abc");
    settle(&mut u, &mut doc);
    let mut input = idle();
    input.push_text("zz");
    let (out, quads) = frame(&mut u, input, &mut doc);
    assert!(!out.changed && !out.focused);
    assert_eq!(doc.text(), "abc");
    assert_eq!(quad_of(&quads, style().border).shape, Shape::round(4));
    assert_eq!(quad_of(&quads, style().fill).shape, Shape::round(3));
}

#[test]
fn a_press_elsewhere_drops_focus() {
    ui!(u);
    let mut doc = Doc::new("abc");
    settle(&mut u, &mut doc);
    click(&mut u, &mut doc, CONTENT_X);
    assert_eq!(u.focused(), Some(FKEY));
    let (out, _) = frame(&mut u, pointer(180.0, 60.0, true), &mut doc);
    assert!(!out.focused);
    assert_eq!(u.focused(), None);
    // Holding the press out there does not refocus or blur again.
    let (out, _) = frame(&mut u, pointer(180.0, 60.0, false), &mut doc);
    assert!(!out.focused);
}

#[test]
fn a_press_on_the_border_also_focuses() {
    ui!(u);
    let mut doc = Doc::new("abc");
    settle(&mut u, &mut doc);
    let (out, _) = frame(&mut u, pointer(0.5, MID_Y, true), &mut doc);
    assert!(out.focused);
    assert_eq!(out.cursor, Cursor::at(0));
}

#[test]
fn dragging_extends_the_selection_and_shift_click_keeps_the_anchor() {
    ui!(u);
    let mut doc = Doc::new("abcdef");
    settle(&mut u, &mut doc);
    frame(&mut u, pointer(CONTENT_X + 1.0, MID_Y, true), &mut doc);
    let (out, _) = frame(&mut u, pointer(CONTENT_X + 17.0, MID_Y, true), &mut doc);
    assert_eq!(
        out.cursor,
        Cursor {
            caret: 2,
            anchor: 0
        }
    );
    // Dragging off the end clamps to the last boundary.
    let (out, _) = frame(&mut u, pointer(195.0, 90.0, true), &mut doc);
    assert_eq!(out.cursor.caret, 6);
    assert_eq!(out.cursor.anchor, 0);
    frame(&mut u, pointer(195.0, 90.0, false), &mut doc);

    let mut input = pointer(CONTENT_X + 24.0, MID_Y, true);
    input.mods.shift = true;
    let (out, _) = frame(&mut u, input, &mut doc);
    assert_eq!(
        out.cursor,
        Cursor {
            caret: 3,
            anchor: 0
        }
    );
}

#[test]
fn the_scroll_follows_the_caret() {
    ui!(u);
    // 30 chars is 240 px against a 146 px content box.
    let mut doc = Doc::new("abcdefghijklmnopqrstuvwxyz0123");
    settle(&mut u, &mut doc);
    click(&mut u, &mut doc, CONTENT_X);
    let mut input = idle();
    input.push_key(KeyEvent {
        sym: keys::END,
        pressed: true,
        repeat: false,
        mods: Mods::default(),
    });
    let (out, _) = frame(&mut u, input, &mut doc);
    assert_eq!(out.cursor, Cursor::at(30));
    assert_eq!(out.scroll, 240.0 + 1.0 - 146.0);
    // A click maps through the scroll: the left edge is now x 95.
    let out = click(&mut u, &mut doc, CONTENT_X + 1.0);
    assert_eq!(out.cursor.caret, 12);
    let mut input = idle();
    input.push_key(KeyEvent {
        sym: keys::HOME,
        pressed: true,
        repeat: false,
        mods: Mods::default(),
    });
    let (out, _) = frame(&mut u, input, &mut doc);
    assert_eq!(out.scroll, 0.0);
}

#[test]
fn submit_and_cancel_reach_the_widget_result() {
    ui!(u);
    let mut doc = Doc::new("go");
    settle(&mut u, &mut doc);
    click(&mut u, &mut doc, CONTENT_X);
    let mut input = idle();
    input.push_key(KeyEvent {
        sym: keys::RETURN,
        pressed: true,
        repeat: false,
        mods: Mods::default(),
    });
    let (out, _) = frame(&mut u, input, &mut doc);
    assert!(out.submitted && !out.cancelled);
    let mut input = idle();
    input.push_key(KeyEvent {
        sym: keys::ESCAPE,
        pressed: true,
        repeat: false,
        mods: Mods::default(),
    });
    let (out, _) = frame(&mut u, input, &mut doc);
    assert!(out.cancelled && !out.submitted);
}

#[test]
fn bad_arguments_are_errors() {
    ui!(u);
    let mut bytes = [0u8; 4];
    let mut len = 5;
    u.begin_frame(idle(), VIEW);
    let err = field(
        &mut u,
        FKEY,
        SizeSpec::px(100.0),
        &mut bytes,
        &mut len,
        &[0.0],
        style(),
    );
    assert_eq!(err, Err(WidgetError::FieldLengthOutOfRange));
    len = 0;
    let flat = FieldStyle {
        height: 0.0,
        ..style()
    };
    let err = field(
        &mut u,
        FKEY,
        SizeSpec::px(100.0),
        &mut bytes,
        &mut len,
        &[0.0],
        flat,
    );
    assert_eq!(err, Err(WidgetError::FieldHeightNotPositive));
    let negative = FieldStyle {
        pad: -1.0,
        ..style()
    };
    let err = field(
        &mut u,
        FKEY,
        SizeSpec::px(100.0),
        &mut bytes,
        &mut len,
        &[0.0],
        negative,
    );
    assert_eq!(err, Err(WidgetError::FieldInsetNegative));
}
