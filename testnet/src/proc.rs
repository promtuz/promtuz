//! One supervised child process (resolver / relay / client).
//!
//! Spawns a real binary with an isolated working directory, streams its
//! stdout+stderr both to the console (prefixed) and into an in-memory
//! buffer we can assert against, and SIGKILLs it on drop.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::Command;

pub struct NodeProc {
    pub label: String,
    child: Child,
    log: Arc<Mutex<Vec<String>>>,
    /// Flipped when stdout hits EOF — a reliable "it died" signal for these
    /// binaries, which log continuously while alive.
    dead: Arc<AtomicBool>,
}

impl NodeProc {
    pub fn spawn(
        label: impl Into<String>,
        program: &Path,
        args: &[&str],
        cwd: &Path,
        echo: bool,
    ) -> Result<Self> {
        let label = label.into();
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .env("RUST_LOG", std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {}", program.display()))?;

        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let dead = Arc::new(AtomicBool::new(false));

        let stdout = child.stdout.take().context("child stdout pipe")?;
        let stderr = child.stderr.take().context("child stderr pipe")?;
        pump(label.clone(), stdout, log.clone(), Some(dead.clone()), echo);
        pump(label.clone(), stderr, log.clone(), None, echo);

        Ok(Self { label, child, log, dead })
    }

    /// Wait until a captured line contains `needle`; error on timeout or
    /// premature exit. Returns the matching line.
    pub async fn wait_for_log(&self, needle: &str, timeout: Duration) -> Result<String> {
        self.wait_for_any(&[needle], timeout).await
    }

    /// Like [`Self::wait_for_log`] but succeeds on the first line matching
    /// *any* of `needles`.
    pub async fn wait_for_any(&self, needles: &[&str], timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(hit) = self.find(needles) {
                return Ok(hit);
            }
            if self.dead.load(Ordering::SeqCst) {
                // Final scan in case the line and EOF raced.
                if let Some(hit) = self.find(needles) {
                    return Ok(hit);
                }
                return Err(anyhow!("{}: exited before logging any of {:?}", self.label, needles));
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "{}: timed out after {:?} waiting for any of {:?}",
                    self.label,
                    timeout,
                    needles
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn find(&self, needles: &[&str]) -> Option<String> {
        let log = self.log.lock().expect("log mutex");
        log.iter().find(|l| needles.iter().any(|n| l.contains(n))).cloned()
    }

    /// Last `n` captured lines, for failure post-mortems.
    pub fn tail(&self, n: usize) -> Vec<String> {
        let log = self.log.lock().expect("log mutex");
        let start = log.len().saturating_sub(n);
        log[start..].to_vec()
    }

    pub async fn kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

fn pump<R>(label: String, reader: R, log: Arc<Mutex<Vec<String>>>, dead: Option<Arc<AtomicBool>>, echo: bool)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if echo {
                println!("[{label}] {line}");
            }
            log.lock().expect("log mutex").push(line);
        }
        if let Some(dead) = dead {
            dead.store(true, Ordering::SeqCst);
        }
    });
}
