//! Against the real session bus, skipped when there is none.
//!
//! Owns a name, calls itself through the daemon, answers, and hears its
//! own signal, so every message kind crosses a real bus once.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "../examples/pump/mod.rs"]
mod pump;

use std::time::Duration;

use pump::{Bus, Mem};
use stein_dbus::{Call, Kind, ReplyTo, Signal, bus};

const LIMIT: Duration = Duration::from_secs(5);

/// Whether a message is the one waited for: kind, reply serial, member.
type Want<'w> = dyn Fn(Kind, Option<u32>, Option<&str>) -> bool + 'w;

/// Turn until a message matching `want` arrives, answering calls on the way.
fn wait_for(b: &mut Bus<'_>, want: &Want<'_>) -> String {
    let mut got = None;
    let mut calls = Vec::new();
    let start = std::time::Instant::now();
    while got.is_none() {
        assert!(start.elapsed() < LIMIT, "timed out");
        b.turn(Duration::from_millis(50), &mut |m| {
            if m.kind() == Kind::Call && m.member() == Some("Ping") {
                let arg = m.body().str().unwrap_or("").to_string();
                calls.push((ReplyTo::of(m), arg));
            }
            if want(m.kind(), m.reply_serial(), m.member()) {
                let body = m.body().str().unwrap_or("").to_string();
                got = Some(body);
            }
        })
        .expect("turn");
        for (to, arg) in calls.drain(..) {
            b.conn
                .reply(&to, "s", |w| w.str(&format!("pong {arg}")))
                .expect("reply");
        }
    }
    got.unwrap_or_default()
}

#[test]
fn a_round_trip_through_the_session_bus() {
    let Ok(address) = pump::session() else {
        eprintln!("no session bus, skipped");
        return;
    };
    let mut mem = Mem::new();
    let mut b = Bus::connect(&mut mem, &address).expect("connect");
    b.until(LIMIT, &mut |_| {}, &|b| b.conn.is_ready())
        .expect("ready");
    let me = b.conn.unique_name().expect("unique name").to_string();
    assert!(me.starts_with(':'));

    let name = format!("dev.stein.DbusTest.p{}", std::process::id());
    let s = bus::request_name(b.conn.out(), &name, bus::DO_NOT_QUEUE).expect("request");
    let mut owned = 0;
    let start = std::time::Instant::now();
    while owned == 0 && start.elapsed() < LIMIT {
        b.turn(Duration::from_millis(50), &mut |m| {
            if m.reply_serial() == Some(s) {
                owned = m.body().u32().unwrap_or(u32::MAX);
            }
        })
        .expect("turn");
    }
    assert_eq!(owned, bus::PRIMARY_OWNER);

    // Call ourselves by the well known name, the daemon routes it back.
    let call = Call {
        destination: Some(&name),
        path: "/dev/stein/Test",
        interface: Some("dev.stein.Test"),
        member: "Ping",
        sig: "s",
        flags: 0,
    };
    let serial = b.conn.call(&call, |w| w.str("hi")).expect("call");
    let answer = wait_for(&mut b, &|k, r, _| k == Kind::Return && r == Some(serial));
    assert_eq!(answer, "pong hi");

    // Hear our own broadcast.
    bus::add_match(b.conn.out(), "type='signal',interface='dev.stein.Test'").expect("match");
    let sig = Signal {
        destination: None,
        path: "/dev/stein/Test",
        interface: "dev.stein.Test",
        member: "Changed",
        sig: "s",
    };
    b.conn.signal(&sig, |w| w.str("now")).expect("signal");
    let heard = wait_for(&mut b, &|k, _, m| k == Kind::Signal && m == Some("Changed"));
    assert_eq!(heard, "now");

    // An unknown method on the bus itself comes back as an error.
    let bad = Call {
        destination: Some(bus::NAME),
        path: bus::PATH,
        interface: Some(bus::INTERFACE),
        member: "NoSuchMethod",
        sig: "",
        flags: 0,
    };
    let serial = b.conn.call(&bad, |_| Ok(())).expect("call");
    wait_for(&mut b, &|k, r, _| k == Kind::Error && r == Some(serial));
}
