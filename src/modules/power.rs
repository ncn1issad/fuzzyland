use crate::config::IconsConfig;
use crate::core::fuzzel::{self, Anchor, FuzzelOptions};
use crate::core::hyprland;
use crate::core::notify::{Notification, Urgency};
use crate::daemon::menu::{MenuKind, MenuSession};
use crate::daemon::AppContext;
use anyhow::{Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, warn};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::Connection;

const LOGIN1_DEST: &str = "org.freedesktop.login1";
const LOGIN1_PATH: &str = "/org/freedesktop/login1";
const LOGIN1_MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";
const LOGIN1_SESSION_IFACE: &str = "org.freedesktop.login1.Session";

const SYSTEMD1_DEST: &str = "org.freedesktop.systemd1";
const SYSTEMD1_PATH: &str = "/org/freedesktop/systemd1";
const SYSTEMD1_MANAGER_IFACE: &str = "org.freedesktop.systemd1.Manager";
const SYSTEMD1_UNIT_IFACE: &str = "org.freedesktop.systemd1.Unit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PowerAction {
    Lock,
    Suspend,
    Hibernate,
    Logout,
    Reboot,
    Shutdown,
}

const ORDERED_ACTIONS: [PowerAction; 6] = [
    PowerAction::Lock,
    PowerAction::Suspend,
    PowerAction::Hibernate,
    PowerAction::Logout,
    PowerAction::Reboot,
    PowerAction::Shutdown,
];

impl PowerAction {
    fn key(self) -> &'static str {
        match self {
            Self::Lock => "lock",
            Self::Suspend => "suspend",
            Self::Hibernate => "hibernate",
            Self::Logout => "logout",
            Self::Reboot => "reboot",
            Self::Shutdown => "shutdown",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Lock => "Lock",
            Self::Suspend => "Suspend",
            Self::Hibernate => "Hibernate",
            Self::Logout => "Logout",
            Self::Reboot => "Reboot",
            Self::Shutdown => "Shutdown",
        }
    }

    fn icon(self, icons: &IconsConfig) -> &str {
        match self {
            Self::Lock => &icons.power_lock,
            Self::Suspend => &icons.power_suspend,
            Self::Hibernate => &icons.power_hibernate,
            Self::Logout => &icons.power_logout,
            Self::Reboot => &icons.power_reboot,
            Self::Shutdown => &icons.power_shutdown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PowerAvailability {
    can_suspend: bool,
    can_hibernate: bool,
}

fn is_login1_available(answer: &str) -> bool {
    matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "yes" | "challenge"
    )
}

fn filter_actions(
    actions: &[PowerAction],
    hide_unavailable: bool,
    availability: PowerAvailability,
) -> Vec<PowerAction> {
    actions
        .iter()
        .copied()
        .filter(|action| {
            if !hide_unavailable {
                return true;
            }
            match action {
                PowerAction::Suspend => availability.can_suspend,
                PowerAction::Hibernate => availability.can_hibernate,
                _ => true,
            }
        })
        .collect()
}

fn needs_confirmation(action: PowerAction, confirm_list: &[String]) -> bool {
    confirm_list
        .iter()
        .any(|entry| entry.trim().eq_ignore_ascii_case(action.key()))
}

fn non_empty_trimmed(val: Option<&str>) -> Option<&str> {
    val.map(str::trim).filter(|s| !s.is_empty())
}

fn has_uwsm_env(xdg_session_desktop: Option<&str>, uwsm_finalize: Option<&str>) -> bool {
    if non_empty_trimmed(uwsm_finalize).is_some() {
        return true;
    }
    xdg_session_desktop.is_some_and(|v| v.to_ascii_lowercase().contains("uwsm"))
}

fn is_executable_file(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn spawn_detached(program: &str, args: &[&str]) -> Result<()> {
    let mut std_cmd = std::process::Command::new(program);
    std_cmd
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // SAFETY: `setsid()` is async-signal-safe and detaches the child into its own session.
    unsafe {
        std_cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = Command::from(std_cmd)
        .spawn()
        .with_context(|| format!("Failed to spawn '{program}'"))?;

    // Reap the detached child asynchronously so the persistent daemon leaves no zombies.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    Ok(())
}

async fn query_login1_capability(bus: &Connection, method: &str) -> bool {
    match bus
        .call_method(
            Some(LOGIN1_DEST),
            LOGIN1_PATH,
            Some(LOGIN1_MANAGER_IFACE),
            method,
            &(),
        )
        .await
    {
        Ok(reply) => match reply.body().deserialize::<String>() {
            Ok(answer) => {
                debug!("login1.{method}() => {answer}");
                is_login1_available(&answer)
            }
            Err(err) => {
                warn!("Failed to deserialize login1.{method}() response: {err}");
                false
            }
        },
        Err(err) => {
            warn!("Failed to call login1.{method}(): {err}");
            false
        }
    }
}

async fn query_availability(ctx: &AppContext) -> PowerAvailability {
    if !ctx.config.power.hide_unavailable {
        return PowerAvailability {
            can_suspend: true,
            can_hibernate: true,
        };
    }

    match ctx.system_bus().await {
        Ok(bus) => {
            let (can_suspend, can_hibernate) = tokio::join!(
                query_login1_capability(bus, "CanSuspend"),
                query_login1_capability(bus, "CanHibernate"),
            );
            PowerAvailability {
                can_suspend,
                can_hibernate,
            }
        }
        Err(err) => {
            warn!("Could not connect to system D-Bus to check power availability: {err}");
            PowerAvailability {
                can_suspend: false,
                can_hibernate: false,
            }
        }
    }
}

async fn is_user_systemd_unit_active(ctx: &AppContext, unit: &str) -> bool {
    let Ok(bus) = ctx.session_bus().await else {
        debug!("Session D-Bus unavailable while checking systemd unit '{unit}'");
        return false;
    };

    let Ok(reply) = bus
        .call_method(
            Some(SYSTEMD1_DEST),
            SYSTEMD1_PATH,
            Some(SYSTEMD1_MANAGER_IFACE),
            "GetUnit",
            &(unit,),
        )
        .await
    else {
        debug!("systemd user unit '{unit}' is not loaded");
        return false;
    };

    let Ok(unit_path) = reply.body().deserialize::<OwnedObjectPath>() else {
        debug!("Failed to deserialize unit path for '{unit}'");
        return false;
    };

    let Ok(prop_reply) = bus
        .call_method(
            Some(SYSTEMD1_DEST),
            unit_path.as_str(),
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(SYSTEMD1_UNIT_IFACE, "ActiveState"),
        )
        .await
    else {
        debug!("Failed to query ActiveState for '{unit}'");
        return false;
    };

    match prop_reply.body().deserialize::<OwnedValue>() {
        Ok(val) => match String::try_from(val) {
            Ok(state) => {
                debug!("systemd user unit '{unit}' ActiveState={state}");
                state == "active"
            }
            Err(err) => {
                debug!("Unexpected ActiveState value for '{unit}': {err}");
                false
            }
        },
        Err(err) => {
            debug!("Failed to deserialize ActiveState for '{unit}': {err}");
            false
        }
    }
}

async fn login1_get_session_path(bus: &Connection) -> Result<OwnedObjectPath> {
    if let Ok(session_id) = std::env::var("XDG_SESSION_ID") {
        let trimmed = session_id.trim();
        if !trimmed.is_empty() {
            match bus
                .call_method(
                    Some(LOGIN1_DEST),
                    LOGIN1_PATH,
                    Some(LOGIN1_MANAGER_IFACE),
                    "GetSession",
                    &(trimmed,),
                )
                .await
            {
                Ok(reply) => {
                    return reply
                        .body()
                        .deserialize::<OwnedObjectPath>()
                        .context("Failed to deserialize login1 session path");
                }
                Err(err) => {
                    debug!("login1.Manager.GetSession({trimmed}) failed: {err}");
                }
            }
        }
    }

    let reply = bus
        .call_method(
            Some(LOGIN1_DEST),
            LOGIN1_PATH,
            Some(LOGIN1_MANAGER_IFACE),
            "GetSession",
            &("auto",),
        )
        .await
        .context("org.freedesktop.login1.Manager.GetSession(\"auto\") failed")?;

    reply
        .body()
        .deserialize::<OwnedObjectPath>()
        .context("Failed to deserialize login1 auto session path")
}

async fn login1_lock_session(ctx: &AppContext) -> Result<()> {
    let bus = ctx.system_bus().await?;
    if let Ok(session_path) = login1_get_session_path(bus).await {
        match bus
            .call_method(
                Some(LOGIN1_DEST),
                session_path.as_str(),
                Some(LOGIN1_SESSION_IFACE),
                "Lock",
                &(),
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(err) => {
                debug!(
                    "login1.Session.Lock() on {} failed: {err}; falling back to LockSessions()",
                    session_path.as_str()
                );
            }
        }
    }

    bus.call_method(
        Some(LOGIN1_DEST),
        LOGIN1_PATH,
        Some(LOGIN1_MANAGER_IFACE),
        "LockSessions",
        &(),
    )
    .await
    .context("org.freedesktop.login1.Manager.LockSessions() failed")?;
    Ok(())
}

async fn login1_terminate_session(ctx: &AppContext) -> Result<()> {
    let bus = ctx.system_bus().await?;
    let session_path = login1_get_session_path(bus).await?;
    bus.call_method(
        Some(LOGIN1_DEST),
        session_path.as_str(),
        Some(LOGIN1_SESSION_IFACE),
        "Terminate",
        &(),
    )
    .await
    .with_context(|| {
        format!(
            "org.freedesktop.login1.Session.Terminate() failed on {}",
            session_path.as_str()
        )
    })?;
    Ok(())
}

async fn login1_power_call(ctx: &AppContext, method: &str) -> Result<()> {
    let bus = ctx.system_bus().await?;
    bus.call_method(
        Some(LOGIN1_DEST),
        LOGIN1_PATH,
        Some(LOGIN1_MANAGER_IFACE),
        method,
        &(true,),
    )
    .await
    .with_context(|| format!("org.freedesktop.login1.Manager.{method}(true) failed"))?;
    Ok(())
}

async fn is_uwsm_active() -> bool {
    let Some(uwsm_bin) = find_in_path("uwsm") else {
        return false;
    };

    let check = tokio::time::timeout(
        Duration::from_secs(2),
        Command::new(&uwsm_bin)
            .args(["check", "is-active"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await;

    match check {
        Ok(Ok(status)) if status.success() => return true,
        Ok(Ok(_)) => {}
        Ok(Err(err)) => debug!("Failed to run `uwsm check is-active`: {err}"),
        Err(_) => debug!("Timed out running `uwsm check is-active`"),
    }

    let xdg_desktop = std::env::var("XDG_SESSION_DESKTOP").ok();
    let uwsm_finalize = std::env::var("UWSM_FINALIZE_VARNAMES").ok();
    has_uwsm_env(xdg_desktop.as_deref(), uwsm_finalize.as_deref())
}

async fn execute_lock(ctx: &AppContext) -> Result<()> {
    if let Some(cmd) = non_empty_trimmed(ctx.config.power.lock_cmd.as_deref()) {
        return spawn_detached("/bin/sh", &["-c", cmd]);
    }

    if is_user_systemd_unit_active(ctx, "hypridle.service").await {
        login1_lock_session(ctx).await
    } else {
        spawn_detached("hyprlock", &[])
    }
}

async fn execute_logout(ctx: &AppContext) -> Result<()> {
    if let Some(cmd) = non_empty_trimmed(ctx.config.power.logout_cmd.as_deref()) {
        return spawn_detached("/bin/sh", &["-c", cmd]);
    }

    if is_uwsm_active().await {
        return spawn_detached("uwsm", &["stop"]);
    }

    match hyprland::dispatch("exit").await {
        Ok(_) => Ok(()),
        Err(hypr_err) => {
            debug!(
                "Hyprland `dispatch exit` failed ({hypr_err}); falling back to login1 session terminate"
            );
            login1_terminate_session(ctx).await.with_context(|| {
                format!(
                    "Hyprland exit failed ({hypr_err}) and login1 session termination also failed"
                )
            })
        }
    }
}

async fn execute_action(ctx: &AppContext, action: PowerAction) -> Result<()> {
    match action {
        PowerAction::Lock => execute_lock(ctx).await,
        PowerAction::Suspend => login1_power_call(ctx, "Suspend").await,
        PowerAction::Hibernate => login1_power_call(ctx, "Hibernate").await,
        PowerAction::Logout => execute_logout(ctx).await,
        PowerAction::Reboot => login1_power_call(ctx, "Reboot").await,
        PowerAction::Shutdown => login1_power_call(ctx, "PowerOff").await,
    }
}

pub async fn run_menu(ctx: &AppContext) -> Result<()> {
    let guard = match ctx.menu.begin_session(MenuKind::Power) {
        MenuSession::ToggledOff => return Ok(()),
        MenuSession::Acquired(guard) => guard,
    };

    let (output, availability) =
        tokio::join!(hyprland::detect_cursor_output(), query_availability(ctx));

    let actions = filter_actions(
        &ORDERED_ACTIONS,
        ctx.config.power.hide_unavailable,
        availability,
    );
    if actions.is_empty() {
        return Ok(());
    }

    let icons = &ctx.config.icons;
    let entries: Vec<(String, PowerAction)> = actions
        .into_iter()
        .map(|action| {
            let text = format!("{}  {}", action.icon(icons), action.label());
            (text, action)
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
    let action = entries[idx].1;

    if needs_confirmation(action, &ctx.config.power.confirm) {
        let icon = action.icon(icons);
        let yes_label = format!("{icon}  Yes, {}", action.label());
        let cancel_label = format!("{}  Cancel", icons.cancel);
        let confirm_opts = FuzzelOptions {
            prompt: format!("{icon}  "),
            placeholder: Some(format!("Confirm {}...", action.label().to_ascii_lowercase())),
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

    if let Err(err) = execute_action(ctx, action).await {
        warn!("Power action '{}' failed: {err:#}", action.label());
        let notif = Notification::new("Power", format!("{} failed", action.label()))
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
    use crate::config::PowerConfig;

    #[test]
    fn action_order_is_canonical() {
        let keys: Vec<&str> = ORDERED_ACTIONS.iter().map(|a| a.key()).collect();
        assert_eq!(
            keys,
            vec!["lock", "suspend", "hibernate", "logout", "reboot", "shutdown"]
        );

        let labels: Vec<&str> = ORDERED_ACTIONS.iter().map(|a| a.label()).collect();
        assert_eq!(
            labels,
            vec!["Lock", "Suspend", "Hibernate", "Logout", "Reboot", "Shutdown"]
        );
    }

    #[test]
    fn confirmation_matching_is_case_insensitive_and_trimmed() {
        let defaults = PowerConfig::default().confirm;
        assert!(!needs_confirmation(PowerAction::Lock, &defaults));
        assert!(!needs_confirmation(PowerAction::Suspend, &defaults));
        assert!(!needs_confirmation(PowerAction::Hibernate, &defaults));
        assert!(needs_confirmation(PowerAction::Logout, &defaults));
        assert!(needs_confirmation(PowerAction::Reboot, &defaults));
        assert!(needs_confirmation(PowerAction::Shutdown, &defaults));

        let custom = vec![
            "LOCK".to_string(),
            "  Hibernate ".to_string(),
            "ShUtDoWn".to_string(),
        ];
        assert!(needs_confirmation(PowerAction::Lock, &custom));
        assert!(needs_confirmation(PowerAction::Hibernate, &custom));
        assert!(needs_confirmation(PowerAction::Shutdown, &custom));
        assert!(!needs_confirmation(PowerAction::Suspend, &custom));
        assert!(!needs_confirmation(PowerAction::Logout, &custom));
        assert!(!needs_confirmation(PowerAction::Reboot, &custom));
    }

    #[test]
    fn login1_availability_parsing() {
        assert!(is_login1_available("yes"));
        assert!(is_login1_available("challenge"));
        assert!(is_login1_available(" YES "));
        assert!(is_login1_available("Challenge"));
        assert!(!is_login1_available("no"));
        assert!(!is_login1_available("na"));
        assert!(!is_login1_available(""));
    }

    #[test]
    fn availability_filtering_respects_hide_unavailable_flag() {
        let none_available = PowerAvailability {
            can_suspend: false,
            can_hibernate: false,
        };

        // When hide_unavailable is false, all actions remain in order.
        let kept = filter_actions(&ORDERED_ACTIONS, false, none_available);
        assert_eq!(kept, ORDERED_ACTIONS.to_vec());

        // When hide_unavailable is true and neither suspend nor hibernate is available.
        let filtered = filter_actions(&ORDERED_ACTIONS, true, none_available);
        assert_eq!(
            filtered,
            vec![
                PowerAction::Lock,
                PowerAction::Logout,
                PowerAction::Reboot,
                PowerAction::Shutdown,
            ]
        );

        // Only suspend available -> hibernate omitted, order preserved.
        let suspend_only = filter_actions(
            &ORDERED_ACTIONS,
            true,
            PowerAvailability {
                can_suspend: true,
                can_hibernate: false,
            },
        );
        assert_eq!(
            suspend_only,
            vec![
                PowerAction::Lock,
                PowerAction::Suspend,
                PowerAction::Logout,
                PowerAction::Reboot,
                PowerAction::Shutdown,
            ]
        );

        // Only hibernate available -> suspend omitted, order preserved.
        let hibernate_only = filter_actions(
            &ORDERED_ACTIONS,
            true,
            PowerAvailability {
                can_suspend: false,
                can_hibernate: true,
            },
        );
        assert_eq!(
            hibernate_only,
            vec![
                PowerAction::Lock,
                PowerAction::Hibernate,
                PowerAction::Logout,
                PowerAction::Reboot,
                PowerAction::Shutdown,
            ]
        );
    }

    #[test]
    fn uwsm_env_detection() {
        assert!(has_uwsm_env(None, Some("WAYLAND_DISPLAY DISPLAY")));
        assert!(has_uwsm_env(Some("Hyprland-uwsm"), None));
        assert!(has_uwsm_env(Some("UWSM"), None));
        assert!(!has_uwsm_env(Some("Hyprland"), None));
        assert!(!has_uwsm_env(Some("Hyprland"), Some("   ")));
        assert!(!has_uwsm_env(None, None));
    }
}
