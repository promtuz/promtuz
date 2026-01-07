use common::crypto::SigningKey;
use common::msg::reason::CloseReason;
use jni::JNIEnv;
use jni::objects::JObject;
use jni_macro::jni;
use log::debug;
use log::error;
use log::info;

use crate::JC;
use crate::RUNTIME;
use crate::data::ResolverSeeds;
use crate::data::identity::Identity;
use crate::data::relay::Relay;
use crate::events::Emittable;
use crate::events::connection::ConnectionState;
use crate::jni_try;
use crate::ndk::read_raw_res;
// use crate::quic::server::KeyPair;
use crate::quic::server::RELAY;
use crate::quic::server::RelayConnError;
use crate::utils::has_internet;

/// Connects to Relay
#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn connect(mut env: JNIEnv, _: JC, context: JObject) {
    if let Some(Some(conn)) = RELAY.read().as_ref().map(|r| r.connection.clone())
        && conn.close_reason().is_some()
    {
        debug!("API: CONNECTION ALREADY EXISTS, CLOSING!");
        CloseReason::Reconnecting.close(&conn);
    };

    info!("API: CONNECTING");

    // Checking Internet Connectivity
    if !has_internet() {
        ConnectionState::NoInternet.emit();
        return;
    }

    let seeds = jni_try!(read_raw_res(&mut env, &context, "resolver_seeds"));
    let seeds = jni_try!(serde_json::from_slice::<ResolverSeeds>(&seeds)).seeds;

    // let ipk = jni_try!(Identity::public_key());
    let isk = jni_try!(Identity::secret_key(&mut env));

    let isk = SigningKey::from_bytes(&isk);

    RUNTIME.spawn(async move {
        loop {
            debug!("RELAY(BEST): Fetching");
            match Relay::fetch_best() {
                Ok(relay) => {
                    let id = relay.id.clone();
                    debug!("RELAY(BEST): Found [{}]", id);
                    // FIXME: temporaily cloned, for future safety only pass public key and move `sign` helper function somewhere else
                    match relay.connect(isk.clone()).await {
                        Ok(_) => break,
                        Err(RelayConnError::Continue) => continue,
                        Err(RelayConnError::Error(err)) => {
                            error!("RELAY({}): Connection failed - {:?}", id, err);
                        },
                    }
                },
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    debug!("RELAY(BEST): Not Found, Resolving");
                    match Relay::resolve(&seeds).await {
                        Ok(_) => continue,
                        Err(err) => {
                            error!("RESOLVE: Failed {err}");
                        },
                    }
                },
                Err(err) => {
                    error!("DB: Relay fetch best failed - {err}")
                },
            }

            break;
        }
    });
}
