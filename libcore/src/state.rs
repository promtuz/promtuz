//! The client core: one owner for the databases, the host ports, the relay session and the
//! background tasks.

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use parking_lot::Mutex;
use parking_lot::RwLock;
use quinn::Endpoint;
use tokio::runtime::Handle;
use tokio::runtime::Runtime;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::data::ResolverSeed;
use crate::data::identity::CachedIsk;
use crate::db::Stores;
use crate::platform::CoreEvents;
use crate::platform::SecureStore;
use crate::quic::dialer::ClientConfigs;
use crate::quic::server::Session;

pub struct Core {
    pub db: Stores,
    /// The runtime every core task runs on.
    pub runtime: Handle,
    /// Set by `init`. Events before it are dropped.
    pub events: OnceLock<Arc<dyn CoreEvents>>,
    pub secure_store: OnceLock<Arc<dyn SecureStore>>,
    pub(crate) net: OnceLock<Net>,
    /// Counts every task core spawns.
    pub tasks: TaskTracker,
    /// Cancelling it ends the relay session and every task core spawned.
    pub cancel: CancellationToken,
    session: RwLock<Option<Arc<Session>>>,
    /// Relay id the user asked to connect to. The relay loop takes it on its next pick instead of
    /// a weighted-random choice.
    preferred_relay: Mutex<Option<String>>,
    /// Cuts the relay loop's retry sleep short so a reconnect fires at once.
    pub(crate) foreground: Notify,
    pub(crate) relay_probe: tokio::sync::Mutex<()>,
    pub(crate) task_removed: AtomicBool,
    pub(crate) network_change_pending: AtomicBool,
    /// Serializes inbox drains across sessions: the relay tracks one batch.
    pub(crate) inbox_sync: tokio::sync::Mutex<()>,
    /// Starts idle: a headless push wake stays idle until the UI foregrounds it.
    pub(crate) presence_idle: AtomicBool,
    pub(crate) presence_update: tokio::sync::Mutex<()>,
    pub(crate) isk: RwLock<Option<CachedIsk>>,
    pub(crate) avatar_generation: AtomicU64,
    pub(crate) kp_publish_ready: AtomicBool,
    pub(crate) mls_operations: [Mutex<()>; 64],
    pub(crate) profile_changed: Notify,
    pub(crate) profile_publish: AtomicBool,
    pub(crate) profile_refresh: Mutex<std::collections::BTreeSet<[u8; 32]>>,
    pub(crate) profile_update: tokio::sync::Mutex<()>,
    pub(crate) p2p: crate::p2p::P2p,
    pub(crate) calls: crate::call::Calls,
    pub(crate) transfers: crate::transfer::Transfers,
    pub(crate) stickers: crate::stickers::Stickers,
    pub(crate) push: crate::push::Push,
    pub(crate) staging: crate::staging::Staging,
    pub(crate) groups: crate::groups::Groups,
    pub(crate) messaging: crate::messaging::Messaging,
    pub(crate) receipts: crate::data::receipts::Receipts,
}

/// What `init` builds for reaching relays, resolvers and gateways.
pub(crate) struct Net {
    pub endpoint: Endpoint,
    pub dialer:   ClientConfigs,
    pub seeds:    Vec<ResolverSeed>,
}

impl Core {
    pub fn new(db: Stores, runtime: Handle) -> Self {
        Self {
            db,
            runtime,
            events: OnceLock::new(),
            secure_store: OnceLock::new(),
            net: OnceLock::new(),
            tasks: TaskTracker::new(),
            cancel: CancellationToken::new(),
            session: RwLock::new(None),
            preferred_relay: Mutex::new(None),
            foreground: Notify::new(),
            relay_probe: tokio::sync::Mutex::new(()),
            task_removed: AtomicBool::new(false),
            network_change_pending: AtomicBool::new(false),
            inbox_sync: tokio::sync::Mutex::new(()),
            presence_idle: AtomicBool::new(true),
            presence_update: tokio::sync::Mutex::new(()),
            isk: RwLock::new(None),
            avatar_generation: AtomicU64::new(0),
            kp_publish_ready: AtomicBool::new(false),
            mls_operations: [const { Mutex::new(()) }; 64],
            profile_changed: Notify::new(),
            profile_publish: AtomicBool::new(true),
            profile_refresh: Mutex::default(),
            profile_update: tokio::sync::Mutex::new(()),
            p2p: Default::default(),
            calls: Default::default(),
            transfers: Default::default(),
            stickers: Default::default(),
            push: Default::default(),
            staging: Default::default(),
            groups: Default::default(),
            messaging: Default::default(),
            receipts: Default::default(),
        }
    }

    /// Runs `task` on the runtime, counted by `tasks` until it finishes or `cancel` drops it.
    pub fn spawn<F>(&self, task: F) -> JoinHandle<Option<F::Output>>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.tasks.spawn_on(self.cancel.clone().run_until_cancelled_owned(task), &self.runtime)
    }

    pub fn spawn_blocking<F, T>(&self, task: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.tasks.spawn_blocking_on(task, &self.runtime)
    }

    /// Keeps a long-lived loop running until it returns `Ok` or `cancel` fires. An error or a
    /// panic is logged and the loop starts again after a backoff.
    pub(crate) fn supervise<F, Fut>(&self, name: &'static str, run: F)
    where
        F: Fn(CancellationToken) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let cancel = self.cancel.child_token();
        let tasks = self.tasks.clone();
        let runtime = self.runtime.clone();
        self.spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                match tasks.spawn_on(run(cancel.clone()), &runtime).await {
                    Ok(Ok(())) => return,
                    Ok(Err(e)) => log::warn!("{name} failed, restarting: {e:#}"),
                    Err(e) => log::warn!("{name} stopped, restarting: {e}"),
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        });
    }

    pub fn session(&self) -> Option<Arc<Session>> {
        self.session.read().clone()
    }

    pub(crate) fn publish(&self, session: Arc<Session>) {
        *self.session.write() = Some(session);
    }

    /// Clears `session` if it is still the published one. A replacement is left alone.
    pub(crate) fn retire(&self, session: &Session) -> bool {
        let mut current = self.session.write();
        if current.as_ref().is_some_and(|s| s.conn.stable_id() == session.conn.stable_id()) {
            *current = None;
            true
        } else {
            false
        }
    }

    pub fn set_preferred_relay(&self, id: String) {
        *self.preferred_relay.lock() = Some(id);
    }

    pub fn take_preferred_relay(&self) -> Option<String> {
        self.preferred_relay.lock().take()
    }
}

static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| Runtime::new().unwrap());

static CORE: LazyLock<Core> =
    LazyLock::new(|| Core::new(Stores::open_default(), RUNTIME.handle().clone()));

#[cfg(test)]
thread_local! {
    /// A test's own core, installed by `test_support::ScopedCore`.
    pub(crate) static SCOPED: std::cell::Cell<Option<&'static Core>> =
        const { std::cell::Cell::new(None) };
}

/// The process's core. The FFI reaches it before `init`, so it is built on first use.
pub fn core() -> &'static Core {
    #[cfg(test)]
    if let Some(core) = SCOPED.get() {
        return core;
    }
    &CORE
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use common::proto::mls_wire::CallMsg;
    use common::utils::now_ms;
    use ed25519_dalek::SigningKey;

    use crate::call::Phase;
    use crate::data::contact::Contact;
    use crate::data::conversation::Conversation;
    use crate::data::message::Message;
    use crate::p2p::diagnostics::Event;
    use crate::test_support::ScopedCore;
    use crate::test_support::data::identity;
    use crate::test_support::transfer::attachment;

    /// Without a relay, the P2P endpoint's loops never end, a download sleeps between retries and
    /// an answered call waits 20 s for media. Cancelling the core stops all of them at once.
    #[tokio::test]
    async fn cancelling_the_core_stops_p2p_loops_a_download_and_a_call() {
        let _ = common::quic::config::setup_crypto_provider();
        let scope = ScopedCore::new();
        let core = scope.core;
        let me = identity(&core.db.identity().lock(), 0x61).verifying_key().to_bytes();
        let peer = SigningKey::from_bytes(&[0x62; 32]).verifying_key().to_bytes();
        {
            let contacts = core.db.contacts().lock();
            Contact::save_pending_tx(&contacts, peer, "peer", 0).unwrap();
            Contact::mark_paired_tx(&contacts, &peer).unwrap();
        }
        let (file, dispatch) = ([0x63; 32], [0x64; 16]);
        let chat = {
            let db = core.db.messages().lock();
            let chat = Conversation::join_group_tx(&db, &peer, &[peer, me]).unwrap();
            Message::save_incoming_tx(&db, chat, peer, &dispatch, "", 0, None).unwrap();
            crate::data::media::save_tx(&db, &chat, &dispatch, &attachment(file, 4096)).unwrap();
            chat
        };

        assert!(crate::p2p::link(peer).await.is_err(), "no relay carries the offer");
        crate::api::media::download_attachment(file.to_vec()).unwrap();
        let call = [0x65; 16];
        let offer = CallMsg::Offer {
            call,
            expires_at_ms: now_ms() + 30_000,
            video: false,
            ufrag: "ufrag".into(),
            pwd: "pwd-of-twenty-four-chars".into(),
            fingerprint: [0; 32],
            ssrc: 1,
            video_ssrc: 0,
            candidates: Vec::new(),
        };
        crate::call::on_signal(peer, chat, offer);
        crate::call::accept(call).unwrap();
        let recorded = |event: fn(&Event) -> bool| {
            crate::p2p::diagnostics::snapshot().events.iter().any(|(_, e)| event(e))
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !recorded(|e| matches!(e, Event::TransportRetry)) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the download waits to retry");
        assert!(recorded(|e| matches!(e, Event::SignalingFailed)), "the endpoint is up");
        assert!(crate::call::current().is_some_and(|c| c.phase == Phase::Connecting));

        core.cancel.cancel();
        core.tasks.close();
        let stopped = tokio::time::timeout(Duration::from_millis(500), core.tasks.wait()).await;
        assert!(stopped.is_ok(), "{} tasks still running", core.tasks.len());
    }
}
