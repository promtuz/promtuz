//! The client core: one owner for the databases, the host ports, the relay session and the
//! background tasks.

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
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
    /// The parent of every session's token; cancelling it stops the long-lived loops.
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
        }
    }

    /// Runs `task` on the runtime, counted by `tasks` until it finishes.
    pub fn spawn<F>(&self, task: F) -> JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.tasks.spawn_on(task, &self.runtime)
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
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(backoff) => {},
                }
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
