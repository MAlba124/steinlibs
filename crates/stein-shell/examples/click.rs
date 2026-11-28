//! Click somewhere, through the compositor's virtual pointer, so input
//! paths can be driven without a hand on the mouse.
//!
//! `cargo run -p stein-shell --example click -- OUTPUT X Y [left|right|middle|up|down]`,
//! X and Y in the output's own pixels; up and down turn the wheel one notch. Needs zwlr_virtual_pointer_manager_v1, which sway has.

use std::process::ExitCode;
use std::time::Duration;

use wayland_client::protocol::{wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

#[derive(Default)]
struct State {
    seat: Option<wl_seat::WlSeat>,
    manager: Option<ZwlrVirtualPointerManagerV1>,
    outputs: Vec<(wl_output::WlOutput, String, [i32; 2])>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        s: &mut Self,
        r: &wl_registry::WlRegistry,
        e: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = e
        else {
            return;
        };
        match interface.as_str() {
            "wl_seat" => s.seat = Some(r.bind(name, 1, qh, ())),
            "zwlr_virtual_pointer_manager_v1" if version >= 2 => {
                s.manager = Some(r.bind(name, 2, qh, ()));
            }
            "wl_output" if version >= 4 => {
                let o = r.bind(name, 4, qh, ());
                s.outputs.push((o, String::new(), [0, 0]));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        s: &mut Self,
        o: &wl_output::WlOutput,
        e: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = s.outputs.iter_mut().find(|(p, _, _)| p == o) else {
            return;
        };
        match e {
            wl_output::Event::Name { name } => entry.1 = name,
            wl_output::Event::Mode { width, height, .. } => entry.2 = [width, height],
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore ZwlrVirtualPointerV1);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(name), Some(x), Some(y)) = (
        args.first(),
        args.get(1).and_then(|v| v.parse::<u32>().ok()),
        args.get(2).and_then(|v| v.parse::<u32>().ok()),
    ) else {
        eprintln!("click: OUTPUT X Y [left|right|middle|up|down]");
        return ExitCode::FAILURE;
    };
    let button = match args.get(3).map(String::as_str) {
        Some("right") => Press::Button(BTN_RIGHT),
        Some("middle") => Press::Button(BTN_MIDDLE),
        Some("up") => Press::Wheel(-15.0),
        Some("down") => Press::Wheel(15.0),
        _ => Press::Button(BTN_LEFT),
    };
    match click(name, x, y, button) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("click: {e}");
            ExitCode::FAILURE
        }
    }
}

/// A button to press and release, or wheel travel in the protocol's units.
#[derive(Clone, Copy)]
enum Press {
    Button(u32),
    Wheel(f64),
}

fn click(name: &str, x: u32, y: u32, press: Press) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut s = State::default();
    for _ in 0..2 {
        queue.roundtrip(&mut s).map_err(|e| e.to_string())?;
    }
    let manager = s.manager.as_ref().ok_or("no zwlr_virtual_pointer_manager_v1 v2")?;
    let (output, _, [w, h]) = s
        .outputs
        .iter()
        .find(|(_, n, _)| n == name)
        .ok_or(format!("no output {name}"))?;
    // Mapped onto the one output, so the extent is its mode.
    let vp = manager.create_virtual_pointer_with_output(s.seat.as_ref(), Some(output), &qh, ());
    let (w, h) = (*w as u32, *h as u32);
    let (x, y) = (x.min(w - 1), y.min(h - 1));
    // The position rides in the same frame as each button, since a real
    // mouse on the same seat may have moved the cursor in between.
    let states = [
        (10, wl_pointer::ButtonState::Pressed),
        (60, wl_pointer::ButtonState::Released),
    ];
    for (t, state) in states {
        vp.motion_absolute(t, x, y, w, h);
        match press {
            Press::Button(b) => vp.button(t, b, state),
            Press::Wheel(v) if state == wl_pointer::ButtonState::Pressed => {
                vp.axis_source(wl_pointer::AxisSource::Wheel);
                vp.axis(t, wl_pointer::Axis::VerticalScroll, v);
            }
            Press::Wheel(_) => {}
        }
        vp.frame();
        conn.flush().map_err(|e| e.to_string())?;
        std::thread::sleep(Duration::from_millis(50));
    }
    vp.destroy();
    queue.roundtrip(&mut s).map_err(|e| e.to_string())?;
    Ok(())
}
