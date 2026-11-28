//! A bar on every output through [`MultiScene`], with popups and images.
//!
//! `cargo run -p stein-shell --example bars -- [--bottom] [--popup] [--grab]`
//!
//! Clicking a bar opens a popup at the pointer showing an RGBA checker;
//! clicking the popup closes it. `--popup` opens one off the first bar at
//! start that animates for a while and then closes itself. `--grab` makes
//! popups grab, so a click on another client dismisses them; examples/click.rs
//! makes that click.
//! Outputs coming and going add and remove bars.

use stein_layout::Rect;
use stein_shell::layer::bar;
use stein_shell::{Images, MultiScene, Output, Popup, Role, Side, Want};
use stein_ui::draw::{Damage, Quad};
use stein_ui::{Button, Input};

const HEIGHT: u32 = 24;
const POPUP: u32 = u32::MAX;
const BG: u32 = 0xff20_2228;
const HOVER: u32 = 0xff4a_7fc1;
const MENU: u32 = 0xff30_3446;
/// Frames the self-opened popup lives for.
const LIFE: u32 = 90;

/// One surface's draw list and what it last presented.
struct View {
    key: u32,
    quads: Vec<Quad>,
    last: Vec<Quad>,
    damage: Damage,
}

struct Bars {
    bottom: bool,
    grab: bool,
    wants: Vec<Want>,
    outputs: Vec<u32>,
    views: Vec<View>,
    popup: Option<Popup>,
    /// Frames the popup has been built, for the one that closes itself.
    age: Option<u32>,
    checker: Vec<u8>,
}

impl Bars {
    fn view(&mut self, key: u32) -> &mut View {
        if let Some(i) = self.views.iter().position(|v| v.key == key) {
            return &mut self.views[i];
        }
        self.views.push(View {
            key,
            quads: Vec::new(),
            last: Vec::new(),
            damage: Damage::new(),
        });
        let n = self.views.len();
        &mut self.views[n - 1]
    }

    fn get(&self, key: u32) -> Option<&View> {
        self.views.iter().find(|v| v.key == key)
    }

    fn rewant(&mut self) {
        self.wants.clear();
        for &id in &self.outputs {
            self.wants.push(Want {
                key: id,
                role: Role::Layer {
                    layer: bar(HEIGHT, self.bottom, "stein-bars"),
                    output: Some(id),
                },
            });
        }
        if let Some(p) = self.popup {
            self.wants.push(Want {
                key: POPUP,
                role: Role::Popup(p),
            });
        }
        self.views
            .retain(|v| v.key == POPUP || self.outputs.contains(&v.key));
    }

    fn open(&mut self, parent: u32, x: i32) {
        let side = if self.bottom {
            Side::Above
        } else {
            Side::Below
        };
        self.popup = Some(Popup {
            parent,
            anchor: [x, 0, 1, HEIGHT as i32],
            size: [160, 120],
            side,
            grab: self.grab,
        });
        self.rewant();
    }

    fn close(&mut self) {
        self.popup = None;
        self.age = None;
        self.rewant();
    }

    fn draw_bar(&mut self, key: u32, input: &Input, viewport: [f32; 2]) {
        let all = Rect {
            min: [0.0, 0.0],
            max: viewport,
        };
        let v = self.view(key);
        v.quads.clear();
        v.quads.push(Quad::solid(all, all, BG));
        if input.pointer_in {
            let x = input.pointer[0].floor();
            let r = Rect {
                min: [x - 20.0, 0.0],
                max: [x + 20.0, viewport[1]],
            };
            v.quads.push(Quad::solid(r, all, HOVER));
        }
        let icon = Rect {
            min: [4.0, 4.0],
            max: [20.0, 20.0],
        };
        v.quads
            .push(Quad::image(icon, all, 0xffff_ffff, [0.0, 0.0, 1.0, 1.0]));
    }

    /// Age the popup and draw it, false once it closed itself.
    fn build_popup(&mut self, input: &Input, viewport: [f32; 2]) -> bool {
        let age = self.age.map(|a| a + 1);
        self.age = age;
        let expired = age.is_some_and(|a| a >= LIFE);
        if expired {
            eprintln!("bars: popup lived {LIFE} frames, closing");
        }
        if expired || clicked(input).is_some() {
            self.close();
            return false;
        }
        self.draw_popup(viewport);
        true
    }

    fn draw_popup(&mut self, viewport: [f32; 2]) {
        let all = Rect {
            min: [0.0, 0.0],
            max: viewport,
        };
        let age = self.age.unwrap_or(0);
        let v = self.view(POPUP);
        v.quads.clear();
        v.quads.push(Quad::solid(all, all, MENU));
        let pic = Rect {
            min: [8.0, 8.0],
            max: [72.0, 72.0],
        };
        v.quads
            .push(Quad::image(pic, all, 0xffff_ffff, [0.0, 0.0, 1.0, 1.0]));
        // A bar that grows while the popup lives, so it redraws each frame.
        let w = (age as f32 * 1.5).min(viewport[0] - 16.0);
        let meter = Rect {
            min: [8.0, viewport[1] - 16.0],
            max: [8.0 + w, viewport[1] - 8.0],
        };
        v.quads.push(Quad::solid(meter, all, HOVER));
    }
}

fn clicked(input: &Input) -> Option<i32> {
    input.buttons[..input.button_count as usize]
        .iter()
        .find(|b| b.pressed && b.button == Button::Left)
        .map(|b| b.at[0])
}

impl MultiScene for Bars {
    fn outputs(&mut self, outputs: &[Output<'_>]) {
        eprintln!(
            "bars: outputs {:?}",
            outputs.iter().map(|o| o.name).collect::<Vec<_>>()
        );
        self.outputs = outputs.iter().map(|o| o.id).collect();
        if let Some(p) = self.popup
            && !self.outputs.contains(&p.parent)
        {
            self.popup = None;
        }
        if self.age == Some(0)
            && self.popup.is_none()
            && let Some(&first) = self.outputs.first()
        {
            self.open(first, 40);
        }
        self.rewant();
    }

    fn surfaces(&self) -> &[Want] {
        &self.wants
    }

    fn closed(&mut self, key: u32) {
        eprintln!("bars: surface {key} closed by the compositor");
        if key == POPUP {
            self.close();
        }
    }

    fn build(&mut self, key: u32, input: Input, viewport: [f32; 2]) {
        let open = match key {
            POPUP => self.build_popup(&input, viewport),
            _ => {
                if let Some(x) = clicked(&input) {
                    self.open(key, x);
                }
                self.draw_bar(key, &input, viewport);
                true
            }
        };
        let v = self.view(key);
        if open && v.quads != v.last {
            v.damage.add(Rect {
                min: [0.0, 0.0],
                max: viewport,
            });
        }
    }

    fn needs_present(&self, key: u32) -> bool {
        self.get(key).is_some_and(|v| !v.damage.is_empty())
    }

    fn damage(&self, key: u32) -> Damage {
        self.get(key).map_or(Damage::new(), |v| v.damage)
    }

    fn presented(&mut self, key: u32) {
        let v = self.view(key);
        v.last.clone_from(&v.quads);
        v.damage.clear();
    }

    fn quads(&self, key: u32) -> &[Quad] {
        self.get(key).map_or(&[], |v| &v.quads)
    }

    fn atlas(&self) -> (&[u8], [u32; 2]) {
        (&[0], [1, 1])
    }

    fn images(&self) -> Images<'_> {
        Images {
            pixels: &self.checker,
            size: [8, 8],
            version: 1,
        }
    }

    fn app_id(&self) -> &str {
        "dev.stein.bars-example"
    }
}

/// An 8x8 checker of orange and clear, with a translucent diagonal.
fn checker() -> Vec<u8> {
    let mut px = Vec::with_capacity(8 * 8 * 4);
    for y in 0..8 {
        for x in 0..8 {
            let texel = match ((x + y) % 2, x == y) {
                (_, true) => [255, 255, 255, 128],
                (0, _) => [255, 140, 0, 255],
                _ => [0, 0, 0, 0],
            };
            px.extend_from_slice(&texel);
        }
    }
    px
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |f: &str| args.iter().any(|a| a == f);
    let scene = Bars {
        bottom: has("--bottom"),
        grab: has("--grab"),
        wants: Vec::new(),
        outputs: Vec::new(),
        views: Vec::new(),
        popup: None,
        age: has("--popup").then_some(0),
        checker: checker(),
    };
    match stein_shell::run_multi(Box::new(scene)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bars: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
