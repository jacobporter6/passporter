//! Running the CLI: fresh empty working directory, scrubbed environment,
//! input on stdin, capped output, timeout with kill.

use std::io;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::invocation::Invocation;
use super::output::Finished;
use crate::error::{Error, Result};

/// Longest stderr kept, in bytes (the rest is read and dropped).
const MAX_STDERR_BYTES: usize = 16 * 1024;

/// Prefix of the per-run temporary working directory.
pub(crate) const TEMP_DIR_PREFIX: &str = "passporter-summarize-";

/// Runs `invocation` with `input` on stdin in a fresh, empty temporary
/// directory (removed afterwards), killing it after `timeout`.
pub(crate) async fn run(
    invocation: &Invocation,
    input: &[u8],
    timeout: Duration,
    max_stdout: usize,
) -> Result<Finished> {
    let dir = tempfile::Builder::new()
        .prefix(TEMP_DIR_PREFIX)
        .tempdir()
        .map_err(|e| Error::TempDir(Arc::new(e)))?;
    let finished = run_in(invocation, dir.path(), input, timeout, max_stdout).await;
    // Best effort: the process is gone, so nothing should be holding it.
    let _ = dir.close();
    let finished = finished?;
    if finished.stdout.len() > max_stdout {
        return Err(Error::OutputTooLarge { limit: max_stdout });
    }
    Ok(finished)
}

async fn run_in(
    invocation: &Invocation,
    cwd: &Path,
    input: &[u8],
    timeout: Duration,
    max_stdout: usize,
) -> Result<Finished> {
    let mut child = Command::new(&invocation.program)
        .args(&invocation.args)
        .env_clear()
        .envs(invocation.env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| Error::Spawn {
            program: invocation.program.clone(),
            source: Arc::new(source),
        })?;
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(Error::io(io::Error::other("child stdio was not piped")));
    };

    let exchange = async {
        let write = async {
            if let Err(e) = stdin.write_all(input).await {
                // The CLI may exit (e.g. on a startup error) without reading.
                if e.kind() != io::ErrorKind::BrokenPipe {
                    return Err(e);
                }
            }
            drop(stdin);
            Ok::<_, io::Error>(())
        };
        // One byte over the limit is enough to know it was exceeded.
        let ((), stdout, stderr) = tokio::try_join!(
            write,
            read_capped(stdout, max_stdout.saturating_add(1)),
            read_capped(stderr, MAX_STDERR_BYTES),
        )?;
        let status = child.wait().await?;
        Ok::<_, io::Error>(Finished {
            status,
            stdout,
            stderr,
        })
    };
    match tokio::time::timeout(timeout, exchange).await {
        Ok(finished) => finished.map_err(Error::io),
        Err(_elapsed) => {
            // SIGKILL, then reap so no zombie is left behind.
            let _ = child.kill().await;
            Err(Error::Timeout(timeout))
        }
    }
}

/// Reads `reader` to the end, keeping at most `cap` bytes, so a chatty
/// process never blocks on a full pipe and never grows memory unbounded.
async fn read_capped(mut reader: impl AsyncRead + Unpin, cap: usize) -> io::Result<Vec<u8>> {
    let mut kept = Vec::new();
    // On the heap: an 8 KiB array would bloat every enclosing future.
    let mut buf = vec![0_u8; 8192];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(kept);
        }
        let room = cap.saturating_sub(kept.len());
        kept.extend_from_slice(&buf[..n.min(room)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_capped_drains_but_keeps_only_the_cap() {
        let data = vec![7_u8; 100_000];
        let kept = read_capped(&data[..], 10).await.unwrap();
        assert_eq!(kept, vec![7_u8; 10]);
    }
}
