//! Window, input and software painting.

/// Protocol tracing, off unless `STEIN_LOG` is set.
///
/// The spike this grew from existed partly to map the Wayland surface area, so
/// every event was worth printing while that was the job. It is not worth
/// printing while using the thing: a pointer move alone emits a line per frame.
pub(crate) fn verbose() -> bool {
    use std::sync::OnceLock;
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| std::env::var_os("STEIN_LOG").is_some())
}

/// Milliseconds since the first trace, for reading traces as a timeline.
pub(crate) fn since() -> f64 {
    use std::sync::OnceLock;
    static T: OnceLock<std::time::Instant> = OnceLock::new();
    T.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1e3
}

macro_rules! trace {
    ($($a:tt)*) => {
        if crate::verbose() {
            eprintln!("{:9.2} {}", crate::since(), format_args!($($a)*));
        }
    };
}

pub mod blend;
pub mod clipboard;
pub mod gpu;
mod input;
pub mod keyboard;
pub mod layer;
mod lock;
pub mod multi;
pub mod paint;
mod plan;
pub mod scene;
mod selection;
mod shell;
pub mod shot;
mod surface;
pub mod video;

pub use blend::Blend;
pub use gpu::{Begun, GpuCtx, GpuPaint, PaintError, Presenting};
pub use keyboard::{Keyboard, KeyboardError};
pub use layer::{Anchor, Keys, Layer, LayerError, Level};
pub use multi::{MultiScene, Output, Popup, Role, Side, Want};
pub use paint::{Backdrop, Images};
pub use scene::{Clip, Cursor, Flavour, Scene};
pub use shell::{ShellError, run, run_multi};
pub use video::{Dmabuf, DmabufPlane, VideoLayer, dmabuf_formats};
