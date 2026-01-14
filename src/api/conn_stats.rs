use std::sync::atomic::AtomicI32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::PROTOCOL_VERSION;
use jni::JNIEnv;
use jni::sys::jint;
use jni::sys::jobject;
use jni_macro::jni;
use serde::Serialize;

use crate::ENDPOINT;
use crate::JC;
use crate::data::relay::RelayInfo;
use crate::events::connection::ConnectionState;
use crate::quic::server::RELAY;
use crate::utils::systime;

pub static CONNECTION_STATE: AtomicI32 = AtomicI32::new(ConnectionState::Idle as i32);

/// as disconnecting will not reset it, it's rather start time from last connection
pub static CONNECTION_START_TIME: AtomicU64 = AtomicU64::new(0);

macro_rules! serializable_stats {
    (
        $(#[$meta:meta])*
        $vis:vis struct $Name:ident from $Source:ty {
            $(
                $(#[$field_meta:meta])*
                $field:ident: $typ:ty
            ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Serialize)]
        $vis struct $Name {
            $(
                $(#[$field_meta])*
                pub $field: $typ,
            )*
        }

        impl From<$Source> for $Name {
            fn from(stats: $Source) -> Self {
                Self {
                    $($field: stats.$field,)*
                }
            }
        }
    };
}

#[derive(Debug, Serialize)]
pub struct EndpointStats {
    pub open_connections: usize,
    pub accepted_handshakes: u64,
    pub outgoing_handshakes: u64,
    pub refused_handshakes: u64,
    pub ignored_handshakes: u64,
    pub bind_addr: String,
}

serializable_stats! {
    pub struct PathStats from quinn::PathStats {
        #[serde(with = "serde_duration_as_micros")]
        rtt: Duration, // secs
        cwnd: u64,
        congestion_events: u64,
        black_holes_detected: u64,
    }
}

mod serde_duration_as_micros {
    use std::time::Duration;

    use serde::Serializer;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(duration.as_micros() as u64)
    }
}

serializable_stats! {
    pub struct FrameStats from quinn::FrameStats {
        acks: u64,
        max_stream_data: u64,
        crypto: u64,
        reset_stream: u64,
    }
}

serializable_stats! {
    pub struct UdpStats from quinn::UdpStats {
        datagrams: u64,
        bytes: u64,
        ios: u64,
    }
}

#[derive(Debug, Serialize)]
pub struct ConnectionStats {
    pub uptime: u64, // seconds since connection established

    pub path: PathStats,

    pub frame_rx: FrameStats,
    pub frame_tx: FrameStats,

    #[serde(default)]
    pub udp_rx: UdpStats,
    #[serde(default)]
    pub udp_tx: UdpStats,

    pub remote_address: String,
}

#[derive(Debug, Serialize)]
pub struct NetworkStats {
    pub state: i32, // ConnectionState

    //==//==//==||  ENDPOINT  ||==//==//==//
    pub endpoint: EndpointStats,

    //==//==//==|| CONNECTION ||==//==//==//
    pub connection: Option<ConnectionStats>,

    pub connected_relay: Option<RelayInfo>,

    pub version: u16,
}

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn getInternalConnectionState(_: JNIEnv, _: JC) -> jint {
    CONNECTION_STATE.load(Ordering::Relaxed)
}

fn gather_stats() -> NetworkStats {
    // calling gather stats before initializing api? you deserve the panic
    let endpoint = ENDPOINT.get().unwrap();
    let ep_stats = endpoint.stats();

    let relay = {
        let guard = RELAY.read();
        guard.clone()
    };

    let now = systime().as_secs();

    NetworkStats {
        state: CONNECTION_STATE.load(Ordering::Relaxed),
        endpoint: EndpointStats {
            open_connections: endpoint.open_connections(),
            accepted_handshakes: ep_stats.accepted_handshakes,
            outgoing_handshakes: ep_stats.outgoing_handshakes,
            refused_handshakes: ep_stats.refused_handshakes,
            ignored_handshakes: ep_stats.ignored_handshakes,
            bind_addr: endpoint.local_addr().unwrap().to_string(),
        },
        connected_relay: relay.as_ref().and_then(|r| r.info().ok()),
        connection: relay.and_then(|r| {
            r.connection.map(|conn| {
                let stats = conn.stats();

                ConnectionStats {
                    uptime: now - CONNECTION_START_TIME.load(Ordering::Relaxed),
                    path: stats.path.into(),
                    frame_rx: stats.frame_rx.into(),
                    frame_tx: stats.frame_tx.into(),
                    udp_rx: stats.udp_rx.into(),
                    udp_tx: stats.udp_tx.into(),
                    remote_address: conn.remote_address().to_string(),
                }
            })
        }),
        version: PROTOCOL_VERSION,
    }
}

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn getNetworkStats(env: JNIEnv, _: JC) -> jobject {
    let netstats = gather_stats();
    let mut stats = vec![];

    ciborium::into_writer(&netstats, &mut stats).unwrap();

    let barray = env.byte_array_from_slice(&stats).unwrap();

    barray.into_raw()
}
