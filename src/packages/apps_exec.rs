// SPDX-License-Identifier: AGPL-3.0-or-later

//! Short-lived confined subprocess execution for app `subprocess` tool handlers
//! (apps framework, Slice 2b).
//!
//! Unlike the native MCP host — which spawns a *long-lived* stdio server through
//! `mira pkg-exec` — an app `subprocess` tool runs a **one-shot** command: feed
//! stdin, capture stdout/stderr (bounded), wall-clock timeout, then read the
//! exit code. The confinement is identical (`launcher::wrap` +
//! [`crate::packages::launcher::ConfineSpec`], built by
//! `install::app_subprocess_confine_spec`), so an app's bundled command gets the
//! same fail-closed sandbox (read-only host, masked secrets, no/allowlisted
//! network) as a native plugin. Linux-only in practice — `wrap` routes through
//! `mira pkg-exec`, whose namespace confinement fail-closes off Linux; the caller
//! refuses non-Linux platforms up front.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::launcher::{self, ConfineSpec};

/// Outcome of a one-shot confined command.
pub struct SubprocResult {
    pub exit_code: i32,
    pub stdout:    String,
    pub stderr:    String,
    pub truncated: bool,
    pub timed_out: bool,
}

/// Spawn `command args` through the confinement launcher, feed `stdin`, capture
/// stdout/stderr up to `cap` bytes each, and kill after `timeout`. `cwd` is the
/// app's install dir; `home` its private writable data dir. Returns `Err` only
/// when the process can't be spawned/awaited — a non-zero exit or a timeout is a
/// normal `Ok` result the caller turns into a tool failure.
#[allow(clippy::too_many_arguments)]
pub async fn run_confined(
    mira_exe: &str,
    command:  &str,
    args:     &[String],
    stdin:    Option<&str>,
    cwd:      &Path,
    home:     &Path,
    spec:     &ConfineSpec,
    timeout:  Duration,
    cap:      usize,
) -> Result<SubprocResult, String> {
    let (cmd, wargs) = launcher::wrap(mira_exe, command, args, spec);

    let mut c = Command::new(&cmd);
    c.args(&wargs)
        .current_dir(cwd)
        // Minimal, predictable environment — the confined command gets a
        // writable HOME (its private data dir) and a standard PATH, nothing else
        // of MIRA's process env (which may hold provider keys).
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin:/usr/local/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = c.spawn().map_err(|e| format!("spawn failed: {e}"))?;

    if let Some(data) = stdin {
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(data.as_bytes()).await;
            let _ = si.shutdown().await;
        }
    }
    // Drain both pipes concurrently so a chatty child never deadlocks on a full
    // pipe while we wait, and cap each stream.
    let out = child.stdout.take().ok_or("no stdout pipe")?;
    let err = child.stderr.take().ok_or("no stderr pipe")?;
    let ho = tokio::spawn(capped_read(out, cap));
    let he = tokio::spawn(capped_read(err, cap));

    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            let (stdout, o_trunc) = ho.await.unwrap_or((String::new(), false));
            let (stderr, e_trunc) = he.await.unwrap_or((String::new(), false));
            Ok(SubprocResult {
                exit_code: status.code().unwrap_or(-1),
                stdout, stderr,
                truncated: o_trunc || e_trunc,
                timed_out: false,
            })
        }
        Ok(Err(e)) => Err(format!("wait failed: {e}")),
        Err(_) => {
            let _ = child.start_kill();
            let (stdout, o_trunc) = ho.await.unwrap_or((String::new(), false));
            let (stderr, e_trunc) = he.await.unwrap_or((String::new(), false));
            Ok(SubprocResult {
                exit_code: -1,
                stdout, stderr,
                truncated: o_trunc || e_trunc,
                timed_out: true,
            })
        }
    }
}

/// Read `src` to EOF, keeping at most `cap` bytes, but always draining the pipe
/// so the child never blocks on a full buffer past the cap.
async fn capped_read<R: tokio::io::AsyncRead + Unpin>(mut src: R, cap: usize) -> (String, bool) {
    let mut kept: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match src.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                if kept.len() < cap {
                    let take = n.min(cap - kept.len());
                    kept.extend_from_slice(&chunk[..take]);
                    if take < n { truncated = true; }
                } else {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (String::from_utf8_lossy(&kept).trim_end().to_string(), truncated)
}
