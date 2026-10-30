//! The bus itself, `org.freedesktop.DBus`, and the calls every client makes.

use crate::conn::{Call, DbusError, Outbox};
use crate::msg::{Fields, Kind};

pub const NAME: &str = "org.freedesktop.DBus";
pub const PATH: &str = "/org/freedesktop/DBus";
pub const INTERFACE: &str = "org.freedesktop.DBus";

/// RequestName flags.
pub const ALLOW_REPLACEMENT: u32 = 0x1;
pub const REPLACE_EXISTING: u32 = 0x2;
pub const DO_NOT_QUEUE: u32 = 0x4;

/// RequestName answers.
pub const PRIMARY_OWNER: u32 = 1;
pub const IN_QUEUE: u32 = 2;
pub const EXISTS: u32 = 3;
pub const ALREADY_OWNER: u32 = 4;

/// Header fields for a call to the bus.
pub(crate) fn fields(kind: Kind, serial: u32, member: &str) -> Fields<'_> {
    Fields {
        path: Some(PATH),
        interface: Some(INTERFACE),
        member: Some(member),
        destination: Some(NAME),
        ..Fields::new(kind, serial)
    }
}

fn call<'s>(member: &'s str, sig: &'s str) -> Call<'s> {
    Call {
        destination: Some(NAME),
        path: PATH,
        interface: Some(INTERFACE),
        member,
        sig,
        flags: 0,
    }
}

/// Subscribe to what `rule` matches, `type='signal',interface='...'`.
pub fn add_match(conn: &mut Outbox<'_>, rule: &str) -> Result<u32, DbusError> {
    conn.call(&call("AddMatch", "s"), |w| w.str(rule))
}

pub fn remove_match(conn: &mut Outbox<'_>, rule: &str) -> Result<u32, DbusError> {
    conn.call(&call("RemoveMatch", "s"), |w| w.str(rule))
}

/// Ask for a well known name. The reply's u32 is one of the answers above.
pub fn request_name(conn: &mut Outbox<'_>, name: &str, flags: u32) -> Result<u32, DbusError> {
    conn.call(&call("RequestName", "su"), |w| {
        w.str(name)?;
        w.u32(flags)
    })
}

pub fn release_name(conn: &mut Outbox<'_>, name: &str) -> Result<u32, DbusError> {
    conn.call(&call("ReleaseName", "s"), |w| w.str(name))
}

/// Every name on the bus, answered as `as`.
pub fn list_names(conn: &mut Outbox<'_>) -> Result<u32, DbusError> {
    conn.call(&call("ListNames", ""), |_| Ok(()))
}

/// The unique name that owns `name`, answered as `s`.
pub fn get_name_owner(conn: &mut Outbox<'_>, name: &str) -> Result<u32, DbusError> {
    conn.call(&call("GetNameOwner", "s"), |w| w.str(name))
}
