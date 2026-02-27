use std::time::Duration;

use common::quic::CloseReason;
use jni::JNIEnv;
use jni::objects::JObject;
use jni_macro::jni;
use log::debug;
use log::error;
use log::trace;

use crate::JC;
use crate::RUNTIME;
use crate::data::ResolverSeeds;
use crate::data::identity::Identity;
use crate::data::relay::Relay;
use crate::data::relay::RelayError;
use crate::data::relay::ResolveError;
use crate::events::Emittable;
use crate::events::connection::ConnectionState;
use crate::jni_try;
use crate::ndk::read_raw_res;
use crate::quic::server::RELAY;
use crate::quic::server::RelayConnError;
use crate::utils::has_internet;

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn connect(mut env: JNIEnv, _: JC, context: JObject) {
    // Close any stale connection before attempting a new one
    if let Some(conn) = RELAY.read().as_ref().and_then(|r| r.connection.clone())
        && conn.close_reason().is_none()
    {
        log::warn!("connection already active, closing before reconnect");
        CloseReason::Reconnecting.close(&conn);
    }

    if !has_internet() {
        ConnectionState::NoInternet.emit();
        return;
    }

    let seeds = jni_try!(
        read_raw_res(&mut env, &context, "resolver_seeds")
            .and_then(|bytes| Ok(String::from_utf8(bytes)?))
            .and_then(|str| ResolverSeeds::from_str(&str))
    );

    let ipk = jni_try!(Identity::public_key());

    RUNTIME.spawn(async move {
        loop {
            match Relay::fetch_best() {
                Ok(relay) => {
                    let id = relay.id.clone();
                    trace!("connecting to relay({})", id);

                    match relay.connect(ipk).await {
                        Ok(handle) => {
                            match handle.await {
                                Ok(conn_err) => {
                                    error!("relay({}) connection closed: {conn_err}", id)
                                },
                                Err(join_err) => {
                                    error!("relay({}) handle join failed: {join_err}", id)
                                },
                            }
                            // connection dropped, fall through to reconnect
                        },
                        Err(RelayConnError::Continue) => {},
                        Err(RelayConnError::Error(err)) => {
                            error!("relay({}) connect error: {err}", id);
                        },
                    }
                },
                Err(RelayError::NoneAvailable) => {
                    debug!("no relays in database, resolving");
                    match Relay::resolve(&seeds).await {
                        Ok(_) => {},
                        Err(ResolveError::EmptyResponse) => {
                            error!("resolver returned no relays");
                            ConnectionState::Failed.emit();
                            return;
                        },
                        Err(err) => error!("resolver failed: {err}"),
                    }
                },
                Err(err) => {
                    error!("failed to fetch relay: {err}");
                },
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}
