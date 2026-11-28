//! Can we actually reach a compositor?
//!
//! This exists because the failure it catches is invisible at build time. With
//! the `dlopen` feature, libwayland is looked up at runtime, and on a system
//! with no ldconfig cache it is simply never found: the application starts,
//! parses its input, and only then dies trying to open a window.
//!
//! Connecting does not create a surface, so this takes no focus.

#[test]
fn the_wayland_library_is_linked_and_a_compositor_is_reachable() {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("no WAYLAND_DISPLAY, skipping");
        return;
    }
    match wayland_client::Connection::connect_to_env() {
        Ok(c) => {
            // A display object back means libwayland resolved and the socket
            // answered.
            let _ = c.display();
            eprintln!("connected");
        }
        Err(e) => panic!(
            "could not connect: {e}. If that says the library could not be \
             loaded, libwayland is being dlopened rather than linked."
        ),
    }
}
