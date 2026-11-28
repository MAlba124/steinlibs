//! Headless runs: scripted input, then the last frame as a PNG.
//!
//! ```text
//! ui-gallery --shot out.png --page input --size 1280x800 --do click:300,200,type:hello,key:left,wait:30
//! ```
//!
//! Every step is one frame. `wait:N` builds N frames with no input, which
//! is how animations are caught mid way and hovers settle. `wheel:N` turns
//! the wheel N notches down, negative for up, over the pointer.

use stein_shell::paint::{Atlas, Images, clear_region, paint};
use stein_ui::{Button, ButtonEvent, Input, KeyEvent, Mods, keys};

use crate::{Gallery, GalleryError, palette};

/// One scripted frame.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Move([f32; 2]),
    Click([f32; 2]),
    Type(String),
    Key(u32, Mods),
    Wait(u32),
    Wheel(f32),
}

#[derive(Debug)]
pub enum ShotError {
    BadStep(String),
    Frame(GalleryError),
    Write(std::io::Error),
}

impl std::fmt::Display for ShotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShotError::BadStep(s) => write!(f, "bad step {s:?}, want move:X,Y click:X,Y type:TEXT key:NAME wait:N wheel:N"),
            ShotError::Frame(e) => write!(f, "frame: {e}"),
            ShotError::Write(e) => write!(f, "write: {e}"),
        }
    }
}

impl From<GalleryError> for ShotError {
    fn from(e: GalleryError) -> Self {
        ShotError::Frame(e)
    }
}

/// Steps from a comma separated list. A `click:` or `move:` takes the next
/// item as its y.
pub fn parse(script: &str) -> Result<Vec<Step>, ShotError> {
    let mut steps = Vec::new();
    let mut items = script.split(',').filter(|s| !s.is_empty());
    while let Some(item) = items.next() {
        let bad = || ShotError::BadStep(item.to_string());
        let (verb, arg) = item.split_once(':').ok_or_else(bad)?;
        let mut point = || -> Result<[f32; 2], ShotError> {
            let x = arg.parse().map_err(|_| bad())?;
            let y = items.next().and_then(|y| y.parse().ok()).ok_or_else(bad)?;
            Ok([x, y])
        };
        steps.push(match verb {
            "move" => Step::Move(point()?),
            "click" => Step::Click(point()?),
            "type" => Step::Type(arg.to_string()),
            "wait" => Step::Wait(arg.parse().map_err(|_| bad())?),
            "wheel" => Step::Wheel(arg.parse().map_err(|_| bad())?),
            "key" => key_step(arg).ok_or_else(bad)?,
            _ => return Err(bad()),
        });
    }
    Ok(steps)
}

fn key_step(name: &str) -> Option<Step> {
    let (ctrl, shift, base) = match name.split_once('+') {
        Some(("ctrl", rest)) => (true, false, rest),
        Some(("shift", rest)) => (false, true, rest),
        _ => (false, false, name),
    };
    let sym = match base {
        "left" => keys::LEFT,
        "right" => keys::RIGHT,
        "home" => keys::HOME,
        "end" => keys::END,
        "back" => keys::BACKSPACE,
        "delete" => keys::DELETE,
        "enter" => keys::RETURN,
        "esc" => keys::ESCAPE,
        "tab" => keys::TAB,
        c if c.chars().count() == 1 => keys::letter(c.chars().next()?),
        _ => return None,
    };
    let mods = Mods {
        ctrl,
        shift,
        ..Mods::default()
    };
    Some(Step::Key(sym, mods))
}

/// The pointer as the script left it.
struct Held {
    pointer: [f32; 2],
}

impl Held {
    fn input(&self) -> Input {
        Input {
            pointer: self.pointer,
            pointer_in: self.pointer[0] >= 0.0,
            focused: true,
            dt: 1.0 / 60.0,
            ..Input::default()
        }
    }
}

/// Run `steps`, settle, and write the frame to `out`.
pub fn shot(gallery: &mut Gallery, size: [u32; 2], steps: &[Step], out: &std::path::Path) -> Result<(), ShotError> {
    let view = [size[0] as f32, size[1] as f32];
    let mut held = Held { pointer: [-1.0, -1.0] };
    // Two frames so the first hit tests have geometry.
    for _ in 0..2 {
        gallery.frame(held.input(), view)?;
    }
    for step in steps {
        run_step(gallery, &mut held, step, view)?;
    }
    gallery.frame(held.input(), view)?;
    std::fs::write(out, render(gallery, size)).map_err(ShotError::Write)
}

fn run_step(gallery: &mut Gallery, held: &mut Held, step: &Step, view: [f32; 2]) -> Result<(), GalleryError> {
    match step {
        Step::Move(at) => {
            held.pointer = *at;
            gallery.frame(held.input(), view)
        }
        Step::Click(at) => {
            held.pointer = *at;
            gallery.frame(held.input(), view)?;
            for pressed in [true, false] {
                let mut input = held.input();
                input.pointer_down = pressed;
                input.push_button(ButtonEvent {
                    button: Button::Left,
                    pressed,
                    at: [at[0] as i32, at[1] as i32],
                    mods: Mods::default(),
                });
                gallery.frame(input, view)?;
            }
            Ok(())
        }
        Step::Type(text) => {
            let mut input = held.input();
            input.push_text(text);
            gallery.frame(input, view)
        }
        Step::Key(sym, mods) => {
            let mut input = held.input();
            input.mods = *mods;
            input.push_key(KeyEvent {
                sym: *sym,
                pressed: true,
                repeat: false,
                mods: *mods,
            });
            gallery.frame(input, view)
        }
        Step::Wheel(notches) => {
            let mut input = held.input();
            input.wheel = [0.0, -notches * stein_ui::WHEEL_NOTCH];
            gallery.frame(input, view)
        }
        Step::Wait(frames) => (0..*frames).try_for_each(|_| gallery.frame(held.input(), view)),
    }
}

/// The current frame through the software painter, as PNG bytes.
pub fn render(gallery: &Gallery, size: [u32; 2]) -> Vec<u8> {
    let (w, h) = (size[0] as usize, size[1] as usize);
    let mut px = vec![0u32; w * h];
    clear_region(&mut px, w, [0, 0, size[0], size[1]], palette::BG);
    let (pixels, atlas_size) = gallery.atlas();
    let atlas = Atlas {
        pixels,
        size: atlas_size,
        images: Images::NONE,
    };
    paint(stein_shell::Blend::detect(), &mut px, w, gallery.quads(), &atlas);
    stein_shell::shot::png(&px, size[0], size[1])
}
