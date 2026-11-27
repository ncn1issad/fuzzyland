use crate::config::IconsConfig;
use crate::core::clipboard as wl_clipboard;
use crate::core::fuzzel::{self, Anchor, FuzzelOptions};
use crate::core::hyprland;
use crate::core::notify::{Notification, Urgency};
use crate::daemon::menu::{MenuGuard, MenuKind, MenuSession};
use crate::daemon::AppContext;
use anyhow::Result;
use std::fmt;
use std::os::unix::process::CommandExt;
use std::process::{Output, Stdio};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tracing::{debug, warn};

const CLIPHIST_TIMEOUT: Duration = Duration::from_secs(5);
const WATCHER_INITIAL_DELAY: Duration = Duration::from_secs(2);
const WATCHER_MAX_DELAY: Duration = Duration::from_secs(30);
const WATCHER_STABLE_UPTIME: Duration = Duration::from_secs(30);
const WATCHER_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const WATCHER_GROUP_POLL_INTERVAL: Duration = Duration::from_millis(20);
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

#[derive(Debug, Clone)]
struct WatcherSpec {
    label: &'static str,
    program: String,
    args: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct WatcherTiming {
    initial_delay: Duration,
    max_delay: Duration,
    stable_uptime: Duration,
    stop_timeout: Duration,
}

impl Default for WatcherTiming {
    fn default() -> Self {
        Self {
            initial_delay: WATCHER_INITIAL_DELAY,
            max_delay: WATCHER_MAX_DELAY,
            stable_uptime: WATCHER_STABLE_UPTIME,
            stop_timeout: WATCHER_STOP_TIMEOUT,
        }
    }
}

pub async fn supervise_watchers(shutdown: watch::Receiver<bool>) {
    let spec = |label| WatcherSpec {
        label,
        program: "wl-paste".to_string(),
        args: ["--type", label, "--watch", "cliphist", "store"]
            .map(String::from)
            .to_vec(),
    };

    supervise_watcher_pair(spec("text"), spec("image"), WatcherTiming::default(), shutdown).await;
}

async fn supervise_watcher_pair(
    first: WatcherSpec,
    second: WatcherSpec,
    timing: WatcherTiming,
    shutdown: watch::Receiver<bool>,
) {
    tokio::join!(
        supervise_single_watcher(first, timing, shutdown.clone()),
        supervise_single_watcher(second, timing, shutdown),
    );
}

fn is_shutdown(shutdown: &mut watch::Receiver<bool>) -> bool {
    *shutdown.borrow_and_update() || shutdown.has_changed().is_err()
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stop| *stop).await;
}

async fn wait_restart_delay(delay: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        biased;
        _ = wait_for_shutdown(shutdown) => false,
        _ = tokio::time::sleep(delay) => !is_shutdown(shutdown),
    }
}

async fn supervise_single_watcher(
    spec: WatcherSpec,
    timing: WatcherTiming,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut delay = timing.initial_delay;
    let mut not_found_logged = false;

    loop {
        if is_shutdown(&mut shutdown) {
            break;
        }

        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        cmd.as_std_mut().process_group(0);

        let mut child = match cmd.spawn() {
            Ok(child) => {
                not_found_logged = false;
                child
            }
            Err(err) => {
                if err.kind() == std::io::ErrorKind::NotFound {
                    if !not_found_logged {
                        warn!(
                            "Clipboard {} watcher could not start ({} not found); retrying in {:?}",
                            spec.label, spec.program, delay
                        );
                        not_found_logged = true;
                    } else {
                        debug!(
                            "Clipboard {} watcher still missing {}; retrying in {:?}",
                            spec.label, spec.program, delay
                        );
                    }
                } else {
                    warn!(
                        "Failed to spawn clipboard {} watcher: {err}; retrying in {:?}",
                        spec.label, delay
                    );
                }

                if !wait_restart_delay(delay, &mut shutdown).await {
                    break;
                }
                delay = delay.saturating_mul(2).min(timing.max_delay);
                continue;
            }
        };

        let pid = child.id().and_then(|id| i32::try_from(id).ok());
        let started_at = tokio::time::Instant::now();

        let should_restart = tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown) => {
                stop_watcher_child(&mut child, pid, timing.stop_timeout).await;
                false
            }
            status = child.wait() => {
                if status.is_err() {
                    stop_watcher_child(&mut child, pid, timing.stop_timeout).await;
                } else if let Some(pgid) = pid {
                    let deadline = tokio::time::Instant::now() + timing.stop_timeout;
                    stop_remaining_process_group(pgid, deadline).await;
                }
                if started_at.elapsed() >= timing.stable_uptime {
                    delay = timing.initial_delay;
                }
                if is_shutdown(&mut shutdown) {
                    false
                } else {
                    match status {
                        Ok(status) => {
                            warn!(
                                "Clipboard {} watcher exited ({status}); restarting in {:?}",
                                spec.label, delay
                            );
                        }
                        Err(err) => {
                            warn!(
                                "Clipboard {} watcher wait error ({err}); restarting in {:?}",
                                spec.label, delay
                            );
                        }
                    }
                    true
                }
            }
        };

        if !should_restart {
            break;
        }
        if !wait_restart_delay(delay, &mut shutdown).await {
            break;
        }
        delay = delay.saturating_mul(2).min(timing.max_delay);
    }
}

async fn stop_watcher_child(child: &mut Child, pgid: Option<i32>, stop_timeout: Duration) {
    if let Some(pgid) = pgid {
        signal_process_group(pgid, libc::SIGTERM);
    } else {
        let _ = child.start_kill();
    }

    let deadline = tokio::time::Instant::now() + stop_timeout;
    let mut killed_group = false;

    match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => {
            debug!("Error waiting for clipboard watcher to exit: {err}");
        }
        Err(_) => {
            if let Some(pgid) = pgid {
                signal_process_group(pgid, libc::SIGKILL);
                killed_group = true;
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }

    if let Some(pgid) = pgid {
        if !killed_group {
            stop_remaining_process_group(pgid, deadline).await;
        }
    }
}

async fn stop_remaining_process_group(pgid: i32, deadline: tokio::time::Instant) {
    if !process_group_alive(pgid) {
        return;
    }
    signal_process_group(pgid, libc::SIGTERM);
    while process_group_alive(pgid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(WATCHER_GROUP_POLL_INTERVAL).await;
    }
    if process_group_alive(pgid) {
        signal_process_group(pgid, libc::SIGKILL);
    }
}

fn signal_process_group(pgid: i32, sig: i32) {
    if pgid <= 0 {
        return;
    }
    // SAFETY: Negative PID signals every process in process group `pgid`.
    let _ = unsafe { libc::kill(-pgid, sig) };
}

fn parse_proc_stat_state_and_pgrp(stat: &str) -> Option<(char, i32)> {
    let after_comm = stat.rsplit_once(')')?.1;
    let mut fields = after_comm.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?;
    let _ppid = fields.next()?;
    let pgrp = fields.next()?.parse::<i32>().ok()?;
    Some((state, pgrp))
}

fn process_group_alive(pgid: i32) -> bool {
    if pgid <= 0 {
        return false;
    }
    // SAFETY: Signal 0 probes whether any process remains in process group `pgid`.
    let rc = unsafe { libc::kill(-pgid, 0) };
    if rc != 0 {
        return std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    }

    // `kill(-pgid, 0)` still succeeds if only zombie grandchildren remain in the
    // process group (e.g. under a container init that does not reap). When /proc
    // is readable, verify at least one non-zombie member is still running.
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return true;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if let Some((state, pgrp)) = parse_proc_stat_state_and_pgrp(&stat) {
            if pgrp == pgid && !matches!(state, 'Z' | 'X' | 'x') {
                return true;
            }
        }
    }
    false
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

    struct WatcherTestDir(std::path::PathBuf);

    impl WatcherTestDir {
        fn new(name: &str) -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static SEQ: AtomicUsize = AtomicUsize::new(0);

            let base = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
                .unwrap_or_else(std::env::temp_dir);
            let dir = base.join(format!(
                "clipboard-{name}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("create watcher test dir");
            Self(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for WatcherTestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn pid_is_alive(pid: i32) -> bool {
        if pid <= 0 {
            return false;
        }
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    fn pid_is_running(pid: i32) -> bool {
        if !pid_is_alive(pid) {
            return false;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        match parse_proc_stat_state_and_pgrp(&stat) {
            Some((state, _)) => !matches!(state, 'Z' | 'X' | 'x'),
            None => true,
        }
    }

    async fn wait_for_file_lines(path: &std::path::Path, min_lines: usize) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(content) = std::fs::read_to_string(path) {
                let lines: Vec<String> = content
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(ToString::to_string)
                    .collect();
                if lines.len() >= min_lines {
                    return lines;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {} lines in {}",
                min_lines,
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn assert_grandchild_eventually_stops(pid: i32) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while pid_is_running(pid) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "grandchild process {pid} is still running after shutdown"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn fast_timing() -> WatcherTiming {
        WatcherTiming {
            initial_delay: Duration::from_millis(40),
            max_delay: Duration::from_millis(80),
            stable_uptime: Duration::from_secs(5),
            stop_timeout: Duration::from_millis(400),
        }
    }

    #[tokio::test]
    async fn supervise_watchers_restarts_failed_watcher_independently() {
        let dir = WatcherTestDir::new("restart");
        let flaky_log = dir.path().join("flaky.log");
        let steady_log = dir.path().join("steady.log");

        let flaky_script = format!(
            "echo $$ >> '{0}'; n=$(wc -l < '{0}'); if [ \"$n\" -lt 3 ]; then exit 1; fi; exec sleep 30",
            flaky_log.display()
        );
        let steady_script = format!("echo $$ >> '{}'; exec sleep 30", steady_log.display());

        let first = WatcherSpec {
            label: "text",
            program: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), flaky_script],
        };
        let second = WatcherSpec {
            label: "image",
            program: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), steady_script],
        };

        let (tx, rx) = watch::channel(false);
        let task = tokio::spawn(supervise_watcher_pair(first, second, fast_timing(), rx));

        let flaky_pids = wait_for_file_lines(&flaky_log, 3).await;
        let steady_pids = wait_for_file_lines(&steady_log, 1).await;
        assert_eq!(steady_pids.len(), 1, "steady watcher should not restart");

        let _ = tx.send(true);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("supervisor did not stop in time")
            .expect("supervisor task panicked");

        let flaky_last: i32 = flaky_pids.last().unwrap().parse().unwrap();
        let steady_pid: i32 = steady_pids[0].parse().unwrap();
        assert!(!pid_is_alive(flaky_last));
        assert!(!pid_is_alive(steady_pid));
    }

    #[tokio::test]
    async fn supervise_watchers_stops_children_and_active_grandchild_on_true_and_close() {
        for send_true in [true, false] {
            let dir = WatcherTestDir::new(if send_true { "stop-true" } else { "stop-close" });
            let first_pids_file = dir.path().join("first.pids");
            let second_pids_file = dir.path().join("second.pids");

            // Simulates wl-paste spawning an active cliphist child process.
            let mk_spec = |label: &'static str, file: &std::path::Path| WatcherSpec {
                label,
                program: "/bin/sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    format!(
                        "sleep 30 & child=$!; printf '%s\\n%s\\n' \"$$\" \"$child\" > '{}'; wait",
                        file.display()
                    ),
                ],
            };

            let (tx, rx) = watch::channel(false);
            let task = tokio::spawn(supervise_watcher_pair(
                mk_spec("text", &first_pids_file),
                mk_spec("image", &second_pids_file),
                fast_timing(),
                rx,
            ));

            let first_pids = wait_for_file_lines(&first_pids_file, 2).await;
            let second_pids = wait_for_file_lines(&second_pids_file, 2).await;

            if send_true {
                let _ = tx.send(true);
            } else {
                drop(tx);
            }

            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("supervisor did not stop in time")
                .expect("supervisor task panicked");

            for pids in [&first_pids, &second_pids] {
                let watcher_pid: i32 = pids[0].parse().unwrap();
                let grandchild_pid: i32 = pids[1].parse().unwrap();
                assert!(
                    !pid_is_alive(watcher_pid),
                    "direct watcher child {watcher_pid} was not reaped"
                );
                assert_grandchild_eventually_stops(grandchild_pid).await;
            }
        }
    }

    #[tokio::test]
    async fn supervise_watchers_returns_immediately_when_already_shut_down() {
        let dir = WatcherTestDir::new("pre-shutdown");
        let marker = dir.path().join("spawned");
        let mk_spec = |label: &'static str| WatcherSpec {
            label,
            program: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), format!("touch '{}'", marker.display())],
        };

        let (_tx_true, rx_true) = watch::channel(true);
        tokio::time::timeout(
            Duration::from_secs(2),
            supervise_watcher_pair(mk_spec("text"), mk_spec("image"), fast_timing(), rx_true),
        )
        .await
        .expect("already-true receiver should exit immediately");

        let (tx_drop, rx_drop) = watch::channel(false);
        drop(tx_drop);
        tokio::time::timeout(
            Duration::from_secs(2),
            supervise_watcher_pair(mk_spec("text"), mk_spec("image"), fast_timing(), rx_drop),
        )
        .await
        .expect("closed receiver should exit immediately");

        assert!(!marker.exists(), "watchers must not spawn after shutdown");
    }
}
