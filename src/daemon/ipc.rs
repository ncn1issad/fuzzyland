use crate::config::runtime_dir;
use crate::modules::audio::AudioTarget;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub fn socket_path() -> PathBuf {
    runtime_dir().join("daemon.sock")
}

pub fn lock_path() -> PathBuf {
    runtime_dir().join("daemon.lock")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum IpcRequest {
    Ping,
    Network,
    NetworkToggleWifi,
    Bluetooth {
        toggle_power: bool,
    },
    BatteryStatus {
        once: bool,
    },
    BatteryMenu,
    BatteryProfile {
        name: String,
    },
    AudioOutput {
        toggle_eq: bool,
    },
    AudioInput {
        toggle_fx: bool,
    },
    AudioToggleMute {
        target: AudioTarget,
    },
    AudioVolume {
        target: AudioTarget,
        delta: i32,
    },
    BacklightStatus {
        once: bool,
    },
    BacklightAdjust {
        delta: i32,
    },
    BacklightRefresh,
    InternalBacklightAdjust {
        delta: i32,
        device: Option<String>,
    },
    InternalBacklightDim {
        percent: u8,
        device: Option<String>,
    },
    InternalBacklightRestore {
        device: Option<String>,
    },
    Power,
    Clipboard {
        clear: bool,
    },
    Screenshot {
        mode: String,
        save: bool,
        dir: Option<String>,
        delay: f64,
    },
    Media {
        action: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum IpcResponse {
    Ok {
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        #[serde(default)]
        trailing_newline: bool,
    },
    StreamLine {
        line: String,
    },
    Error {
        message: String,
    },
}

impl IpcResponse {
    pub fn ok_empty() -> Self {
        Self::Ok {
            output: None,
            trailing_newline: false,
        }
    }

    pub fn ok_line(output: impl Into<String>) -> Self {
        Self::Ok {
            output: Some(output.into()),
            trailing_newline: true,
        }
    }

    pub fn ok_raw(output: Option<String>) -> Self {
        Self::Ok {
            output,
            trailing_newline: false,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
        }
    }
}

pub async fn write_response<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    resp: &IpcResponse,
) -> Result<()> {
    let mut line = serde_json::to_string(resp)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Attempts to send a request to the running daemon over Unix socket.
/// Returns `Ok(None)` when the daemon is not running so the caller can fall back
/// transparently to standalone execution.
pub async fn try_send_request(req: &IpcRequest) -> Result<Option<i32>> {
    let path = socket_path();
    let stream = match UnixStream::connect(&path).await {
        Ok(s) => s,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(err.into()),
    };

    let (read_half, mut write_half) = stream.into_split();
    let mut payload = serde_json::to_string(req)?;
    payload.push('\n');
    write_half.write_all(payload.as_bytes()).await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let mut stdout = std::io::stdout();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(Some(0));
        }
        let resp: IpcResponse = serde_json::from_str(line.trim_end())?;
        match resp {
            IpcResponse::StreamLine { line } => {
                if writeln!(stdout, "{line}").is_err() || stdout.flush().is_err() {
                    return Ok(Some(0));
                }
            }
            IpcResponse::Ok {
                output,
                trailing_newline,
            } => {
                if let Some(text) = output {
                    if trailing_newline {
                        let _ = writeln!(stdout, "{text}");
                    } else {
                        let _ = write!(stdout, "{text}");
                    }
                    let _ = stdout.flush();
                }
                return Ok(Some(0));
            }
            IpcResponse::Error { message } => {
                eprintln!("{message}");
                return Ok(Some(1));
            }
        }
    }
}
