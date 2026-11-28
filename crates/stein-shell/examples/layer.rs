//! A bare bar on a layer surface, for checking the layer role live.
//!
//! `cargo run -p stein-shell --example layer -- [--bottom] [--height N]
//! [--output NAME]`. The pointer's column is highlighted, so input shows up.
//! Runs until interrupted.

use stein_layout::Rect;
use stein_shell::layer::bar;
use stein_shell::{Layer, Scene};
use stein_ui::Input;
use stein_ui::draw::{Damage, Quad};

const BG: u32 = 0xff20_2228;
const HOVER: u32 = 0xff4a_7fc1;

struct Bar {
    spec: Layer,
    output: Option<String>,
    quads: Vec<Quad>,
    last: Vec<Quad>,
    damage: Damage,
}

impl Scene for Bar {
    fn build(&mut self, input: Input, viewport: [f32; 2]) {
        let all = Rect {
            min: [0.0, 0.0],
            max: viewport,
        };
        self.quads.clear();
        self.quads.push(Quad::solid(all, all, BG));
        if input.pointer_in {
            let x = input.pointer[0].floor();
            let r = Rect {
                min: [x - 20.0, 0.0],
                max: [x + 20.0, viewport[1]],
            };
            self.quads.push(Quad::solid(r, all, HOVER));
        }
        if self.quads != self.last {
            self.damage.add(all);
        }
    }

    fn needs_present(&self) -> bool {
        !self.damage.is_empty()
    }

    fn damage(&self) -> Damage {
        self.damage
    }

    fn presented(&mut self) {
        self.last.clone_from(&self.quads);
        self.damage.clear();
    }

    fn quads(&self) -> &[Quad] {
        &self.quads
    }

    fn atlas(&self) -> (&[u8], [u32; 2]) {
        (&[0], [1, 1])
    }

    fn app_id(&self) -> &str {
        "dev.stein.layer-example"
    }

    fn layer(&self) -> Option<Layer> {
        Some(self.spec)
    }

    fn output(&self) -> Option<&str> {
        self.output.as_deref()
    }
}

fn main() -> std::process::ExitCode {
    let mut bottom = false;
    let mut height = 24;
    let mut output = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bottom" => bottom = true,
            "--height" => height = args.next().and_then(|v| v.parse().ok()).unwrap_or(height),
            "--output" => output = args.next(),
            other => {
                eprintln!("layer: unknown argument {other}");
                return std::process::ExitCode::FAILURE;
            }
        }
    }
    let scene = Bar {
        spec: bar(height, bottom, "stein-example"),
        output,
        quads: Vec::new(),
        last: Vec::new(),
        damage: Damage::new(),
    };
    match stein_shell::run(Box::new(scene)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("layer: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
