//! Drag with the left button through the compositor's virtual pointer, the
//! way a hand resizes a floating window by its border.
//!
//! `cargo run -p stein-shell --example drag -- OUTPUT X0 Y0 X1 Y1 STEPS MS`,
//! in the output's own pixels, STEPS moves MS milliseconds apart. Needs
//! zwlr_virtual_pointer_manager_v1, which sway has.

use std::process::ExitCode;
use std::time::Duration;

use wayland_client::protocol::{wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

const BTN_LEFT: u32 = 0x110;

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
    let args: Vec<u32> = std::env::args()
        .skip(2)
        .filter_map(|v| v.parse().ok())
        .collect();
    let name = std::env::args().nth(1).unwrap_or_default();
    let [x0, y0, x1, y1, steps, ms] = args[..] else {
        eprintln!("drag: OUTPUT X0 Y0 X1 Y1 STEPS MS");
        return ExitCode::FAILURE;
    };
    match drag(&name, [x0, y0], [x1, y1], steps.max(1), ms) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("drag: {e}");
            ExitCode::FAILURE
        }
    }
}

fn drag(name: &str, from: [u32; 2], to: [u32; 2], steps: u32, ms: u32) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut s = State::default();
    for _ in 0..2 {
        queue.roundtrip(&mut s).map_err(|e| e.to_string())?;
    }
    let manager = s
        .manager
        .as_ref()
        .ok_or("no zwlr_virtual_pointer_manager_v1 v2")?;
    let (output, _, [w, h]) = s
        .outputs
        .iter()
        .find(|(_, n, _)| n == name)
        .ok_or(format!("no output {name}"))?;
    let vp = manager.create_virtual_pointer_with_output(s.seat.as_ref(), Some(output), &qh, ());
    let (w, h) = (*w as u32, *h as u32);
    let at = |i: u32| {
        let f =
            |a: u32, b: u32| (a as i64 + (b as i64 - a as i64) * i as i64 / steps as i64) as u32;
        (f(from[0], to[0]).min(w - 1), f(from[1], to[1]).min(h - 1))
    };
    let mut t = 0;
    let (x, y) = at(0);
    vp.motion_absolute(t, x, y, w, h);
    vp.frame();
    conn.flush().map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_millis(50));
    vp.motion_absolute(t, x, y, w, h);
    vp.button(t, BTN_LEFT, wl_pointer::ButtonState::Pressed);
    vp.frame();
    for i in 1..=steps {
        t += ms;
        let (x, y) = at(i);
        vp.motion_absolute(t, x, y, w, h);
        vp.frame();
        conn.flush().map_err(|e| e.to_string())?;
        std::thread::sleep(Duration::from_millis(u64::from(ms)));
    }
    let (x, y) = at(steps);
    vp.motion_absolute(t, x, y, w, h);
    vp.button(t, BTN_LEFT, wl_pointer::ButtonState::Released);
    vp.frame();
    conn.flush().map_err(|e| e.to_string())?;
    vp.destroy();
    queue.roundtrip(&mut s).map_err(|e| e.to_string())?;
    Ok(())
}
