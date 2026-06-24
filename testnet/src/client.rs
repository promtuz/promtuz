//! A simulated libcore client subprocess (the `e2e-client` binary), driven
//! over the line protocol on its stdin/stdout: write a command line, read
//! one `ok`/`err` reply line. The client's own logs go to stderr, which we
//! echo with a prefix.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::Lines;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;
use tokio::time::timeout;

pub struct ClientProc {
    pub label: String,
    /// The client's identity public key (hex), learned from its `ready` line.
    pub ipk: String,
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl ClientProc {
    pub async fn spawn(
        label: impl Into<String>, bin: &Path, envs: &[(&str, String)],
    ) -> Result<Self> {
        let label = label.into();
        let mut child = Command::new(bin)
            .envs(envs.iter().map(|(k, v)| (*k, v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {}", bin.display()))?;

        let stdin = child.stdin.take().context("client stdin")?;
        let stdout = child.stdout.take().context("client stdout")?;
        let stderr = child.stderr.take().context("client stderr")?;

        // Echo the client's own logs (stderr) with a prefix.
        {
            let label = label.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("[{label}] {line}");
                }
            });
        }

        let mut stdout = BufReader::new(stdout).lines();
        let ipk = loop {
            match timeout(Duration::from_secs(15), stdout.next_line()).await {
                Ok(Ok(Some(line))) => {
                    if let Some(rest) = line.strip_prefix("ready ") {
                        break rest.trim().to_string();
                    }
                    if line.starts_with("err ") {
                        bail!("{label} startup error: {line}");
                    }
                    // ignore any other early stdout line
                },
                Ok(Ok(None)) => bail!("{label} exited before signalling ready"),
                Ok(Err(e)) => bail!("{label} stdout error before ready: {e}"),
                Err(_) => bail!("{label} timed out before signalling ready"),
            }
        };

        Ok(Self { label, ipk, child, stdin, stdout })
    }

    /// Send one command, await its single `ok`/`err` reply, and return the
    /// payload after `ok <cmd> `.
    pub async fn cmd(&mut self, line: &str) -> Result<String> {
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;

        let resp = match timeout(Duration::from_secs(30), self.stdout.next_line()).await {
            Ok(Ok(Some(l))) => l,
            Ok(Ok(None)) => bail!("{}: stdout closed awaiting reply to `{line}`", self.label),
            Ok(Err(e)) => bail!("{}: stdout error: {e}", self.label),
            Err(_) => bail!("{}: timed out awaiting reply to `{line}`", self.label),
        };

        if let Some(rest) = resp.strip_prefix("ok ") {
            // rest = "<cmd> <data...>"; drop the echoed command token.
            Ok(rest.splitn(2, ' ').nth(1).unwrap_or("").to_string())
        } else if let Some(rest) = resp.strip_prefix("err ") {
            bail!("{}: `{line}` failed: {rest}", self.label)
        } else {
            bail!("{}: unexpected reply to `{line}`: {resp}", self.label)
        }
    }

    pub async fn shutdown(&mut self) {
        let _ = self.cmd("quit").await;
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}
