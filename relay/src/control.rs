//! Unix-socket control channel for `pzrelay clear-db`. The running daemon holds fjall's
//! single-writer lock, so a second process cannot open the store itself.

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

/// The line protocol is unauthenticated, so the socket's 0600 mode is the authorization. It is
/// bound in a 0700 staging dir and renamed into place, so it is never reachable at another mode.
fn bind_private(sock: &Path) -> Result<UnixListener> {
    let parent = sock.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
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
