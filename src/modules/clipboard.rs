use crate::config::IconsConfig;
use crate::core::clipboard as wl_clipboard;
use crate::core::fuzzel::{self, Anchor, FuzzelOptions};
use crate::core::hyprland;
use crate::core::notify::{Notification, Urgency};
use crate::daemon::menu::{MenuGuard, MenuKind, MenuSession};
use crate::daemon::AppContext;
use anyhow::Result;
use std::fmt;
use std::process::{Output, Stdio};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::warn;

const CLIPHIST_TIMEOUT: Duration = Duration::from_secs(5);
const APP_NAME: &str = "Clipboard";
const SYNC_ID: &str = "waybar-clipboard";

const EXIT_ACCEPT: i32 = 0;
const EXIT_DELETE: i32 = 10; // custom-1: Shift+Delete
const EXIT_CLEAR: i32 = 11; // custom-2: Control+Shift+Delete

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliphistEntry {
    raw_line: String,
    preview: String,
}

#[derive(Debug)]
enum CliphistError {
    NotInstalled,
    TimedOut,
    ExitFailed(Option<i32>, String),
    Io(std::io::Error),
    Clipboard(anyhow::Error),
}

impl fmt::Display for CliphistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => write!(f, "cliphist is not installed"),
            Self::TimedOut => write!(f, "cliphist command timed out"),
            Self::ExitFailed(code, stderr) => {
                if stderr.is_empty() {
                    write!(f, "cliphist exited with status {code:?}")
                } else {
                    write!(f, "cliphist exited with status {code:?}: {stderr}")
                }
            }
            Self::Io(err) => write!(f, "cliphist I/O error: {err}"),
            Self::Clipboard(err) => write!(f, "{err}"),
        }
    }
}

pub async fn run_menu(ctx: &AppContext) -> Result<()> {
    let MenuSession::Acquired(guard) = ctx.menu.begin_session(MenuKind::Clipboard) else {
        return Ok(());
    };

    let output = hyprland::detect_cursor_output().await;
    let mut select_idx: usize = 0;

    loop {
        if !guard.is_current() {
            return Ok(());
        }

        let entries = match list_cliphist().await {
            Ok(entries) => entries,
            Err(CliphistError::NotInstalled) => {
                notify(ctx, "Clipboard error", "cliphist is not installed", Urgency::Normal).await;
                return Ok(());
            }
            Err(err) => {
                warn!("Failed to list cliphist entries: {err}");
                notify(
                    ctx,
                    "Clipboard error",
                    "Failed to read clipboard history",
                    Urgency::Normal,
                )
                .await;
                return Ok(());
            }
        };

        if entries.is_empty() {
            let empty_row = format!("{}  Clipboard history is empty", ctx.config.icons.clipboard);
            let opts = FuzzelOptions {
                prompt: format!("{}  ", ctx.config.icons.clipboard),
                placeholder: Some("Copy something to start recording...".to_string()),
                anchor: Anchor::Top,
                width: Some(42),
                max_lines: Some(1),
                minimal_lines: true,
                no_sort: true,
                output: output.clone(),
                ..Default::default()
            };
            let _ = fuzzel::select_raw(&ctx.config.fuzzel, &guard, &[empty_row], &opts).await?;
            return Ok(());
        }

        let mut lines: Vec<String> = entries
            .iter()
            .map(|entry| format_entry(&entry.preview, &ctx.config.icons))
            .collect();
        let clear_row_idx = lines.len();
        lines.push(format!("{}  Clear history…", ctx.config.icons.trash));

        let clamped_idx = select_idx.min(entries.len() - 1);
        let opts = FuzzelOptions {
            prompt: format!("{}  ", ctx.config.icons.clipboard),
            placeholder: Some("Search clipboard...".to_string()),
            mesg: Some("Enter: copy · Shift+Del: delete · Ctrl+Shift+Del: clear".to_string()),
            anchor: Anchor::Top,
            width: Some(56),
            max_lines: Some(12),
            minimal_lines: true,
            no_sort: true,
            counter: true,
            select_index: Some(clamped_idx),
            output: output.clone(),
            overrides: vec![
                "key-bindings.expunge=none".to_string(),
                "key-bindings.custom-1=Shift+Delete".to_string(),
                "key-bindings.custom-2=Control+Shift+Delete".to_string(),
            ],
            ..Default::default()
        };

        let res = fuzzel::select_raw(&ctx.config.fuzzel, &guard, &lines, &opts).await?;

        if res.exit_code == EXIT_CLEAR {
            confirm_clear(ctx, &guard, output.as_deref(), entries.len()).await?;
            return Ok(());
        }

        let Some(idx) = res.index else {
            return Ok(());
        };
        if idx >= lines.len() {
            return Ok(());
        }

        if idx == clear_row_idx {
            if res.exit_code == EXIT_ACCEPT {
                confirm_clear(ctx, &guard, output.as_deref(), entries.len()).await?;
            }
            return Ok(());
        }

        let entry = &entries[idx];

        match res.exit_code {
            EXIT_ACCEPT => {
                match copy_entry(&entry.raw_line).await {
                    Ok(()) => {
                        let body = truncate_chars(&sanitize_preview(&entry.preview), 80);
                        notify(ctx, "Copied from history", &body, Urgency::Low).await;
                    }
                    Err(CliphistError::NotInstalled) => {
                        notify(
                            ctx,
                            "Clipboard error",
                            "cliphist is not installed",
                            Urgency::Normal,
                        )
                        .await;
                    }
                    Err(err) => {
                        warn!("Failed to copy clipboard entry: {err}");
                        notify(
                            ctx,
                            "Clipboard error",
                            "Failed to copy entry to clipboard",
                            Urgency::Normal,
                        )
                        .await;
                    }
                }
                return Ok(());
            }
            EXIT_DELETE => {
                match delete_entry(&entry.raw_line).await {
                    Ok(()) => {}
                    Err(CliphistError::NotInstalled) => {
                        notify(
                            ctx,
                            "Clipboard error",
                            "cliphist is not installed",
                            Urgency::Normal,
                        )
                        .await;
                        return Ok(());
                    }
                    Err(err) => {
                        warn!("Failed to delete clipboard entry: {err}");
                        notify(
                            ctx,
                            "Clipboard error",
                            "Failed to delete entry from clipboard history",
                            Urgency::Normal,
                        )
                        .await;
                        return Ok(());
                    }
                }
                select_idx = idx;
            }
            _ => return Ok(()),
        }
    }
}

pub async fn clear(ctx: &AppContext) -> Result<()> {
    let MenuSession::Acquired(guard) = ctx.menu.begin_session(MenuKind::Clipboard) else {
        return Ok(());
    };

    let output = hyprland::detect_cursor_output().await;
    let entries = match list_cliphist().await {
        Ok(entries) => entries,
        Err(CliphistError::NotInstalled) => {
            notify(ctx, "Clipboard error", "cliphist is not installed", Urgency::Normal).await;
            return Ok(());
        }
        Err(err) => {
            warn!("Failed to list cliphist entries: {err}");
            notify(
                ctx,
                "Clipboard error",
                "Failed to read clipboard history",
                Urgency::Normal,
            )
            .await;
            return Ok(());
        }
    };

    confirm_clear(ctx, &guard, output.as_deref(), entries.len()).await?;
    Ok(())
}

async fn confirm_clear(
    ctx: &AppContext,
    guard: &MenuGuard,
    output: Option<&str>,
    count: usize,
) -> Result<bool> {
    if count == 0 {
        notify(ctx, "Clipboard history", "History is already empty", Urgency::Low).await;
        return Ok(false);
    }

    let yes_label = format!("{}  Yes, clear history ({count})", ctx.config.icons.trash);
    let cancel_label = format!("{}  Cancel", ctx.config.icons.cancel);
    let opts = FuzzelOptions {
        prompt: format!("{}  ", ctx.config.icons.trash),
        placeholder: Some("Confirm clear...".to_string()),
        anchor: Anchor::Top,
        width: Some(32),
        max_lines: Some(2),
        minimal_lines: true,
        no_sort: true,
        output: output.map(str::to_string),
        ..Default::default()
    };

    if fuzzel::confirm(&ctx.config.fuzzel, guard, &yes_label, &cancel_label, &opts).await? {
        match wipe_history().await {
            Ok(()) => {
                let noun = if count == 1 { "entry" } else { "entries" };
                notify(
                    ctx,
                    "Clipboard cleared",
                    &format!("Removed {count} {noun}"),
                    Urgency::Low,
                )
                .await;
                return Ok(true);
            }
            Err(CliphistError::NotInstalled) => {
                notify(ctx, "Clipboard error", "cliphist is not installed", Urgency::Normal).await;
            }
            Err(err) => {
                warn!("Failed to wipe cliphist history: {err}");
                notify(
                    ctx,
                    "Clipboard error",
                    "Failed to wipe cliphist history",
                    Urgency::Normal,
                )
                .await;
            }
        }
    }

    Ok(false)
}

async fn notify(ctx: &AppContext, summary: &str, body: &str, urgency: Urgency) {
    let mut notif = Notification::new(APP_NAME, summary)
        .urgency(urgency)
        .sync_id(SYNC_ID);
    if !body.is_empty() {
        notif = notif.body(body);
    }
    ctx.notifier.send_best_effort(&notif).await;
}

async fn run_cliphist(
    args: &[&str],
    stdin_data: Option<&[u8]>,
) -> std::result::Result<Output, CliphistError> {
    let mut cmd = Command::new("cliphist");
    cmd.args(args)
        .stdin(if stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(CliphistError::NotInstalled);
        }
        Err(err) => return Err(CliphistError::Io(err)),
    };

    let output = tokio::time::timeout(CLIPHIST_TIMEOUT, async move {
        if let Some(data) = stdin_data {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(data).await;
                let _ = stdin.shutdown().await;
            }
        }
        child.wait_with_output().await
    })
    .await
    .map_err(|_| CliphistError::TimedOut)?
    .map_err(CliphistError::Io)?;

    Ok(output)
}

async fn list_cliphist() -> std::result::Result<Vec<CliphistEntry>, CliphistError> {
    let output = run_cliphist(&["list"], None).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        warn!(
            "cliphist list exited with status {:?}: {}",
            output.status.code(),
            stderr
        );
        return Ok(Vec::new());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_cliphist_output(&stdout))
}

async fn copy_entry(raw_line: &str) -> std::result::Result<(), CliphistError> {
    let mut payload = Vec::with_capacity(raw_line.len() + 1);
    payload.extend_from_slice(raw_line.as_bytes());
    payload.push(b'\n');

    let output = run_cliphist(&["decode"], Some(&payload)).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(CliphistError::ExitFailed(output.status.code(), stderr));
    }

    wl_clipboard::copy_bytes(&output.stdout, None)
        .await
        .map_err(CliphistError::Clipboard)?;
    Ok(())
}

async fn delete_entry(raw_line: &str) -> std::result::Result<(), CliphistError> {
    let mut payload = Vec::with_capacity(raw_line.len() + 1);
    payload.extend_from_slice(raw_line.as_bytes());
    payload.push(b'\n');

    let output = run_cliphist(&["delete"], Some(&payload)).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(CliphistError::ExitFailed(output.status.code(), stderr));
    }
    Ok(())
}

async fn wipe_history() -> std::result::Result<(), CliphistError> {
    let output = run_cliphist(&["wipe"], None).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(CliphistError::ExitFailed(output.status.code(), stderr));
    }

    if let Err(err) = wl_clipboard::clear().await {
        warn!("Failed to clear Wayland clipboard via wl-copy --clear: {err}");
    }
    Ok(())
}

fn parse_cliphist_output(raw_stdout: &str) -> Vec<CliphistEntry> {
    let mut entries = Vec::new();
    for line in raw_stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let preview = match line.split_once('\t') {
            Some((_, rest)) => rest,
            None => line,
        };
        entries.push(CliphistEntry {
            raw_line: line.to_string(),
            preview: preview.to_string(),
        });
    }
    entries
}

fn sanitize_preview(preview: &str) -> String {
    preview
        .replace(['\0', '\x1f', '\r', '\n'], " ")
        .trim()
        .to_string()
}

fn format_entry(preview: &str, icons: &IconsConfig) -> String {
    let clean = sanitize_preview(preview);
    if let Some(label) = clean
        .strip_prefix("[[ binary data ")
        .and_then(|s| s.strip_suffix(" ]]"))
    {
        return format!("{}  {label}", icons.clipboard_image);
    }
    format!("{}  {clean}", icons.copy)
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cliphist_output_handles_tabs_blank_lines_and_plain_lines() {
        let raw = "\n  \t  \n101\thello world\n102\tcol1\tcol2\tcol3\n103\t[[ binary data 24 KiB png 1920x1080 ]]\nno_tab_entry\n";
        let parsed = parse_cliphist_output(raw);
        assert_eq!(
            parsed,
            vec![
                CliphistEntry {
                    raw_line: "101\thello world".to_string(),
                    preview: "hello world".to_string(),
                },
                CliphistEntry {
                    raw_line: "102\tcol1\tcol2\tcol3".to_string(),
                    preview: "col1\tcol2\tcol3".to_string(),
                },
                CliphistEntry {
                    raw_line: "103\t[[ binary data 24 KiB png 1920x1080 ]]".to_string(),
                    preview: "[[ binary data 24 KiB png 1920x1080 ]]".to_string(),
                },
                CliphistEntry {
                    raw_line: "no_tab_entry".to_string(),
                    preview: "no_tab_entry".to_string(),
                },
            ]
        );
    }

    #[test]
    fn format_entry_text_preview() {
        let icons = IconsConfig::default();
        assert_eq!(
            format_entry("  hello clipboard  ", &icons),
            format!("{}  hello clipboard", icons.copy)
        );
    }

    #[test]
    fn format_entry_binary_image_preview() {
        let icons = IconsConfig::default();
        assert_eq!(
            format_entry("[[ binary data 142 KiB png 1280x720 ]]", &icons),
            format!("{}  142 KiB png 1280x720", icons.clipboard_image)
        );
    }

    #[test]
    fn format_entry_sanitizes_control_chars_and_preserves_inner_tabs() {
        let icons = IconsConfig::default();
        assert_eq!(
            format_entry("foo\0bar\x1fbaz", &icons),
            format!("{}  foo bar baz", icons.copy)
        );
        assert_eq!(
            format_entry("\tcol1\tcol2\t", &icons),
            format!("{}  col1\tcol2", icons.copy)
        );
        assert_eq!(
            format_entry("\0[[ binary data 10 KiB jpeg 640x480 ]]\x1f", &icons),
            format!("{}  10 KiB jpeg 640x480", icons.clipboard_image)
        );
    }

    #[test]
    fn truncate_chars_is_utf8_safe() {
        let s = "héllo — 世界 🌍";
        assert_eq!(truncate_chars(s, 7), "héllo —");
        assert_eq!(truncate_chars(s, 80), s);
    }
}
