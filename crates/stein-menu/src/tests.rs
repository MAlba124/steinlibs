#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use stein_layout::Align;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Cmd {
    #[default]
    Nothing,
    Open,
    Copy,
    Paste,
    Sort,
    Name,
    Size,
    Gone,
}

const VIEW: [f32; 2] = [800.0, 600.0];

fn m() -> Metrics {
    Metrics::new(20.0, 8.0)
}

/// Every character 8 pixels, like a grid font.
fn measure(s: &str) -> f32 {
    s.chars().count() as f32 * 8.0
}

/// Open, Copy (Ctrl+C), -, Paste disabled, Sort by >, Gone.
fn menu() -> Menu<Cmd> {
    let mut menu = Menu::new();
    let l = menu.open();
    l.item("Open", "Enter", Cmd::Open);
    l.item("Copy", "Ctrl+C", Cmd::Copy);
    l.sep();
    l.item("Paste", "", Cmd::Paste).disabled();
    l.sub("Sort by", Cmd::Sort);
    l.item("Gone", "", Cmd::Gone);
    menu.place([100.0, 100.0], VIEW, &m(), &mut measure);
    menu
}

/// Fill the Sort by submenu the way an app answers `OpenSub`.
fn open_sort(menu: &mut Menu<Cmd>) {
    let l = menu.open_sub(0, 4).unwrap();
    l.radio("Name", Cmd::Name, true);
    l.radio("Size", Cmd::Size, false);
    menu.place_sub(VIEW, &m(), &mut measure);
}

fn centre(r: Rect) -> [f32; 2] {
    [(r.min[0] + r.max[0]) * 0.5, (r.min[1] + r.max[1]) * 0.5]
}

#[test]
fn rows_keep_their_text_and_kinds() {
    let menu = menu();
    let l = &menu.levels()[0];
    assert_eq!(l.len(), 6);
    assert_eq!(l.label(1), "Copy");
    assert_eq!(l.hint(1), "Ctrl+C");
    assert_eq!(l.rows()[2].kind, Kind::Sep);
    assert!(!l.rows()[3].live(), "a disabled row is not live");
    assert_eq!(l.rows()[4].kind, Kind::Sub);
}

#[test]
fn the_cursor_skips_separators_and_disabled_rows() {
    let mut menu = menu();
    let order: Vec<Option<usize>> = (0..6)
        .map(|_| {
            menu.key(keys::DOWN);
            menu.levels()[0].at
        })
        .collect();
    assert_eq!(
        order,
        [Some(0), Some(1), Some(4), Some(5), Some(5), Some(5)]
    );
    menu.wrap = true;
    menu.key(keys::DOWN);
    assert_eq!(menu.levels()[0].at, Some(0), "did not wrap");
    menu.key(keys::UP);
    assert_eq!(menu.levels()[0].at, Some(5));
    menu.key(keys::HOME);
    assert_eq!(menu.levels()[0].at, Some(0));
    menu.key(keys::END);
    assert_eq!(menu.levels()[0].at, Some(5));
}

#[test]
fn a_level_of_nothing_live_has_no_cursor() {
    let mut menu: Menu<Cmd> = Menu::new();
    let l = menu.open();
    l.sep();
    l.item("off", "", Cmd::Open).disabled();
    menu.key(keys::DOWN);
    assert_eq!(menu.levels()[0].at, None);
}

#[test]
fn enter_runs_an_item_and_asks_for_a_submenu() {
    let mut menu = menu();
    menu.key(keys::DOWN);
    assert_eq!(menu.key(keys::RETURN), Outcome::Run(Cmd::Open));
    menu.key(keys::DOWN);
    assert_eq!(
        menu.key(keys::RIGHT),
        Outcome::Stay,
        "Right on an item runs it"
    );
    menu.key(keys::DOWN);
    let want = Outcome::OpenSub {
        level: 0,
        row: 4,
        cmd: Cmd::Sort,
    };
    assert_eq!(menu.key(keys::RIGHT), want);
    assert_eq!(menu.key(keys::RETURN), want);
}

#[test]
fn keys_go_into_a_submenu_and_back_out() {
    let mut menu = menu();
    open_sort(&mut menu);
    menu.enter_sub();
    assert_eq!(menu.focus(), 1);
    assert_eq!(menu.levels()[1].at, Some(0));
    assert_eq!(menu.lit(0), Some(4), "the parent row stays lit");
    menu.key(keys::DOWN);
    assert_eq!(menu.key(keys::RETURN), Outcome::Run(Cmd::Size));
    assert_eq!(menu.key(keys::LEFT), Outcome::Stay);
    assert_eq!(menu.focus(), 0);
    assert_eq!(menu.levels().len(), 1, "the submenu stayed open");
    assert_eq!(menu.key(keys::ESCAPE), Outcome::Close);
}

#[test]
fn hovering_a_submenu_row_asks_for_it_once() {
    let (m, mut menu) = (m(), menu());
    let sort = centre(menu.levels()[0].row_rect(4, &m));
    let asked = menu.hover(sort, &m);
    assert!(matches!(asked, Outcome::OpenSub { row: 4, .. }));
    open_sort(&mut menu);
    assert_eq!(
        menu.hover(sort, &m),
        Outcome::Stay,
        "asked again while open"
    );
    // Onto the submenu: it takes the cursor and the parent stays lit.
    let size = centre(menu.levels()[1].row_rect(1, &m));
    menu.hover(size, &m);
    assert_eq!((menu.focus(), menu.levels()[1].at), (1, Some(1)));
    assert_eq!(menu.lit(0), Some(4));
    // Back onto an item of the root: the submenu closes.
    let open = centre(menu.levels()[0].row_rect(0, &m));
    menu.hover(open, &m);
    assert_eq!(menu.levels().len(), 1);
    assert_eq!(menu.lit(0), Some(0));
}

#[test]
fn presses_choose_ignore_or_close() {
    let (m, mut menu) = (m(), menu());
    let l = menu.levels()[0];
    assert_eq!(
        menu.press(centre(l.row_rect(1, &m)), &m),
        Outcome::Run(Cmd::Copy)
    );
    assert_eq!(
        menu.press(centre(l.row_rect(2, &m)), &m),
        Outcome::Stay,
        "a separator"
    );
    assert_eq!(
        menu.press(centre(l.row_rect(3, &m)), &m),
        Outcome::Stay,
        "disabled"
    );
    assert_eq!(menu.press([5.0, 5.0], &m), Outcome::Close);
    open_sort(&mut menu);
    let size = centre(menu.levels()[1].row_rect(1, &m));
    assert_eq!(menu.press(size, &m), Outcome::Run(Cmd::Size));
}

#[test]
fn separators_are_shorter_and_never_hit() {
    let (m, menu) = (m(), menu());
    let l = &menu.levels()[0];
    let sep = l.row_rect(2, &m);
    assert_eq!(sep.max[1] - sep.min[1], 10.0);
    assert_eq!(l.row_at(centre(sep), &m), None);
    let rows: Vec<Rect> = (0..l.len()).map(|i| l.row_rect(i, &m)).collect();
    for w in rows.windows(2) {
        assert_eq!(w[0].max[1], w[1].min[1], "rows do not tile");
    }
    assert_eq!(l.rect.max[1], rows[5].max[1] + m.pad);
}

#[test]
fn the_width_fits_label_hint_and_arrow() {
    let (m, menu) = (m(), menu());
    let l = &menu.levels()[0];
    let [w, _] = l.size(&m, &mut measure);
    let widest = 2.0 * m.pad + m.mark + measure("Sort by") + m.gap + measure("Ctrl+C") + m.end;
    assert_eq!(w, widest);
    let mut clamped = m;
    clamped.min_w = 400.0;
    assert_eq!(l.size(&clamped, &mut measure)[0], 400.0);
}

#[test]
fn placement_stays_inside_the_view_from_every_corner() {
    for at in [[0.0, 0.0], [799.0, 0.0], [0.0, 599.0], [799.0, 599.0]] {
        let mut menu = menu();
        menu.place(at, VIEW, &m(), &mut measure);
        open_sort(&mut menu);
        for l in menu.levels() {
            let r = l.rect;
            assert!(r.min[0] >= 0.0 && r.min[1] >= 0.0, "{at:?} {r:?}");
            assert!(r.max[0] <= VIEW[0] && r.max[1] <= VIEW[1], "{at:?} {r:?}");
        }
    }
}

#[test]
fn a_submenu_opens_left_when_the_right_is_full() {
    let mut menu = menu();
    open_sort(&mut menu);
    assert!(
        menu.levels()[1].rect.min[0] > menu.levels()[0].rect.min[0],
        "right of it"
    );
    // The root fits at 620, its submenu does not fit to its right.
    menu.place([620.0, 100.0], VIEW, &m(), &mut measure);
    assert_eq!(menu.levels()[0].rect.min[0], 620.0);
    open_sort(&mut menu);
    let (root, sub) = (menu.levels()[0].rect, menu.levels()[1].rect);
    assert!(sub.max[0] <= root.min[0] + m().pad, "{root:?} {sub:?}");
}

#[test]
fn text_past_the_table_is_cut_at_a_character() {
    let mut menu: Menu<Cmd> = Menu::new();
    let l = menu.open();
    let long = "\u{e9}".repeat(TEXT);
    l.item(&long, "", Cmd::Open);
    assert_eq!(l.label(0).len(), TEXT, "not cut to the table");
    l.item("more", "hint", Cmd::Copy);
    assert_eq!((l.label(1), l.hint(1)), ("", ""));
}

#[test]
fn rows_past_the_table_are_dropped() {
    let mut menu: Menu<Cmd> = Menu::new();
    let l = menu.open();
    for _ in 0..ROWS + 5 {
        l.item("x", "", Cmd::Open);
    }
    assert_eq!(l.len(), ROWS);
}

#[test]
fn painting_draws_boxes_lit_rows_marks_and_rules() {
    let (m, mut menu) = (m(), menu());
    menu.key(keys::DOWN);
    menu.key(keys::DOWN);
    open_sort(&mut menu);
    let style = Style {
        shadow: 1,
        border: 2,
        bg: 3,
        lit: 4,
        text: 5,
        hint: 6,
        hint_lit: 7,
        disabled: 8,
        mark: 9,
        rule: 10,
    };
    let mut rects = Vec::new();
    let mut texts = Vec::new();
    menu.paint(&m, &style, &mut |p| match p {
        Paint::Rect { colour, .. } => rects.push(colour),
        Paint::Text {
            text,
            colour,
            align,
            ..
        } => texts.push((text.to_string(), colour, align)),
    });
    // Two boxes of three, one rule, the lit Copy row and nothing else lit.
    assert_eq!(rects.iter().filter(|&&c| c == 3).count(), 2);
    assert_eq!(rects.iter().filter(|&&c| c == 10).count(), 1);
    assert_eq!(rects.iter().filter(|&&c| c == 4).count(), 1);
    assert!(
        texts.contains(&("Ctrl+C".into(), 7, Align::End)),
        "{texts:?}"
    );
    assert!(texts.contains(&("Enter".into(), 6, Align::End)));
    assert!(
        texts.contains(&("Paste".into(), 8, Align::Start)),
        "disabled"
    );
    assert!(texts.contains(&("\u{203a}".into(), 5, Align::End)));
    assert!(
        texts.contains(&("\u{2022}".into(), 9, Align::Center)),
        "the radio"
    );
    assert!(!texts.iter().any(|t| t.0.is_empty()));
}

/// A pointer resting on a row does not take the cursor back from the
/// keys; moving it does.
#[test]
fn a_resting_pointer_leaves_the_keys_alone() {
    let (m, mut menu) = (m(), menu());
    let copy = centre(menu.levels()[0].row_rect(1, &m));
    menu.hover(copy, &m);
    assert_eq!(menu.levels()[0].at, Some(1));
    menu.key(keys::DOWN);
    assert_eq!(menu.levels()[0].at, Some(4));
    menu.hover(copy, &m);
    assert_eq!(
        menu.levels()[0].at,
        Some(4),
        "the resting pointer took it back"
    );
    menu.hover([copy[0] + 1.0, copy[1]], &m);
    assert_eq!(menu.levels()[0].at, Some(1));
}

/// Levels on surfaces of their own: each at the origin of its space, the
/// pointer handed to one level at a time.
#[test]
fn levels_on_their_own_surfaces_take_the_pointer_one_at_a_time() {
    let (m, mut menu) = (m(), menu());
    let size = menu.levels()[0].rect;
    let origin = |r: Rect| Rect {
        min: [0.0, 0.0],
        max: [r.max[0] - r.min[0], r.max[1] - r.min[1]],
    };
    menu.level_mut(0).unwrap().rect = origin(size);
    let sort = centre(menu.levels()[0].row_rect(4, &m));
    assert!(matches!(
        menu.hover_in(0, sort, &m),
        Outcome::OpenSub { row: 4, .. }
    ));
    open_sort(&mut menu);
    let sub = menu.levels()[1].rect;
    menu.level_mut(1).unwrap().rect = origin(sub);
    // The same local point on the submenu's surface is its own row.
    let size_row = centre(menu.levels()[1].row_rect(1, &m));
    menu.hover_in(1, size_row, &m);
    assert_eq!((menu.focus(), menu.levels()[1].at), (1, Some(1)));
    assert_eq!(menu.press_in(1, size_row, &m), Outcome::Run(Cmd::Size));
    assert_eq!(
        menu.press_in(0, sort, &m),
        Outcome::OpenSub {
            level: 0,
            row: 4,
            cmd: Cmd::Sort
        }
    );
    assert_eq!(
        menu.press_in(5, sort, &m),
        Outcome::Close,
        "a level that is not open"
    );
}
