//! QUIC (RFC 9000) under HTTP/3: quinn-proto's endpoint and transport config,
//! seeded connection ids for replayable runs, and its errors as [`QuicLoss`].

use core::time::Duration;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use quinn_proto as q;

use super::{H3Error, QuicLoss};

/// Peer bytes in flight on one request stream, and on the connection (RFC 9000 4).
pub const STREAM_WINDOW: u64 = 4 << 20;
pub const CONN_WINDOW: u64 = 8 * STREAM_WINDOW;
/// Idle timeout asked for (RFC 9000 10.1).
pub const IDLE: Duration = Duration::from_secs(30);
/// PING interval, half the idle timeout.
pub const KEEP_ALIVE: Duration = Duration::from_secs(15);
/// Largest UDP payload taken (RFC 9000 18.2), Ethernet MTU less IPv6 and UDP.
pub const MAX_UDP_PAYLOAD: u16 = 1452;
/// Uni streams the peer may hold open. A server opens three (RFC 9114 6.2) and may
/// open reserved ones, which are stopped.
pub const PEER_UNI: u32 = 8;

/// Connection ids from a seed so a simulated run replays. A real client draws them
/// from system randomness, as RFC 9000 5.1 wants unlinkable ids.
struct SeededCids {
    state: u64,
}

/// SplitMix64 (Steele, Lea, Flood, OOPSLA 2014).
fn mix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl q::ConnectionIdGenerator for SeededCids {
    fn generate_cid(&mut self) -> q::ConnectionId {
        q::ConnectionId::new(&mix(&mut self.state).to_le_bytes())
    }

    fn cid_len(&self) -> usize {
        8
    }

    fn cid_lifetime(&self) -> Option<Duration> {
        None
    }
}

/// Transport policy of a client connection.
#[derive(Clone, Copy, Debug)]
pub struct Transport {
    pub stream_window: u64,
    pub conn_window: u64,
    pub idle: Duration,
    pub keep_alive: Option<Duration>,
    /// BBR instead of Cubic (RFC 9438), for A/B runs.
    pub bbr: bool,
}

impl Default for Transport {
    fn default() -> Self {
        Transport {
            stream_window: STREAM_WINDOW,
            conn_window: CONN_WINDOW,
            idle: IDLE,
            keep_alive: Some(KEEP_ALIVE),
            bbr: false,
        }
    }
}

fn window(v: u64) -> Result<q::VarInt, H3Error> {
    q::VarInt::from_u64(v).map_err(|_| H3Error::Connect {
        what: "a window past a varint",
    })
}

/// Client transport config from `t`. No bidi streams for the peer (RFC 9114 6.1).
pub fn transport(t: &Transport) -> Result<q::TransportConfig, H3Error> {
    let idle = q::IdleTimeout::try_from(t.idle).map_err(|_| H3Error::Connect {
        what: "an idle timeout past a varint",
    })?;
    let mut config = q::TransportConfig::default();
    config
        .stream_receive_window(window(t.stream_window)?)
        .receive_window(window(t.conn_window)?)
        .max_idle_timeout(Some(idle))
        .keep_alive_interval(t.keep_alive)
        .max_concurrent_bidi_streams(0u32.into())
        .max_concurrent_uni_streams(PEER_UNI.into())
        // RFC 9000 14, every path carries 1200 bytes.
        .initial_mtu(1200)
        .min_mtu(1200)
        // One datagram per send.
        .enable_segmentation_offload(false);
    if t.bbr {
        config.congestion_controller_factory(Arc::new(q::congestion::BbrConfig::default()));
    }
    Ok(config)
}

/// Client config over `crypto` (TLS 1.3, ALPN `h3`). With `seed` the first
/// destination id is the seed's.
pub fn client_config(
    crypto: Arc<dyn q::crypto::ClientConfig>,
    t: &Transport,
    seed: Option<u64>,
) -> Result<q::ClientConfig, H3Error> {
    let mut config = q::ClientConfig::new(crypto);
    config.transport_config(Arc::new(transport(t)?));
    if let Some(seed) = seed {
        // 16 bytes (RFC 9000 7.2).
        let next = Arc::new(AtomicU64::new(seed ^ 0xD5C1D));
        config.initial_dst_cid_provider(Arc::new(move || {
            let mut s = next.fetch_add(0x9E37_79B9, Ordering::Relaxed);
            let (a, b) = (mix(&mut s), mix(&mut s));
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&a.to_le_bytes());
            id[8..].copy_from_slice(&b.to_le_bytes());
            q::ConnectionId::new(&id)
        }));
    }
    Ok(config)
}

/// Endpoint config with [`MAX_UDP_PAYLOAD`] datagrams. With `seed` its randomness
/// and connection ids are the seed's.
pub fn endpoint_config(seed: Option<u64>) -> Result<q::EndpointConfig, H3Error> {
    let mut endpoint = q::EndpointConfig::default();
    endpoint
        .max_udp_payload_size(MAX_UDP_PAYLOAD)
        .map_err(|_| H3Error::Connect {
            what: "a payload size quinn refuses",
        })?;
    if let Some(seed) = seed {
        let mut s = seed;
        let mut bytes = [0u8; 32];
        for part in bytes.chunks_mut(8) {
            part.copy_from_slice(&mix(&mut s).to_le_bytes());
        }
        endpoint.rng_seed(Some(bytes));
        endpoint.cid_generator(move || {
            Box::new(SeededCids {
                state: seed ^ 0xC1D,
            })
        });
    }
    Ok(endpoint)
}

/// How a connection ended.
pub fn loss(e: &q::ConnectionError) -> QuicLoss {
    match e {
        q::ConnectionError::VersionMismatch => QuicLoss::VersionMismatch,
        q::ConnectionError::TransportError(t) => QuicLoss::Transport {
            code: u64::from(t.code),
        },
        q::ConnectionError::ConnectionClosed(c) => QuicLoss::Transport {
            code: u64::from(c.error_code),
        },
        q::ConnectionError::ApplicationClosed(c) => QuicLoss::Application {
            code: c.error_code.into_inner(),
        },
        q::ConnectionError::Reset => QuicLoss::Reset,
        q::ConnectionError::TimedOut => QuicLoss::TimedOut,
        q::ConnectionError::LocallyClosed => QuicLoss::Local,
        q::ConnectionError::CidsExhausted => QuicLoss::CidsExhausted,
    }
}

/// Why `connect` refused.
pub fn connect_error(e: &q::ConnectError) -> H3Error {
    H3Error::Connect {
        what: match e {
            q::ConnectError::EndpointStopping => "the endpoint stops",
            q::ConnectError::CidsExhausted => "out of connection ids",
            q::ConnectError::InvalidServerName(_) => "not a server name",
            q::ConnectError::InvalidRemoteAddress(_) => "not an address to dial",
            q::ConnectError::NoDefaultClientConfig => "no client config",
            q::ConnectError::UnsupportedVersion => "an unsupported version",
        },
    }
}

/// Application error code as quinn's varint. H3 codes all fit.
pub fn code(c: u64) -> q::VarInt {
    q::VarInt::from_u64(c).unwrap_or(q::VarInt::from_u32(0x102))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_ids_replay_and_differ_by_seed() {
        use q::ConnectionIdGenerator;
        let ids = |seed| {
            let mut g = SeededCids { state: seed };
            [g.generate_cid(), g.generate_cid()]
        };
        assert_eq!(ids(1), ids(1));
        assert_ne!(ids(1), ids(2));
        assert_ne!(ids(1)[0], ids(1)[1]);
    }

    #[test]
    fn the_default_transport_builds() {
        assert!(transport(&Transport::default()).is_ok());
        assert!(endpoint_config(Some(3)).is_ok());
        let huge = Transport {
            stream_window: u64::MAX,
            ..Transport::default()
        };
        assert!(matches!(transport(&huge), Err(H3Error::Connect { .. })));
    }
}
