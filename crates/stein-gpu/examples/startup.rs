//! Time bringing the GPU up: open and the quad pipeline.
//!
//! ```text
//! cargo run --release -p stein-gpu --example startup -- [auto|software|N|drm:MAJOR:MINOR]
//! ```
//!
//! Run it outside the dev shell with a clean environment to see what an
//! app launched from the compositor pays.

use std::time::Instant;

use stein_gpu::quad::QuadPass;
use stein_gpu::{Adapter, Format, Gpu, GpuDesc};

fn main() {
    let adapter = match std::env::args().nth(1).as_deref() {
        Some("software") => Adapter::Software,
        Some(d) if d.starts_with("drm:") => {
            let mut it = d[4..].split(':').filter_map(|v| v.parse().ok());
            let (major, minor) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
            Adapter::Drm { major, minor }
        }
        Some(n) => n.parse().map(Adapter::Index).unwrap_or(Adapter::Auto),
        None => Adapter::Auto,
    };
    let t = Instant::now();
    let desc = GpuDesc { adapter, validation: false, frames: 1, texture_slots: 16, ..GpuDesc::default() };
    let mut gpu = match Gpu::open(&desc) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("open failed: {e}");
            return;
        }
    };
    let open = t.elapsed();
    let t = Instant::now();
    let pass = QuadPass::new(&mut gpu, Format::Bgra8Unorm);
    let pipe = t.elapsed();
    let c = gpu.caps();
    eprintln!("{}: open {open:?}, quad pipeline {pipe:?} ({}), direct {}", c.name, pass.is_ok(), c.direct);
}
