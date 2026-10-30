//! List the names on the session bus, then print name owner changes.
//!
//! `cargo run -p stein-dbus --example names -- [--system] [--watch SECS]`

mod pump;

use std::process::ExitCode;
use std::time::{Duration, Instant};

use pump::{Bus, BusError, Mem};
use stein_dbus::{Item, Kind, addr, bus};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("names: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), BusError> {
    let mut system = false;
    let mut watch = 0u64;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--system" => system = true,
            "--watch" => watch = args.next().and_then(|v| v.parse().ok()).unwrap_or(10),
            _ => {}
        }
    }
    let address = if system {
        addr::SYSTEM_BUS.to_string()
    } else {
        pump::session()?
    };
    let mut mem = Mem::new();
    let mut b = Bus::connect(&mut mem, &address)?;
    let limit = Duration::from_secs(5);
    b.until(limit, &mut |_| {}, &|b| b.conn.is_ready())?;
    println!("connected as {}", b.conn.unique_name().unwrap_or("?"));

    let serial = bus::list_names(b.conn.out())?;
    let mut names = Vec::new();
    let mut answered = false;
    let start = Instant::now();
    while !answered && start.elapsed() < limit {
        b.turn(Duration::from_millis(50), &mut |m| {
            if m.reply_serial() == Some(serial) {
                answered = true;
                let _ = m.walk(&mut |i| {
                    if let Item::Str(s) = i {
                        names.push(s.to_string());
                    }
                });
            }
        })?;
    }
    names.sort();
    for n in &names {
        println!("{n}");
    }
    println!("{} names", names.len());
    if watch == 0 {
        return Ok(());
    }

    let rule = "type='signal',interface='org.freedesktop.DBus',member='NameOwnerChanged'";
    bus::add_match(b.conn.out(), rule)?;
    let until = Instant::now() + Duration::from_secs(watch);
    while Instant::now() < until {
        b.turn(Duration::from_millis(200), &mut |m| {
            if m.kind() == Kind::Signal && m.is(bus::INTERFACE, "NameOwnerChanged") {
                let mut s = Vec::new();
                let _ = m.walk(&mut |i| {
                    if let Item::Str(v) = i {
                        s.push(if v.is_empty() { "-" } else { v }.to_string());
                    }
                });
                println!("owner changed: {}", s.join(" "));
            }
        })?;
    }
    Ok(())
}
