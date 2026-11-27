use crate::config::{expand_tilde, IconsConfig, PowerConfig};
use crate::core::fuzzel::{self, Anchor, FuzzelOptions};
use crate::core::hyprland;
use crate::core::notify::{Notification, Urgency};
use crate::core::process::spawn_reaped;
use crate::daemon::menu::{MenuKind, MenuSession};
use crate::daemon::AppContext;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;
use tokio::process::Command;
use tracing::{debug, warn};

const CANONICAL_ORDER: [&str; 6] = [
    "lock",
    "suspend",
    "hibernate",
    "logout",
    "reboot",
    "shutdown",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerButton {
    pub label: String,
    pub text: String,
    pub action: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LayoutStamp {
    path: Option<PathBuf>,
    mtime: Option<SystemTime>,
    len: u64,
}

#[derive(Debug, Default)]
pub struct PowerState {
    cache: RwLock<Option<(LayoutStamp, Vec<PowerButton>)>>,
}

impl PowerState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-loads wlogout buttons into live RAM at daemon startup.
    pub fn warm_load(&self, cfg: &PowerConfig) {
        let buttons = self.get_buttons(cfg);
        debug!("Loaded {} wlogout power button(s) into RAM", buttons.len());
    }

    /// Returns cached wlogout buttons from live RAM, automatically reloading
    /// if the active layout file was created, removed, or modified on disk.
    pub fn get_buttons(&self, cfg: &PowerConfig) -> Vec<PowerButton> {
        let candidates = layout_candidates(cfg);
        self.get_buttons_from(&candidates)
    }

    fn get_buttons_from(&self, candidates: &[PathBuf]) -> Vec<PowerButton> {
        let active_path = candidates.iter().find(|p| p.is_file()).cloned();
        let stamp = match &active_path {
            Some(path) => match std::fs::metadata(path) {
                Ok(meta) => LayoutStamp {
                    path: Some(path.clone()),
                    mtime: meta.modified().ok(),
                    len: meta.len(),
                },
                Err(_) => LayoutStamp {
                    path: Some(path.clone()),
                    mtime: None,
                    len: 0,
                },
            },
            None => LayoutStamp {
                path: None,
                mtime: None,
                len: 0,
            },
        };

        if let Ok(guard) = self.cache.read() {
            if let Some((cached_stamp, cached_buttons)) = guard.as_ref() {
                if *cached_stamp == stamp {
                    return cached_buttons.clone();
                }
            }
        }

        let buttons = load_buttons_from_path(active_path.as_deref());
        if let Ok(mut guard) = self.cache.write() {
            *guard = Some((stamp, buttons.clone()));
        }
        buttons
    }
}

#[derive(Debug, Deserialize)]
struct RawWlogoutEntry {
    #[serde(default)]
    label: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    action: String,
}

fn layout_candidates(cfg: &PowerConfig) -> Vec<PathBuf> {
    if let Some(custom) = cfg.layout_path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return vec![expand_tilde(custom)];
    }

    let mut paths = Vec::with_capacity(2);
    if let Some(config_dir) = dirs::config_dir() {
        paths.push(config_dir.join("wlogout").join("layout"));
    } else if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".config").join("wlogout").join("layout"));
    }
    paths.push(PathBuf::from("/etc/wlogout/layout"));
    paths
}

fn default_buttons() -> Vec<PowerButton> {
    vec![
        PowerButton {
            label: "lock".to_string(),
            text: "Lock".to_string(),
            action: "hyprlock".to_string(),
        },
        PowerButton {
            label: "suspend".to_string(),
            text: "Suspend".to_string(),
            action: "systemctl suspend".to_string(),
        },
        PowerButton {
            label: "hibernate".to_string(),
            text: "Hibernate".to_string(),
            action: "systemctl hibernate".to_string(),
        },
        PowerButton {
            label: "logout".to_string(),
            text: "Logout".to_string(),
            action: "uwsm stop".to_string(),
        },
        PowerButton {
            label: "reboot".to_string(),
            text: "Reboot".to_string(),
            action: "systemctl reboot".to_string(),
        },
        PowerButton {
            label: "shutdown".to_string(),
            text: "Shutdown".to_string(),
            action: "systemctl poweroff".to_string(),
        },
    ]
}

fn parse_wlogout_layout(raw: &str) -> Vec<PowerButton> {
    let mut items = Vec::new();
    let stream = serde_json::Deserializer::from_str(raw).into_iter::<RawWlogoutEntry>();
    for entry in stream {
        match entry {
            Ok(obj) => {
                let action = obj.action.trim();
                if action.is_empty() {
                    continue;
                }
                let raw_label = obj.label.trim();
                let label = raw_label.to_ascii_lowercase();
                let text = obj
                    .text
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(raw_label)
                    .to_string();
                if text.is_empty() {
                    continue;
                }
                items.push(PowerButton {
                    label,
                    text,
                    action: action.to_string(),
                });
            }
            Err(err) => {
                warn!("Stopped parsing wlogout layout on invalid JSON object: {err}");
                break;
            }
        }
    }

    items.sort_by_key(|item| {
        CANONICAL_ORDER
            .iter()
            .position(|&name| name == item.label)
            .unwrap_or(CANONICAL_ORDER.len())
    });

    items
}

fn load_buttons_from_path(path: Option<&Path>) -> Vec<PowerButton> {
    let Some(path) = path else {
        debug!("No wlogout layout file found; using built-in default power actions");
        return default_buttons();
    };

    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let parsed = parse_wlogout_layout(&raw);
            if parsed.is_empty() {
                warn!(
                    "wlogout layout {} contained no valid actions; using defaults",
                    path.display()
                );
                default_buttons()
            } else {
                debug!("Parsed {} button(s) from {}", parsed.len(), path.display());
                parsed
            }
        }
        Err(err) => {
            warn!(
                "Failed to read wlogout layout {}: {err}; using defaults",
                path.display()
            );
            default_buttons()
        }
    }
}

fn icon_for_label<'a>(label: &str, icons: &'a IconsConfig) -> &'a str {
    match label {
        "lock" => &icons.power_lock,
        "suspend" => &icons.power_suspend,
        "hibernate" => &icons.power_hibernate,
        "logout" => &icons.power_logout,
        "reboot" => &icons.power_reboot,
        "shutdown" => &icons.power_shutdown,
        _ => &icons.power_shutdown,
    }
}

fn needs_confirmation(label: &str, confirm_list: &[String]) -> bool {
    confirm_list
        .iter()
        .any(|entry| entry.trim().eq_ignore_ascii_case(label))
}

fn spawn_detached_shell(command: &str) -> Result<()> {
    let mut std_cmd = std::process::Command::new("/bin/sh");
    std_cmd.args(["-c", command]);

    // SAFETY: `setsid()` is async-signal-safe and detaches the child into its own session.
    unsafe {
        std_cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    spawn_reaped(&mut Command::from(std_cmd))
        .with_context(|| format!("Failed to spawn power command '{command}'"))
}

pub async fn run_menu(ctx: &AppContext) -> Result<()> {
    let guard = match ctx.menu.begin_session(MenuKind::Power) {
        MenuSession::ToggledOff => return Ok(()),
        MenuSession::Acquired(guard) => guard,
    };

    let output = hyprland::detect_cursor_output().await;
    let buttons = ctx.power.get_buttons(&ctx.config.power);
    if buttons.is_empty() {
        return Ok(());
    }

    let icons = &ctx.config.icons;
    let entries: Vec<(String, PowerButton)> = buttons
        .into_iter()
        .map(|btn| {
            let icon = icon_for_label(&btn.label, icons);
            (format!("{icon}  {}", btn.text), btn)
        })
        .collect();

    let opts = FuzzelOptions {
        prompt: format!("{}  ", icons.power_shutdown),
        placeholder: Some("Power action...".to_string()),
        anchor: Anchor::TopRight,
        width: Some(24),
        max_lines: Some(entries.len()),
        output: output.clone(),
        ..FuzzelOptions::default()
    };

    let Some(idx) = fuzzel::select(&ctx.config.fuzzel, &guard, &entries, &opts).await? else {
        return Ok(());
    };
    let selected = entries[idx].1.clone();

    if needs_confirmation(&selected.label, &ctx.config.power.confirm) {
        let icon = icon_for_label(&selected.label, icons);
        let yes_label = format!("{icon}  Yes, {}", selected.text);
        let cancel_label = format!("{}  Cancel", icons.cancel);
        let confirm_opts = FuzzelOptions {
            prompt: format!("{icon}  "),
            placeholder: Some(format!("Confirm {}...", selected.text.to_ascii_lowercase())),
            anchor: Anchor::TopRight,
            width: Some(24),
            output,
            ..FuzzelOptions::default()
        };

        let confirmed = fuzzel::confirm(
            &ctx.config.fuzzel,
            &guard,
            &yes_label,
            &cancel_label,
            &confirm_opts,
        )
        .await?;
        if !confirmed {
            return Ok(());
        }
    }

    drop(guard);

    if let Err(err) = spawn_detached_shell(&selected.action) {
        warn!("Power action '{}' failed: {err:#}", selected.text);
        let notif = Notification::new("Power", format!("{} failed", selected.text))
            .body(format!("{err}"))
            .urgency(Urgency::Critical);
        ctx.notifier.send_best_effort(&notif).await;
        return Err(err);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_concatenated_wlogout_layout_and_orders_canonically() {
        let layout = r#"
{
    "label" : "shutdown",
    "action" : "systemctl poweroff",
    "text" : "Shutdown",
    "keybind" : "s"
}
{
    "label" : "custom",
    "action" : "my-custom-cmd",
    "text" : "Custom Action",
    "keybind" : "c"
}
{
    "label" : "lock",
    "action" : "/usr/bin/hyprlock",
    "text" : "Lock",
    "keybind" : "l"
}
{
    "label" : "reboot",
    "action" : "systemctl reboot",
    "text" : "Reboot",
    "keybind" : "r"
}
"#;
        let buttons = parse_wlogout_layout(layout);
        let labels: Vec<&str> = buttons.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, vec!["lock", "reboot", "shutdown", "custom"]);
        assert_eq!(buttons[0].action, "/usr/bin/hyprlock");
        assert_eq!(buttons[3].text, "Custom Action");
        assert_eq!(buttons[3].action, "my-custom-cmd");
    }

    #[test]
    fn skips_entries_with_missing_action_and_falls_back_text_to_label() {
        let layout = r#"
{ "label": "empty", "action": "   ", "text": "Ignored" }
{ "label": "Suspend", "action": "systemctl suspend" }
"#;
        let buttons = parse_wlogout_layout(layout);
        assert_eq!(buttons.len(), 1);
        assert_eq!(buttons[0].label, "suspend");
        assert_eq!(buttons[0].text, "Suspend");
        assert_eq!(buttons[0].action, "systemctl suspend");
    }

    #[test]
    fn confirmation_matching_is_case_insensitive_and_trimmed() {
        let defaults = PowerConfig::default().confirm;
        assert!(!needs_confirmation("lock", &defaults));
        assert!(!needs_confirmation("suspend", &defaults));
        assert!(!needs_confirmation("hibernate", &defaults));
        assert!(needs_confirmation("logout", &defaults));
        assert!(needs_confirmation("reboot", &defaults));
        assert!(needs_confirmation("shutdown", &defaults));

        let custom = vec!["LOCK".to_string(), "  Custom ".to_string()];
        assert!(needs_confirmation("lock", &custom));
        assert!(needs_confirmation("custom", &custom));
        assert!(!needs_confirmation("shutdown", &custom));
    }

    #[test]
    fn power_state_caches_in_ram_and_reloads_on_change() {
        let dir = std::env::temp_dir().join(format!(
            "fuzzyland-power-test-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("layout");

        std::fs::write(
            &file,
            r#"{ "label": "lock", "action": "hyprlock", "text": "Lock" }"#,
        )
        .unwrap();

        let state = PowerState::new();
        let candidates = vec![file.clone()];

        let first = state.get_buttons_from(&candidates);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].action, "hyprlock");

        // Modify file with different length so stamp changes even within same mtime tick.
        std::fs::write(
            &file,
            r#"{ "label": "lock", "action": "swaylock -f", "text": "Lock Screen" }
{ "label": "shutdown", "action": "systemctl poweroff", "text": "Power Off" }"#,
        )
        .unwrap();

        let second = state.get_buttons_from(&candidates);
        assert_eq!(second.len(), 2);
        assert_eq!(second[0].action, "swaylock -f");
        assert_eq!(second[1].text, "Power Off");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_layout_file_falls_back_to_defaults() {
        let state = PowerState::new();
        let buttons = state.get_buttons_from(&[PathBuf::from("/nonexistent/wlogout/layout")]);
        let labels: Vec<&str> = buttons.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["lock", "suspend", "hibernate", "logout", "reboot", "shutdown"]
        );
    }
}
