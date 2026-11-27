use std::io;
use std::process::Stdio;
use tokio::process::Command;
use tracing::debug;

/// Launch a menu action without waiting for it, then reap it when it exits.
pub fn spawn_reaped(command: &mut Command) -> io::Result<()> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    tokio::spawn(async move {
        if let Err(err) = child.wait().await {
            debug!("Failed to wait for menu action: {err}");
        }
    });
    Ok(())
}
