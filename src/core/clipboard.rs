use anyhow::{anyhow, Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const COPY_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn copy_text(text: &str) -> Result<()> {
    copy_bytes(text.as_bytes(), None).await
}

pub async fn copy_bytes(data: &[u8], mime_type: Option<&str>) -> Result<()> {
    let mut cmd = Command::new("wl-copy");
    if let Some(mime) = mime_type {
        cmd.arg("--type").arg(mime);
    }
    // Null stdout/stderr so wl-copy's forked daemon does not keep pipes open and hang waiters.
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = cmd
        .spawn()
        .context("Failed to spawn wl-copy (is wl-clipboard installed?)")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(data).await?;
        stdin.shutdown().await?;
    }

    let status = tokio::time::timeout(COPY_TIMEOUT, child.wait())
        .await
        .map_err(|_| anyhow!("wl-copy timed out"))??;

    if !status.success() {
        return Err(anyhow!("wl-copy exited with status {status}"));
    }
    Ok(())
}

pub async fn clear() -> Result<()> {
    let status = tokio::time::timeout(
        COPY_TIMEOUT,
        Command::new("wl-copy")
            .arg("--clear")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await
    .map_err(|_| anyhow!("wl-copy --clear timed out"))??;

    if !status.success() {
        return Err(anyhow!("wl-copy --clear exited with status {status}"));
    }
    Ok(())
}
