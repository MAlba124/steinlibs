//! Whole gallery frames, headless. Skipped when fontconfig finds no font.

use super::*;
use stein_ui::{Button, ButtonEvent, KeyEvent, Mods};

const VIEW: [f32; 2] = [1280.0, 860.0];

fn gallery() -> Option<Gallery> {
    let sans = text::find_font("UI_GALLERY_SANS", &["sans-serif"])?;
    let mono = text::find_font("UI_GALLERY_MONO", &["monospace"])?;
    let bump: &'static Bump = Box::leak(Box::new(Bump::reserve(ARENA).ok()?));
    Gallery::new(bump, Text::load(sans, mono).ok()?).ok()
}

fn idle(pointer: [f32; 2]) -> Input {
    Input {
        pointer,
        pointer_in: true,
        focused: true,
        dt: 1.0 / 60.0,
        ..Input::default()
    }
}

fn frames(g: &mut Gallery, n: usize, pointer: [f32; 2]) {
    for _ in 0..n {
        g.frame(idle(pointer), VIEW).expect("frame");
    }
}

fn click(g: &mut Gallery, at: [f32; 2]) {
    frames(g, 1, at);
    for pressed in [true, false] {
        let mut input = idle(at);
        input.pointer_down = pressed;
        input.push_button(ButtonEvent {
            button: Button::Left,
            pressed,
            at: [at[0] as i32, at[1] as i32],
            mods: Mods::default(),
        });
        g.frame(input, VIEW).expect("frame");
    }
}

/// The centre of the nav item for `page`, from the sidebar's layout:
/// 16 padding, a 50 high brand row, then 40 high rows 4 apart.
fn nav(page: Page) -> [f32; 2] {
    let i = Page::ALL.iter().position(|p| *p == page).unwrap_or(0) as f32;
    [100.0, 8.0 + 16.0 + 50.0 + 4.0 + i * 44.0 + 20.0]
}

#[test]
fn every_page_builds_and_draws() {
    let Some(mut g) = gallery() else { return };
    for page in Page::ALL {
        g.demo.page = page;
        frames(&mut g, 3, [-1.0, -1.0]);
        let quads = g.quads();
        assert!(quads.len() > 40, "{page:?} drew {} quads", quads.len());
        assert!(quads.iter().any(|q| q.is_textured()), "{page:?} has text");
        assert!(quads.iter().any(|q| !q.shape.is_none()), "{page:?} has rounded boxes");
    }
}

#[test]
fn the_shapes_page_covers_strokes_pills_and_single_corners() {
    let Some(mut g) = gallery() else { return };
    frames(&mut g, 3, [-1.0, -1.0]);
    let shapes: Vec<_> = g.quads().iter().map(|q| q.shape).collect();
    assert!(shapes.iter().any(|s| s.stroke > 0), "a ring");
    assert!(shapes.contains(&Shape::PILL), "a pill");
    assert!(shapes.contains(&Shape::corners([0, 0, 0, 36])), "one corner");
}

#[test]
fn clicking_the_sidebar_moves_between_pages() {
    let Some(mut g) = gallery() else { return };
    frames(&mut g, 3, [-1.0, -1.0]);
    for page in [Page::Type, Page::List, Page::Shapes, Page::Input] {
        click(&mut g, nav(page));
        assert_eq!(g.demo.page, page);
    }
}

#[test]
fn an_idle_static_page_stops_presenting() {
    let Some(mut g) = gallery() else { return };
    g.demo.page = Page::Type;
    frames(&mut g, 120, [-1.0, -1.0]);
    assert!(!g.needs_frame(), "nothing moves, so the window can sleep");
}

#[test]
fn the_motion_page_keeps_animating_until_paused() {
    let Some(mut g) = gallery() else { return };
    g.demo.page = Page::Motion;
    frames(&mut g, 3, [-1.0, -1.0]);
    let before: Vec<_> = g.quads().to_vec();
    frames(&mut g, 10, [-1.0, -1.0]);
    assert!(g.needs_frame());
    assert_ne!(before, g.quads(), "the frame moved");
    g.demo.playing = false;
    frames(&mut g, 120, [-1.0, -1.0]);
    let still: Vec<_> = g.quads().to_vec();
    frames(&mut g, 5, [-1.0, -1.0]);
    assert_eq!(still, g.quads(), "paused is still");
    assert!(!g.needs_frame());
}

#[test]
fn typing_into_the_field_edits_its_text() {
    let Some(mut g) = gallery() else { return };
    g.demo.page = Page::Input;
    frames(&mut g, 3, [-1.0, -1.0]);
    // The field sits under the heading and the section label.
    click(&mut g, [400.0, 147.0]);
    let mut input = idle([400.0, 147.0]);
    let ctrl = Mods {
        ctrl: true,
        ..Mods::default()
    };
    input.mods = ctrl;
    input.push_key(KeyEvent {
        sym: stein_ui::keys::A,
        pressed: true,
        repeat: false,
        mods: ctrl,
    });
    g.frame(input, VIEW).expect("frame");
    let mut input = idle([400.0, 147.0]);
    input.push_text("Nora Vik");
    g.frame(input, VIEW).expect("frame");
    assert_eq!(g.demo.field_text(), "Nora Vik");
    let caret = g.demo.caret.expect("the field reports its caret");
    assert!(caret.focused);
    assert_eq!(caret.caret, "Nora Vik".len());
}

#[test]
fn the_switch_and_counter_answer_clicks() {
    let Some(mut g) = gallery() else { return };
    g.demo.page = Page::Input;
    frames(&mut g, 3, [-1.0, -1.0]);
    let on = g.demo.toggle;
    // The button and switch row, under the field section.
    click(&mut g, [314.0, 265.0]);
    assert_eq!(g.demo.clicks, 1);
    click(&mut g, [521.0, 265.0]);
    assert_eq!(g.demo.toggle, !on);
}

#[test]
fn a_shot_script_parses_and_rejects_nonsense() {
    let steps = shot::parse("click:10,20,type:hi,key:ctrl+a,key:shift+left,wait:3").expect("parses");
    assert_eq!(steps.len(), 5);
    assert_eq!(steps[0], shot::Step::Click([10.0, 20.0]));
    assert!(shot::parse("click:10").is_err());
    assert!(shot::parse("jump:1").is_err());
    assert!(shot::parse("key:hyper+q").is_err());
}

#[test]
fn pressing_the_slider_moves_it_to_the_pointer() {
    let Some(mut g) = gallery() else { return };
    g.demo.page = Page::Input;
    frames(&mut g, 3, [-1.0, -1.0]);
    // The track runs from x 264 for 520 pixels.
    click(&mut g, [264.0 + 130.0, 348.0]);
    assert!((g.demo.slider - 0.25).abs() < 0.01, "{}", g.demo.slider);
}
