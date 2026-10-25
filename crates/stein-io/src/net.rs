//! Socket setup.

use std::net::SocketAddr;
use std::os::fd::{AsFd, OwnedFd};

use rustix::io::Errno;
use rustix::net::{self, AddressFamily, SocketAddrAny, SocketAddrUnix, SocketFlags, SocketType};

const BACKLOG: i32 = 128;

fn family_of(addr: SocketAddr) -> AddressFamily {
    match addr {
        SocketAddr::V4(_) => AddressFamily::INET,
        SocketAddr::V6(_) => AddressFamily::INET6,
    }
}

fn socket(addr: SocketAddr, kind: SocketType) -> Result<OwnedFd, Errno> {
    net::socket_with(
        family_of(addr),
        kind,
        SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
        None,
    )
}

/// Bind a TCP socket and listen. Port zero picks one, see [`local_addr`].
pub fn tcp_listen(addr: SocketAddr) -> Result<OwnedFd, Errno> {
    tcp_listen_with(addr, false, BACKLOG)
}

/// [`tcp_listen`] with `SO_REUSEPORT` and a backlog. The kernel clamps
/// `backlog` to `net.core.somaxconn`.
pub fn tcp_listen_with(addr: SocketAddr, reuse_port: bool, backlog: i32) -> Result<OwnedFd, Errno> {
    let fd = socket(addr, SocketType::STREAM)?;
    // Rebind while old connections sit in TIME_WAIT.
    net::sockopt::set_socket_reuseaddr(&fd, true)?;
    if reuse_port {
        net::sockopt::set_socket_reuseport(&fd, true)?;
    }
    net::bind(&fd, &addr)?;
    net::listen(&fd, backlog)?;
    Ok(fd)
}

/// Accept one connection, non-blocking and close-on-exec.
pub fn accept(listener: impl AsFd) -> Result<OwnedFd, Errno> {
    net::accept_with(listener, SocketFlags::NONBLOCK | SocketFlags::CLOEXEC)
}

/// Bind an unconnected datagram socket.
pub fn udp_bind(addr: SocketAddr) -> Result<OwnedFd, Errno> {
    let fd = socket(addr, SocketType::DGRAM)?;
    net::bind(&fd, &addr)?;
    Ok(fd)
}

pub fn local_addr(fd: impl AsFd) -> Result<SocketAddr, Errno> {
    SocketAddr::try_from(net::getsockname(fd)?)
}

/// Peer of a connected socket, if it is an internet one.
pub fn peer_addr(fd: impl AsFd) -> Option<SocketAddr> {
    let any = net::getpeername(fd).ok()??;
    SocketAddr::try_from(any).ok()
}

/// `None` unless an internet address.
pub fn to_socket_addr(any: Option<SocketAddrAny>) -> Option<SocketAddr> {
    SocketAddr::try_from(any?).ok()
}

/// How far a connect got before returning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Started {
    /// Connected inside the syscall, as loopback often does.
    Done,
    /// Wait for writable.
    Pending,
    Failed(Errno),
}

/// Start a non-blocking TCP connect. Errors only if no socket could be made,
/// connect failures are [`Started::Failed`].
pub fn tcp_connect(addr: SocketAddr) -> Result<(OwnedFd, Started), Errno> {
    let fd = socket(addr, SocketType::STREAM)?;
    let started = match net::connect(&fd, &addr) {
        Ok(()) => Started::Done,
        Err(Errno::INPROGRESS) => Started::Pending,
        Err(e) => Started::Failed(e),
    };
    Ok((fd, started))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnixAddr<'p> {
    /// Filesystem path, no terminator.
    Path(&'p [u8]),
    /// Abstract name, no leading NUL.
    Abstract(&'p [u8]),
}

/// As [`tcp_connect`] for a local stream socket. A full backlog is [`Started::Failed`].
pub fn unix_connect(to: UnixAddr<'_>) -> Result<(OwnedFd, Started), Errno> {
    let addr = match to {
        UnixAddr::Path(p) => SocketAddrUnix::new(p)?,
        UnixAddr::Abstract(n) => SocketAddrUnix::new_abstract_name(n)?,
    };
    let fd = net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
        None,
    )?;
    let started = match net::connect(&fd, &addr) {
        Ok(()) => Started::Done,
        Err(Errno::INPROGRESS) => Started::Pending,
        Err(e) => Started::Failed(e),
    };
    Ok((fd, started))
}

/// Outcome of a pending connect. Writability alone does not mean success.
pub fn connect_result(fd: impl std::os::fd::AsFd) -> Result<(), Errno> {
    match net::sockopt::socket_error(fd) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(e),
    }
}

/// Datagram socket with a fixed peer. The kernel drops packets from other addresses.
pub fn udp_connect(addr: SocketAddr) -> Result<OwnedFd, Errno> {
    let fd = socket(addr, SocketType::DGRAM)?;
    net::connect(&fd, &addr)?;
    Ok(fd)
}
