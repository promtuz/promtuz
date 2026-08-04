//! Unix-socket control channel: lets `pzrelay <subcommand>` drive the running
//! daemon, which holds the fjall single-writer lock a second process can't take.
//! Today it serves `clear-db`; the dispatch is a plain line protocol so more
//! commands (info, reload, …) drop in as new match arms.

use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use common::error;
use common::info;
use common::warn;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

use crate::storage::db::Store;

/// Daemon side: bind the control socket at `sock` and dispatch commands until
/// cancelled. Best-effort — a bind failure is logged and the daemon runs on,
/// with `pzrelay clear-db` unavailable.
pub async fn serve(store: Arc<Store>, sock: PathBuf, cancel: CancellationToken) {
    let listener = match bind_private(&sock) {
        Ok(l) => l,
        Err(e) => {
            error!("control socket bind {} failed: {e:#}", sock.display());
            return;
        },
    };
    info!("control socket at {}", sock.display());

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let store = store.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conn(stream, store).await {
                            warn!("control conn: {e}");
                        }
                    });
                },
                Err(e) => warn!("control accept: {e}"),
            },
        }
    }
    let _ = std::fs::remove_file(&sock);
}

/// Bind at `sock` with mode 0600 and no window in which it is reachable at
/// any other mode.
///
/// `clear-db` is destructive and the line protocol is unauthenticated, so the
/// socket's file mode IS the authz: 0600 restricts it to the daemon's own uid,
/// plus root (an admin's `sudo pzrelay clear-db`). The bind therefore happens
/// inside a 0700 staging dir no other user can traverse, and the finished
/// socket is renamed into place — `rename` is atomic and the listening fd is
/// unaffected by the move.
fn bind_private(sock: &Path) -> Result<UnixListener> {
    let parent = sock.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent) // no-op for /run/pzrelay (RuntimeDirectory)
        .with_context(|| format!("create {}", parent.display()))?;

    let staging = parent.join(format!(".pzrelay-control.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&staging)
        .with_context(|| format!("create {}", staging.display()))?;

    let staged_sock = staging.join("sock");
    let bound = (|| -> Result<UnixListener> {
        let listener = UnixListener::bind(&staged_sock).context("bind")?;
        std::fs::set_permissions(&staged_sock, std::fs::Permissions::from_mode(0o600))
            .context("chmod 0600")?;
        let _ = std::fs::remove_file(sock); // clear a stale socket from a crash
        std::fs::rename(&staged_sock, sock).context("publish")?;
        Ok(listener)
    })();

    let _ = std::fs::remove_dir_all(&staging);
    bound
}

async fn handle_conn(mut stream: UnixStream, store: Arc<Store>) -> Result<()> {
    let (rd, mut wr) = stream.split();
    let mut cmd = String::new();
    BufReader::new(rd).read_line(&mut cmd).await.context("read command")?;

    let reply = match cmd.trim() {
        "clear-db" => match store.clear_all() {
            Ok(n) => format!("ok: cleared {n} entries\n"),
            Err(e) => format!("error: clear-db: {e}\n"),
        },
        other => format!("error: unknown command '{other}'\n"),
    };
    wr.write_all(reply.as_bytes()).await.context("write reply")?;
    Ok(())
}

/// Client side of `pzrelay clear-db`: send the command, print the daemon's reply.
pub async fn clear_db_client(sock: &Path) -> Result<()> {
    let mut stream = UnixStream::connect(sock)
        .await
        .with_context(|| format!("connect {} — is the relay running?", sock.display()))?;
    stream.write_all(b"clear-db\n").await.context("send command")?;

    let mut reply = String::new();
    stream.read_to_string(&mut reply).await.context("read reply")?;
    print!("{reply}");
    if reply.starts_with("error") {
        anyhow::bail!("clear-db failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> PathBuf {
        // Short prefix: a sun_path is capped at ~104 bytes.
        let dir = PathBuf::from("/tmp").join(format!("pz-ctl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn published_socket_is_owner_only_and_connectable() {
        let dir = scratch_dir("mode");
        let sock = dir.join("control.sock");

        let listener = bind_private(&sock).unwrap();
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;

        assert_eq!(mode, 0o600);
        assert!(UnixStream::connect(&sock).await.is_ok());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "staging dir left behind");

        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rebinds_over_a_socket_left_by_a_crash() {
        let dir = scratch_dir("stale");
        let sock = dir.join("control.sock");

        let first = bind_private(&sock).unwrap();
        drop(first);
        let second = bind_private(&sock);

        assert!(second.is_ok());
        assert!(UnixStream::connect(&sock).await.is_ok());

        drop(second);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
