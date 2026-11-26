use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Deserialize)]
pub struct CursorPos {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WorkspaceRef {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Monitor {
    pub id: i64,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub transform: u8,
    pub focused: bool,
    #[serde(rename = "activeWorkspace")]
    pub active_workspace: WorkspaceRef,
    #[serde(rename = "specialWorkspace")]
    pub special_workspace: WorkspaceRef,
}

impl Default for Monitor {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            width: 0,
            height: 0,
            x: 0,
            y: 0,
            scale: 1.0,
            transform: 0,
            focused: false,
            active_workspace: WorkspaceRef::default(),
            special_workspace: WorkspaceRef::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Client {
    pub address: String,
    pub mapped: bool,
    pub hidden: bool,
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub workspace: WorkspaceRef,
    pub title: String,
    pub class: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ActiveWindow {
    pub address: String,
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub title: String,
    pub class: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyprEvent {
    pub event: String,
    pub data: String,
}

fn hypr_socket_path(filename: &str) -> Option<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    if sig.is_empty() {
        return None;
    }
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        if !runtime.is_empty() {
            let p = PathBuf::from(runtime).join("hypr").join(&sig).join(filename);
            if p.exists() {
                return Some(p);
            }
        }
    }
    let fallback = PathBuf::from("/tmp/hypr").join(&sig).join(filename);
    if fallback.exists() {
        return Some(fallback);
    }
    None
}

pub fn is_available() -> bool {
    hypr_socket_path(".socket.sock").is_some()
}

pub async fn query_raw(command: &str) -> Result<String> {
    let path = hypr_socket_path(".socket.sock")
        .ok_or_else(|| anyhow!("Hyprland IPC socket not available"))?;

    tokio::time::timeout(QUERY_TIMEOUT, async move {
        let mut stream = UnixStream::connect(&path)
            .await
            .with_context(|| format!("Failed to connect to {}", path.display()))?;
        stream.write_all(command.as_bytes()).await?;
        stream.shutdown().await?;
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    })
    .await
    .map_err(|_| anyhow!("Hyprland IPC query timed out: {command}"))?
}

pub async fn cursor_pos() -> Option<CursorPos> {
    let raw = query_raw("j/cursorpos").await.ok()?;
    serde_json::from_str(&raw).ok()
}

pub async fn monitors() -> Vec<Monitor> {
    let Ok(raw) = query_raw("j/monitors").await else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub async fn clients() -> Vec<Client> {
    let Ok(raw) = query_raw("j/clients").await else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub async fn active_window() -> Option<ActiveWindow> {
    let raw = query_raw("j/activewindow").await.ok()?;
    let win: ActiveWindow = serde_json::from_str(&raw).ok()?;
    if win.address.is_empty() {
        None
    } else {
        Some(win)
    }
}

pub async fn dispatch(args: &str) -> Result<String> {
    query_raw(&format!("dispatch {args}")).await
}

pub fn find_monitor_at_cursor(pos: &CursorPos, monitors: &[Monitor]) -> Option<String> {
    for mon in monitors {
        let scale = if mon.scale > 0.0 { mon.scale } else { 1.0 };
        let mut w = f64::from(mon.width) / scale;
        let mut h = f64::from(mon.height) / scale;
        if matches!(mon.transform, 1 | 3 | 5 | 7) {
            std::mem::swap(&mut w, &mut h);
        }
        let mx = f64::from(mon.x);
        let my = f64::from(mon.y);
        if pos.x >= mx && pos.x < mx + w && pos.y >= my && pos.y < my + h {
            return Some(mon.name.clone());
        }
    }
    None
}

pub async fn detect_cursor_output() -> Option<String> {
    let mons = monitors().await;
    if mons.is_empty() {
        return None;
    }
    if let Some(pos) = cursor_pos().await {
        if let Some(name) = find_monitor_at_cursor(&pos, &mons) {
            return Some(name);
        }
    }
    if let Some(focused) = mons.iter().find(|m| m.focused) {
        if !focused.name.is_empty() {
            return Some(focused.name.clone());
        }
    }
    mons.first()
        .map(|m| m.name.clone())
        .filter(|n| !n.is_empty())
}

pub struct HyprEventStream {
    reader: BufReader<UnixStream>,
}

impl HyprEventStream {
    pub async fn connect() -> Result<Self> {
        let path = hypr_socket_path(".socket2.sock")
            .ok_or_else(|| anyhow!("Hyprland event socket2 not available"))?;
        let stream = UnixStream::connect(&path)
            .await
            .with_context(|| format!("Failed to connect to {}", path.display()))?;
        Ok(Self {
            reader: BufReader::new(stream),
        })
    }

    pub async fn next_event(&mut self) -> Result<Option<HyprEvent>> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line).await?;
            if n == 0 {
                return Ok(None);
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if let Some((event, data)) = trimmed.split_once(">>") {
                return Ok(Some(HyprEvent {
                    event: event.to_string(),
                    data: data.to_string(),
                }));
            }
        }
    }
}
