use common::proto::pack::Unpacker;
use common::proto::push::PushRequest;
use common::quic::protorole::ProtoRole;
use common::debug;
use common::warn;
use quinn::Connection;

/// Per-connection handler. Serves the one-RPC-per-bi-stream contract (mirrors
/// the resolver's client handler): each accepted bi-stream is one
/// [`PushRequest`].
///
/// Skeleton cut: it authenticates the ALPN role, decodes the request, and
/// logs it. The `P → token` registry (Register) and the APNs/FCM dispatch
/// (Wake) land in the next cut.
pub struct Handler;

impl Handler {
    pub async fn handle(conn: Connection) {
        let addr = conn.remote_address();
        debug!("incoming conn from {addr}");

        // Only devices (`client/1`, registration) and home relays (`relay/1`,
        // wake) talk to the gateway. Anything else is closed.
        match ProtoRole::from_conn(&conn) {
            Some(ProtoRole::Client | ProtoRole::Relay) => {},
            Some(_) => return conn.close(0u32.into(), b"UnsupportedALPN"),
            None => return conn.close(0u32.into(), b"NoALPN"),
        }

        while let Ok((_send, mut recv)) = conn.accept_bi().await {
            match PushRequest::unpack(&mut recv).await {
                // ponytail: skeleton — registry + FCM dispatch are the next cut.
                Ok(PushRequest::Register(_)) => {
                    warn!("gateway: RegisterToken from {addr} — dispatch not wired yet");
                },
                Ok(PushRequest::Wake(_)) => {
                    warn!("gateway: WakeRequest from {addr} — dispatch not wired yet");
                },
                Err(e) => {
                    warn!("gateway: request decode failed from {addr}: {e}");
                    break;
                },
            }
        }
    }
}
