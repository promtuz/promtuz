use std::time::Duration;

use common::debug;
use common::warn;
use quinn::Endpoint;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::quic::handler::Handler;
use crate::relay::RelayRef;

/// Maximum time we wait for in-flight per-connection tasks to wind down on
/// shutdown. After this, surviving tasks are aborted so the process can
/// actually exit. Five seconds matches the resolver's `wait_idle` budget.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Accepts all incoming connections for given endpoint and handles them accordingly
pub struct Acceptor {
    /// Clone of endpoint reference from [Relay]
    endpoint: Endpoint,
}

impl Acceptor {
    pub fn new(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }

    /// Run the accept loop. Per-connection handlers are tracked in a
    /// `JoinSet` so shutdown can cooperatively await them; on `cancel`,
    /// stop accepting new connections, then wait up to `SHUTDOWN_GRACE`
    /// before aborting whatever's left.
    pub async fn run(&self, relay: RelayRef, cancel: CancellationToken) {
        let mut tasks: JoinSet<()> = JoinSet::new();

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    debug!("acceptor: shutdown signal received, draining {} task(s)", tasks.len());
                    break;
                }
                incoming = self.endpoint.accept() => {
                    let Some(conn) = incoming else { break; };
                    let relay = relay.clone();
                    let cancel_child = cancel.clone();
                    tasks.spawn(async move {
                        if let Ok(connection) = conn.await {
                            Handler::handle(connection, relay, cancel_child).await;
                        }
                    });
                }
            }
        }

        // Cooperative drain — handlers that observe `cancel` will return
        // promptly. Anything stuck (e.g. blocked on a syscall, or a packet
        // handler that didn't propagate the token) gets aborted.
        match tokio::time::timeout(SHUTDOWN_GRACE, async {
            while tasks.join_next().await.is_some() {}
        })
        .await
        {
            Ok(()) => debug!("acceptor: all connection tasks drained cleanly"),
            Err(_) => {
                warn!(
                    "acceptor: {} task(s) still running after {:?}, aborting",
                    tasks.len(),
                    SHUTDOWN_GRACE
                );
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
            },
        }
    }
}
