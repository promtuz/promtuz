use common::quic::CloseReason;
use jni::JNIEnv;
use jni::objects::JObject;
use jni_macro::jni;
use log::debug;
use log::error;

use crate::JC;
use crate::RUNTIME;
use crate::data::ResolverSeeds;
use crate::data::identity::Identity;
use crate::data::identity::IdentitySigner;
use crate::data::relay::Relay;
use crate::data::relay::ResolveError;
use crate::events::Emittable;
use crate::events::connection::ConnectionState;
use crate::jni_try;
use crate::ndk::read_raw_res;
use crate::quic::server::RELAY;
use crate::quic::server::RelayConnError;
use crate::utils::has_internet;

/// Connects to Relay
#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn connect(mut env: JNIEnv, _: JC, context: JObject) {
    if let Some(Some(conn)) = RELAY.read().as_ref().map(|r| r.connection.clone())
        && conn.close_reason().is_some()
    {
        log::warn!("WARNING: connection already exists, ignoring connect request!");
        CloseReason::Reconnecting.close(&conn);
    };

    // Checking Internet Connectivity
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
    let identity_signer = jni_try!(IdentitySigner::new(&mut env));

    RUNTIME.spawn(async move {
        loop {
            match Relay::fetch_best() {
                Ok(relay) => {
                    let id = relay.id.clone();

                    log::trace!("TRACE: connecting to relay({})", id);

                    match relay.connect(ipk, &identity_signer).await {
                        Ok(_) => break,
                        Err(RelayConnError::Continue) => continue,
                        Err(RelayConnError::Error(err)) => {
                            error!("ERROR: connection to relay({}) failed: {}", id, err);
                        },
                    }
                },
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    debug!("DEBUG: relay not found in database, resolving!");
                    match Relay::resolve(&seeds).await {
                        Ok(_) => continue,
                        Err(ResolveError::EmptyResponse) => {
                            error!("ERROR: resolver returned no relays");
                            // either break or a pause to prevent a loop by new users
                            ConnectionState::Failed.emit();
                            break;
                        },
                        Err(err) => {
                            error!("ERROR: resolver failed: {err}");
                        },
                    }
                },
                Err(err) => {
                    error!("ERROR: failed to fetch relay from database: {err}")
                },
            }

            break;
        }
    });
}
