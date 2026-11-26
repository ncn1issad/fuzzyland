use crate::config::{expand_tilde, runtime_dir, ScreenshotConfig};
use crate::core::clipboard;
use crate::core::fuzzel::{self, Anchor, FuzzelOptions};
use crate::core::hyprland::{self, Client, Monitor};
use crate::core::notify::{Notification, Urgency};
use crate::daemon::menu::{MenuGuard, MenuKind, MenuSession};
use crate::daemon::AppContext;
use anyhow::{anyhow, Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::warn;

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

#[derive(Debug, Clone)]
pub struct ScreenshotArgs {
    pub mode: String,
    pub save: bool,
    pub dir: Option<PathBuf>,
    pub delay: f64,
}

pub async fn run(ctx: &AppContext, args: ScreenshotArgs) -> Result<()> {
    let save_dir = resolve_save_dir(ctx, args.dir);
    let extra_delay = sanitize_delay(args.delay);

    if args.mode == "menu" {
        return run_menu(ctx, args.save, &save_dir, extra_delay).await;
    }

    match args.mode.as_str() {
        "region" | "window" => {
            let guard = match ctx.menu.begin_session(MenuKind::Slurp) {
                MenuSession::ToggledOff => return Ok(()),
                MenuSession::Acquired(g) => g,
            };
            capture(
                ctx,
                &args.mode,
                args.save,
                &save_dir,
                extra_delay,
                Some(guard),
            )
            .await
        }
        _ => capture(ctx, &args.mode, args.save, &save_dir, extra_delay, None).await,
    }
}

fn resolve_save_dir(ctx: &AppContext, dir: Option<PathBuf>) -> PathBuf {
    match dir {
        Some(p) => {
            if let Some(s) = p.to_str() {
                if s == "~" || s.starts_with("~/") {
                    return expand_tilde(s);
                }
            }
            p
        }
        None => ctx.config.screenshot.resolved_save_dir(),
    }
}

fn sanitize_delay(delay_secs: f64) -> Duration {
    if delay_secs.is_finite() && delay_secs > 0.0 {
        Duration::from_secs_f64(delay_secs)
    } else {
        Duration::ZERO
    }
}

fn mode_label(mode: &str) -> &str {
    match mode {
        "region" => "Region",
        "window" => "Window",
        "active-window" => "Active window",
        "monitor" => "Current monitor",
        "all" => "All monitors",
        other => other,
    }
}

async fn notify(
    ctx: &AppContext,
    summary: &str,
    body: &str,
    icon: Option<&str>,
    urgency: Urgency,
) {
    let mut notif = Notification::new("Screenshot", summary)
        .body(body)
        .urgency(urgency)
        .sync_id("screenshot");
    if let Some(icon_path) = icon {
        notif = notif.icon(icon_path);
    }
    ctx.notifier.send_best_effort(&notif).await;
}

async fn run_menu(
    ctx: &AppContext,
    initial_save: bool,
    save_dir: &Path,
    extra_delay: Duration,
) -> Result<()> {
    let guard = match ctx.menu.begin_session(MenuKind::Screenshot) {
        MenuSession::ToggledOff => return Ok(()),
        MenuSession::Acquired(g) => g,
    };

    let output = hyprland::detect_cursor_output().await;
    let icons = &ctx.config.icons;
    let mut save_to_file = initial_save;

    loop {
        if !guard.is_current() {
            return Ok(());
        }

        let save_label = if save_to_file {
            format!("{}  Save to file: On  {}", icons.save, icons.check)
        } else {
            format!("{}  Save to file: Off", icons.save)
        };

        let entries: Vec<(String, &str)> = vec![
            (format!("{}  Region", icons.region), "region"),
            (format!("{}  Window", icons.window), "window"),
            (format!("{}  Current monitor", icons.monitor), "monitor"),
            (format!("{}  All monitors", icons.all_monitors), "all"),
            (save_label, "toggle-save"),
        ];

        let opts = FuzzelOptions {
            prompt: format!("{}  ", icons.camera),
            placeholder: Some("Screenshot...".to_string()),
            anchor: Anchor::Center,
            width: Some(28),
            max_lines: Some(entries.len()),
            minimal_lines: true,
            output: output.clone(),
            ..FuzzelOptions::default()
        };

        let Some(idx) = fuzzel::select(&ctx.config.fuzzel, &guard, &entries, &opts).await? else {
            return Ok(());
        };

        let choice = entries[idx].1;
        if choice == "toggle-save" {
            save_to_file = !save_to_file;
            continue;
        }

        // Wait briefly for Fuzzel's layer fade-out animation before capturing.
        let fade_delay = match choice {
            "monitor" | "all" => Duration::from_millis(250),
            _ => Duration::from_millis(50),
        };
        let total_delay = fade_delay.saturating_add(extra_delay);

        return match choice {
            "region" | "window" => {
                guard.set_kind(MenuKind::Slurp);
                capture(ctx, choice, save_to_file, save_dir, total_delay, Some(guard)).await
            }
            _ => {
                drop(guard);
                capture(ctx, choice, save_to_file, save_dir, total_delay, None).await
            }
        };
    }
}

async fn capture(
    ctx: &AppContext,
    mode: &str,
    save: bool,
    save_dir: &Path,
    delay: Duration,
    slurp_guard: Option<MenuGuard>,
) -> Result<()> {
    let mut grim_args: Vec<String> = Vec::new();

    match mode {
        "region" => {
            let guard = slurp_guard.as_ref().ok_or_else(|| {
                anyhow!("Internal error: slurp MenuGuard missing for region capture")
            })?;
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let Some(geom) = run_slurp(&ctx.config.screenshot, guard, &[], None).await? else {
                return Ok(());
            };
            drop(slurp_guard);
            grim_args.push("-g".to_string());
            grim_args.push(geom);
        }
        "window" => {
            let guard = slurp_guard.as_ref().ok_or_else(|| {
                anyhow!("Internal error: slurp MenuGuard missing for window capture")
            })?;
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let boxes = list_visible_window_geometries().await;
            if boxes.is_empty() {
                drop(slurp_guard);
                notify(
                    ctx,
                    "Screenshot",
                    "No visible windows to capture",
                    None,
                    Urgency::Normal,
                )
                .await;
                return Err(anyhow!("No visible windows to capture"));
            }
            let mut stdin_text = boxes.join("\n");
            stdin_text.push('\n');
            let Some(geom) =
                run_slurp(&ctx.config.screenshot, guard, &["-r"], Some(&stdin_text)).await?
            else {
                return Ok(());
            };
            drop(slurp_guard);
            grim_args.push("-g".to_string());
            grim_args.push(geom);
        }
        "active-window" => {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let Some(geom) = get_active_window_geometry().await else {
                notify(
                    ctx,
                    "Screenshot",
                    "No active window to capture",
                    None,
                    Urgency::Normal,
                )
                .await;
                return Err(anyhow!("No active window to capture"));
            };
            grim_args.push("-g".to_string());
            grim_args.push(geom);
        }
        "monitor" => {
            let Some(output) = hyprland::detect_cursor_output().await else {
                notify(
                    ctx,
                    "Screenshot",
                    "Could not detect current monitor",
                    None,
                    Urgency::Critical,
                )
                .await;
                return Err(anyhow!("Could not detect current monitor"));
            };
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            grim_args.push("-o".to_string());
            grim_args.push(output);
        }
        "all" => {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }
        other => {
            let msg = format!("Unknown mode: {other}");
            notify(ctx, "Screenshot", &msg, None, Urgency::Critical).await;
            return Err(anyhow!(msg));
        }
    }

    grim_args.push("-".to_string());

    let output = match Command::new("grim")
        .args(&grim_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
    {
        Ok(out) => out,
        Err(err) => {
            let msg = format!("Failed to spawn grim: {err}");
            notify(ctx, "Screenshot failed", &msg, None, Urgency::Critical).await;
            return Err(anyhow!(msg));
        }
    };

    if !output.status.success() || output.stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let err_msg = if stderr.is_empty() {
            "grim failed".to_string()
        } else {
            stderr
        };
        notify(ctx, "Screenshot failed", &err_msg, None, Urgency::Critical).await;
        return Err(anyhow!("Screenshot failed: {err_msg}"));
    }

    let png_bytes = output.stdout;
    let copied = match clipboard::copy_bytes(&png_bytes, Some("image/png")).await {
        Ok(()) => true,
        Err(err) => {
            warn!("Failed to copy screenshot to clipboard: {err}");
            false
        }
    };

    let dims_text = parse_png_dimensions(&png_bytes)
        .map(|(w, h)| format!("{w}×{h}"))
        .unwrap_or_default();
    let label = mode_label(mode);

    let mut saved_path: Option<PathBuf> = None;
    let mut icon_path: Option<String> = None;

    if save {
        match next_screenshot_path(save_dir).and_then(|path| {
            std::fs::write(&path, &png_bytes)
                .with_context(|| format!("Failed to write {}", path.display()))?;
            Ok(path)
        }) {
            Ok(path) => {
                icon_path = Some(path.to_string_lossy().into_owned());
                saved_path = Some(path);
            }
            Err(err) => {
                notify(
                    ctx,
                    "Failed to save screenshot",
                    &err.to_string(),
                    None,
                    Urgency::Critical,
                )
                .await;
                return Err(err);
            }
        }
    } else {
        let preview_dir = runtime_dir();
        if std::fs::create_dir_all(&preview_dir).is_ok() {
            let preview_path = preview_dir.join("screenshot-preview.png");
            if std::fs::write(&preview_path, &png_bytes).is_ok() {
                icon_path = Some(preview_path.to_string_lossy().into_owned());
            }
        }
    }

    let (summary, body) = match (saved_path.as_deref(), copied) {
        (Some(path), true) => {
            let detail = format_display_path(path);
            let body = if dims_text.is_empty() {
                detail
            } else {
                format!("{detail} ({dims_text})")
            };
            ("Screenshot saved & copied", body)
        }
        (Some(path), false) => {
            let detail = format_display_path(path);
            let body = if dims_text.is_empty() {
                detail
            } else {
                format!("{detail} ({dims_text})")
            };
            ("Screenshot saved", body)
        }
        (None, true) => {
            let body = if dims_text.is_empty() {
                label.to_string()
            } else {
                format!("{label} · {dims_text}")
            };
            ("Screenshot copied", body)
        }
        (None, false) => {
            notify(
                ctx,
                "Screenshot failed",
                "Could not copy image to clipboard",
                None,
                Urgency::Critical,
            )
            .await;
            return Err(anyhow!("Could not copy image to clipboard"));
        }
    };

    notify(ctx, summary, &body, icon_path.as_deref(), Urgency::Low).await;
    Ok(())
}

async fn run_slurp(
    cfg: &ScreenshotConfig,
    guard: &MenuGuard,
    extra_args: &[&str],
    stdin_text: Option<&str>,
) -> Result<Option<String>> {
    if !guard.is_current() {
        return Ok(None);
    }

    let bg = cfg.effective_slurp_bg();
    let border = cfg.effective_slurp_border();
    let selection = cfg.effective_slurp_selection();
    let box_color = cfg.effective_slurp_box();
    let font = cfg.effective_slurp_font();

    let mut cmd = Command::new("slurp");
    cmd.args([
        "-d",
        "-b",
        &bg,
        "-c",
        &border,
        "-s",
        &selection,
        "-B",
        &box_color,
        "-F",
        &font,
        "-w",
        "2",
    ]);
    cmd.args(extra_args)
        .stdin(if stdin_text.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .context("Failed to spawn slurp (is slurp installed?)")?;

    let pid = child.id();
    if let Some(pid) = pid {
        if !guard.register_child_pid(pid) {
            let _ = child.kill().await;
            return Ok(None);
        }
    }

    if let Some(mut stdin) = child.stdin.take() {
        if let Some(text) = stdin_text {
            let _ = stdin.write_all(text.as_bytes()).await;
        }
        let _ = stdin.shutdown().await;
    }

    let output = child.wait_with_output().await;
    if let Some(pid) = pid {
        guard.clear_child_pid(pid);
    }

    let output = output?;
    if !output.status.success() {
        return Ok(None);
    }

    let geom = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if geom.is_empty() {
        Ok(None)
    } else {
        Ok(Some(geom))
    }
}

async fn list_visible_window_geometries() -> Vec<String> {
    let (mons, cls) = tokio::join!(hyprland::monitors(), hyprland::clients());
    visible_window_geometries(&mons, &cls)
}

pub fn visible_window_geometries(monitors: &[Monitor], clients: &[Client]) -> Vec<String> {
    let mut active_workspaces: HashSet<i64> = HashSet::new();
    for mon in monitors {
        if mon.active_workspace.id != 0 {
            active_workspaces.insert(mon.active_workspace.id);
        }
        if mon.special_workspace.id != 0 {
            active_workspaces.insert(mon.special_workspace.id);
        }
    }

    let mut boxes = Vec::new();
    for client in clients {
        if !client.mapped || client.hidden {
            continue;
        }
        if !active_workspaces.is_empty() && !active_workspaces.contains(&client.workspace.id) {
            continue;
        }
        let [x, y] = client.at;
        let [w, h] = client.size;
        if w <= 0 || h <= 0 {
            continue;
        }
        let raw_title = if !client.title.trim().is_empty() {
            client.title.as_str()
        } else {
            client.class.as_str()
        };
        let cleaned = raw_title.replace(['\r', '\n'], " ");
        let title = cleaned.trim();
        if title.is_empty() {
            boxes.push(format!("{x},{y} {w}x{h}"));
        } else {
            boxes.push(format!("{x},{y} {w}x{h} {title}"));
        }
    }
    boxes
}

async fn get_active_window_geometry() -> Option<String> {
    let win = hyprland::active_window().await?;
    let [x, y] = win.at;
    let [w, h] = win.size;
    if w <= 0 || h <= 0 {
        return None;
    }
    Some(format!("{x},{y} {w}x{h}"))
}

pub fn parse_png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 {
        return None;
    }
    if png[..8] != PNG_SIGNATURE {
        return None;
    }
    if &png[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

fn current_local_timestamp() -> String {
    let mut now: libc::time_t = 0;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        libc::time(&mut now);
        !libc::localtime_r(&now, &mut tm).is_null()
    };
    if ok {
        format!(
            "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec,
        )
    } else {
        format!("{now}")
    }
}

fn next_screenshot_path_with_stamp(save_dir: &Path, stamp: &str) -> PathBuf {
    let mut candidate = save_dir.join(format!("screenshot-{stamp}.png"));
    let mut counter: u32 = 1;
    while candidate.exists() {
        candidate = save_dir.join(format!("screenshot-{stamp}-{counter}.png"));
        counter += 1;
    }
    candidate
}

fn next_screenshot_path(save_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(save_dir)
        .with_context(|| format!("Failed to create directory {}", save_dir.display()))?;
    let stamp = current_local_timestamp();
    Ok(next_screenshot_path_with_stamp(save_dir, &stamp))
}

fn format_display_path_with_home(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home {
        if let Ok(rel) = path.strip_prefix(home) {
            if rel.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rel.display());
        }
    }
    path.display().to_string()
}

pub fn format_display_path(path: &Path) -> String {
    let home = dirs::home_dir();
    format_display_path_with_home(path, home.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::hyprland::WorkspaceRef;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_png_header(width: u32, height: u32) -> Vec<u8> {
        let mut buf = Vec::with_capacity(24);
        buf.extend_from_slice(&PNG_SIGNATURE);
        buf.extend_from_slice(&13u32.to_be_bytes());
        buf.extend_from_slice(b"IHDR");
        buf.extend_from_slice(&width.to_be_bytes());
        buf.extend_from_slice(&height.to_be_bytes());
        buf
    }

    #[test]
    fn parse_png_dimensions_valid() {
        let header = make_png_header(2560, 1440);
        assert_eq!(parse_png_dimensions(&header), Some((2560, 1440)));

        let mut with_extra = make_png_header(1, 2);
        with_extra.extend_from_slice(&[8, 6, 0, 0, 0, 0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(parse_png_dimensions(&with_extra), Some((1, 2)));
    }

    #[test]
    fn parse_png_dimensions_rejects_invalid() {
        // Too short
        assert_eq!(parse_png_dimensions(&[]), None);
        assert_eq!(parse_png_dimensions(&make_png_header(100, 100)[..23]), None);

        // Wrong signature
        let mut bad_sig = make_png_header(100, 100);
        bad_sig[0] = 0x00;
        assert_eq!(parse_png_dimensions(&bad_sig), None);

        // Wrong chunk type
        let mut bad_chunk = make_png_header(100, 100);
        bad_chunk[12..16].copy_from_slice(b"IDAT");
        assert_eq!(parse_png_dimensions(&bad_chunk), None);

        // Zero width or height
        assert_eq!(parse_png_dimensions(&make_png_header(0, 100)), None);
        assert_eq!(parse_png_dimensions(&make_png_header(100, 0)), None);
    }

    fn make_monitor(active_ws: i64, special_ws: i64) -> Monitor {
        Monitor {
            active_workspace: WorkspaceRef {
                id: active_ws,
                name: active_ws.to_string(),
            },
            special_workspace: WorkspaceRef {
                id: special_ws,
                name: if special_ws != 0 {
                    format!("special:{special_ws}")
                } else {
                    String::new()
                },
            },
            ..Monitor::default()
        }
    }

    fn make_client(
        mapped: bool,
        hidden: bool,
        ws_id: i64,
        at: [i32; 2],
        size: [i32; 2],
        title: &str,
        class: &str,
    ) -> Client {
        Client {
            address: "0x1234".to_string(),
            mapped,
            hidden,
            at,
            size,
            workspace: WorkspaceRef {
                id: ws_id,
                name: ws_id.to_string(),
            },
            title: title.to_string(),
            class: class.to_string(),
        }
    }

    #[test]
    fn visible_window_geometries_filters_and_formats() {
        let monitors = vec![make_monitor(1, 0), make_monitor(3, -99)];
        let clients = vec![
            // Active workspace 1 with multiline title
            make_client(true, false, 1, [10, 20], [800, 600], "Line1\nLine2 ", "app1"),
            // Special workspace -99 with empty title -> falls back to class
            make_client(true, false, -99, [50, 60], [400, 300], "   ", "scratchpad"),
            // Active workspace 3 with neither title nor class
            make_client(true, false, 3, [0, 0], [1920, 1080], "", ""),
            // Inactive workspace 2 -> excluded
            make_client(true, false, 2, [0, 0], [500, 500], "Other", "other"),
            // Unmapped -> excluded
            make_client(false, false, 1, [0, 0], [500, 500], "Unmapped", "u"),
            // Hidden -> excluded
            make_client(true, true, 1, [0, 0], [500, 500], "Hidden", "h"),
            // Zero/negative dimensions -> excluded
            make_client(true, false, 1, [0, 0], [0, 500], "ZeroW", "zw"),
            make_client(true, false, 1, [0, 0], [500, -1], "NegH", "nh"),
        ];

        let boxes = visible_window_geometries(&monitors, &clients);
        assert_eq!(
            boxes,
            vec![
                "10,20 800x600 Line1 Line2",
                "50,60 400x300 scratchpad",
                "0,0 1920x1080",
            ]
        );
    }

    #[test]
    fn visible_window_geometries_falls_back_when_no_monitors() {
        let clients = vec![
            make_client(true, false, 7, [100, 200], [640, 480], "Window", "win"),
            make_client(true, true, 7, [100, 200], [640, 480], "Hidden", "win"),
        ];
        let boxes = visible_window_geometries(&[], &clients);
        assert_eq!(boxes, vec!["100,200 640x480 Window"]);
    }

    #[test]
    fn format_display_path_replaces_home_prefix() {
        let home = Path::new("/home/alice");
        assert_eq!(
            format_display_path_with_home(
                Path::new("/home/alice/Pictures/Screenshots/shot.png"),
                Some(home),
            ),
            "~/Pictures/Screenshots/shot.png"
        );
        assert_eq!(
            format_display_path_with_home(Path::new("/home/alice"), Some(home)),
            "~"
        );
        assert_eq!(
            format_display_path_with_home(Path::new("/home/alice2/shot.png"), Some(home)),
            "/home/alice2/shot.png"
        );
        assert_eq!(
            format_display_path_with_home(Path::new("/tmp/shot.png"), Some(home)),
            "/tmp/shot.png"
        );
        assert_eq!(
            format_display_path_with_home(Path::new("/tmp/shot.png"), None),
            "/tmp/shot.png"
        );
    }

    #[test]
    fn unique_filename_collision_appends_counter() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "fuzzyland-screenshot-test-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let stamp = "2026-11-26_12-34-56";
        let first = next_screenshot_path_with_stamp(&dir, stamp);
        assert_eq!(first, dir.join("screenshot-2026-11-26_12-34-56.png"));
        std::fs::write(&first, b"1").unwrap();

        let second = next_screenshot_path_with_stamp(&dir, stamp);
        assert_eq!(second, dir.join("screenshot-2026-11-26_12-34-56-1.png"));
        std::fs::write(&second, b"2").unwrap();

        let third = next_screenshot_path_with_stamp(&dir, stamp);
        assert_eq!(third, dir.join("screenshot-2026-11-26_12-34-56-2.png"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn timestamp_has_expected_format() {
        let ts = current_local_timestamp();
        assert_eq!(ts.len(), 19);
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "_");
        assert_eq!(&ts[13..14], "-");
        assert_eq!(&ts[16..17], "-");
    }
}
