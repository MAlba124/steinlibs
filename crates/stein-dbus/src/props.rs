//! `org.freedesktop.DBus.Properties` both ways, the standard error names,
//! and introspection.
//!
//! Serving: [`request`] reads a Get, GetAll or Set call, and the reply
//! helpers write the variant or `a{sv}` back. Calling: [`get`] and
//! [`get_all`] ask another object, and a reply reads with
//! [`crate::value::Value`] and [`crate::value::Dict`].

use crate::conn::{Call, DbusError, Outbox, ReplyTo, Signal};
use crate::error::WireError;
use crate::msg::{Kind, Message};
use crate::value::Value;
use crate::write::Writer;

pub const INTERFACE: &str = "org.freedesktop.DBus.Properties";
pub const INTROSPECTABLE: &str = "org.freedesktop.DBus.Introspectable";
pub const PEER: &str = "org.freedesktop.DBus.Peer";

/// The error names every service answers with.
pub mod errors {
    pub const FAILED: &str = "org.freedesktop.DBus.Error.Failed";
    pub const UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";
    pub const UNKNOWN_OBJECT: &str = "org.freedesktop.DBus.Error.UnknownObject";
    pub const UNKNOWN_INTERFACE: &str = "org.freedesktop.DBus.Error.UnknownInterface";
    pub const UNKNOWN_PROPERTY: &str = "org.freedesktop.DBus.Error.UnknownProperty";
    pub const READ_ONLY: &str = "org.freedesktop.DBus.Error.PropertyReadOnly";
    pub const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";
}

/// A Properties call, read.
#[derive(Clone, Copy, Debug)]
pub enum Request<'a> {
    Get {
        iface: &'a str,
        name: &'a str,
    },
    GetAll {
        iface: &'a str,
    },
    Set {
        iface: &'a str,
        name: &'a str,
        value: Value<'a>,
    },
}

/// The Properties call `m` makes, if it is one and well formed.
pub fn request<'a>(m: &Message<'a>) -> Option<Request<'a>> {
    if m.kind() != Kind::Call || m.interface() != Some(INTERFACE) {
        return None;
    }
    let mut r = m.body();
    Some(match (m.member()?, m.signature()) {
        ("Get", "ss") => Request::Get {
            iface: r.str().ok()?,
            name: r.str().ok()?,
        },
        ("GetAll", "s") => Request::GetAll {
            iface: r.str().ok()?,
        },
        ("Set", "ssv") => Request::Set {
            iface: r.str().ok()?,
            name: r.str().ok()?,
            value: Value::read(&mut r).ok()?,
        },
        _ => return None,
    })
}

/// Answer a Get: `write` puts the value, of type `sig`.
pub fn reply_get(
    conn: &mut Outbox<'_>,
    to: &ReplyTo,
    sig: &str,
    write: impl FnOnce(&mut Writer<'_>) -> Result<(), WireError>,
) -> Result<Option<u32>, DbusError> {
    conn.reply(to, "v", |w| {
        w.variant(sig)?;
        write(w)
    })
}

/// Answer a GetAll: `write` puts the `{sv}` entries, with [`Writer::sv`].
pub fn reply_all(
    conn: &mut Outbox<'_>,
    to: &ReplyTo,
    write: impl FnOnce(&mut Writer<'_>) -> Result<(), WireError>,
) -> Result<Option<u32>, DbusError> {
    conn.reply(to, "a{sv}", |w| {
        let a = w.begin_array(b'{')?;
        write(w)?;
        w.end_array(a)
    })
}

/// Emit PropertiesChanged for `iface` at `path`: `write` puts the changed
/// `{sv}` entries; nothing is invalidated.
pub fn changed(
    conn: &mut Outbox<'_>,
    path: &str,
    iface: &str,
    write: impl FnOnce(&mut Writer<'_>) -> Result<(), WireError>,
) -> Result<u32, DbusError> {
    let s = Signal {
        destination: None,
        path,
        interface: INTERFACE,
        member: "PropertiesChanged",
        sig: "sa{sv}as",
    };
    conn.signal(&s, |w| {
        w.str(iface)?;
        let a = w.begin_array(b'{')?;
        write(w)?;
        w.end_array(a)?;
        let b = w.begin_array(b's')?;
        w.end_array(b)
    })
}

/// Answer Introspect with `xml`.
pub fn reply_introspect(
    conn: &mut Outbox<'_>,
    to: &ReplyTo,
    xml: &str,
) -> Result<Option<u32>, DbusError> {
    conn.reply(to, "s", |w| w.str(xml))
}

fn props_call<'s>(dest: &'s str, path: &'s str, member: &'s str, sig: &'s str) -> Call<'s> {
    Call {
        destination: Some(dest),
        path,
        interface: Some(INTERFACE),
        member,
        sig,
        flags: 0,
    }
}

/// Ask for one property; the reply is a `v`.
pub fn get(
    conn: &mut Outbox<'_>,
    dest: &str,
    path: &str,
    iface: &str,
    name: &str,
) -> Result<u32, DbusError> {
    conn.call(&props_call(dest, path, "Get", "ss"), |w| {
        w.str(iface)?;
        w.str(name)
    })
}

/// Ask for every property of `iface`; the reply is an `a{sv}`.
pub fn get_all(
    conn: &mut Outbox<'_>,
    dest: &str,
    path: &str,
    iface: &str,
) -> Result<u32, DbusError> {
    conn.call(&props_call(dest, path, "GetAll", "s"), |w| w.str(iface))
}

#[cfg(test)]
mod tests;
