use crate::config::{BatteryConfig, Config, IconsConfig};
use crate::core::fuzzel::{self, Anchor, FuzzelOptions};
use crate::core::hyprland;
use crate::core::notify::{Notification, Urgency};
use crate::core::waybar::{TooltipFormatter, WaybarOutput};
use crate::daemon::menu::{MenuKind, MenuSession};
use crate::daemon::AppContext;
use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, warn};
use zbus::fdo::DBusProxy;
use zbus::message::Type as MessageType;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, Message, MessageStream};

const UPOWER_DEST: &str = "org.freedesktop.UPower";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";
const UPOWER_IFACE: &str = "org.freedesktop.UPower";
const UPOWER_DEVICE_IFACE: &str = "org.freedesktop.UPower.Device";
const UPOWER_DISPLAY_DEVICE_PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const DBUS_PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";

// UPower Device Type enum (from org.freedesktop.UPower.Device)
const UPOWER_TYPE_LINE_POWER: u32 = 1;
const UPOWER_TYPE_BATTERY: u32 = 2;
const UPOWER_TYPE_UPS: u32 = 3;
const UPOWER_TYPE_MONITOR: u32 = 4;
const UPOWER_TYPE_MOUSE: u32 = 5;
const UPOWER_TYPE_KEYBOARD: u32 = 6;
const UPOWER_TYPE_PDA: u32 = 7;
const UPOWER_TYPE_PHONE: u32 = 8;
const UPOWER_TYPE_MEDIA_PLAYER: u32 = 9;
const UPOWER_TYPE_TABLET: u32 = 10;
const UPOWER_TYPE_COMPUTER: u32 = 11;
const UPOWER_TYPE_GAMING_INPUT: u32 = 12;
const UPOWER_TYPE_PEN: u32 = 13;
const UPOWER_TYPE_TOUCHPAD: u32 = 14;
const UPOWER_TYPE_MODEM: u32 = 15;
const UPOWER_TYPE_NETWORK: u32 = 16;
const UPOWER_TYPE_HEADSET: u32 = 17;
const UPOWER_TYPE_SPEAKERS: u32 = 18;
const UPOWER_TYPE_HEADPHONES: u32 = 19;
const UPOWER_TYPE_VIDEO: u32 = 20;
const UPOWER_TYPE_OTHER_AUDIO: u32 = 21;
const UPOWER_TYPE_CAMERA: u32 = 25;
const UPOWER_TYPE_BLUETOOTH_GENERIC_26: u32 = 26;
const UPOWER_TYPE_BLUETOOTH_GENERIC_28: u32 = 28;

// UPower Device State enum
const UPOWER_STATE_UNKNOWN: u32 = 0;
const UPOWER_STATE_CHARGING: u32 = 1;
const UPOWER_STATE_FULLY_CHARGED: u32 = 4;
const UPOWER_STATE_PENDING_CHARGE: u32 = 5;

const PROFILE_ORDER: [&str; 3] = ["power-saver", "balanced", "performance"];
const SIGNAL_DEBOUNCE_MS: u64 = 100;
const FALLBACK_POLL_SECS: u64 = 60;

#[derive(Debug, Clone, Copy)]
struct PowerProfilesEndpoint {
    dest: &'static str,
    path: &'static str,
    iface: &'static str,
}

const POWER_PROFILES_ENDPOINTS: [PowerProfilesEndpoint; 2] = [
    PowerProfilesEndpoint {
        dest: "org.freedesktop.UPower.PowerProfiles",
        path: "/org/freedesktop/UPower/PowerProfiles",
        iface: "org.freedesktop.UPower.PowerProfiles",
    },
    PowerProfilesEndpoint {
        dest: "net.hadess.PowerProfiles",
        path: "/net/hadess/PowerProfiles",
        iface: "net.hadess.PowerProfiles",
    },
];

pub struct BatteryState {
    tx: watch::Sender<WaybarOutput>,
    populated: AtomicBool,
}

impl BatteryState {
    pub fn new() -> Self {
        let (tx, _) = watch::channel(WaybarOutput::empty());
        Self {
            tx,
            populated: AtomicBool::new(false),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<WaybarOutput> {
        self.tx.subscribe()
    }

    fn cached(&self) -> Option<WaybarOutput> {
        if self.populated.load(Ordering::Acquire) {
            Some(self.tx.borrow().clone())
        } else {
            None
        }
    }

    fn update(&self, output: WaybarOutput) {
        self.populated.store(true, Ordering::Release);
        self.tx.send_if_modified(|current| {
            if *current == output {
                false
            } else {
                *current = output;
                true
            }
        });
    }
}

impl Default for BatteryState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default)]
struct UPowerDevice {
    path: String,
    device_type: u32,
    power_supply: bool,
    is_present: bool,
    state: u32,
    percentage: Option<f64>,
    energy_rate: Option<f64>,
    capacity: Option<f64>,
    time_to_empty: i64,
    time_to_full: i64,
    model: String,
    vendor: String,
    native_path: String,
    icon_name: String,
}

impl UPowerDevice {
    fn valid_percentage(&self) -> Option<u8> {
        let pct = self.percentage?;
        if !pct.is_finite() || !(0.0..=100.0).contains(&pct) {
            return None;
        }
        // Reject devices reporting 0% with Unknown state (no real battery gauge).
        if pct == 0.0 && self.state == UPOWER_STATE_UNKNOWN {
            return None;
        }
        Some(pct.round().clamp(0.0, 100.0) as u8)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PeripheralBattery {
    icon: String,
    name: String,
    percentage: u8,
}

pub fn spawn_background(ctx: Arc<AppContext>) {
    tokio::spawn(async move {
        refresh_and_publish(&ctx).await;

        let Ok(conn) = ctx.system_bus().await else {
            warn!("Battery watcher could not connect to system D-Bus; polling periodically");
            fallback_poll_loop(ctx).await;
            return;
        };

        if let Err(err) = subscribe_dbus_signals(conn).await {
            warn!("Battery watcher could not subscribe to D-Bus signals: {err}; falling back to polling");
            fallback_poll_loop(ctx).await;
            return;
        }

        let mut stream = MessageStream::from(conn);
        let mut pending_refresh = false;
        let debounce = tokio::time::sleep(Duration::from_secs(3600));
        tokio::pin!(debounce);

        let mut fallback_tick = tokio::time::interval(Duration::from_secs(FALLBACK_POLL_SECS));
        fallback_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Consume the immediate first tick since we already performed the initial refresh.
        fallback_tick.tick().await;

        loop {
            tokio::select! {
                maybe_msg = stream.next() => {
                    match maybe_msg {
                        Some(Ok(msg)) => {
                            if is_relevant_signal(&msg) && !pending_refresh {
                                pending_refresh = true;
                                debounce
                                    .as_mut()
                                    .reset(tokio::time::Instant::now() + Duration::from_millis(SIGNAL_DEBOUNCE_MS));
                            }
                        }
                        Some(Err(err)) => {
                            debug!("Battery D-Bus message stream error: {err}");
                        }
                        None => {
                            warn!("Battery D-Bus message stream ended; falling back to periodic polling");
                            break;
                        }
                    }
                }
                _ = &mut debounce, if pending_refresh => {
                    pending_refresh = false;
                    refresh_and_publish(&ctx).await;
                }
                _ = fallback_tick.tick() => {
                    pending_refresh = false;
                    refresh_and_publish(&ctx).await;
                }
            }
        }

        fallback_poll_loop(ctx).await;
    });
}

async fn fallback_poll_loop(ctx: Arc<AppContext>) {
    let mut tick = tokio::time::interval(Duration::from_secs(FALLBACK_POLL_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    loop {
        tick.tick().await;
        refresh_and_publish(&ctx).await;
    }
}

async fn refresh_and_publish(ctx: &AppContext) {
    match query_battery_output(ctx).await {
        Ok(out) => ctx.battery.update(out),
        Err(err) => {
            debug!("Battery status refresh failed: {err:#}");
            if ctx.battery.cached().is_none() {
                ctx.battery.update(WaybarOutput::empty());
            }
        }
    }
}

async fn subscribe_dbus_signals(conn: &Connection) -> Result<()> {
    let dbus = DBusProxy::new(conn)
        .await
        .context("Failed to create org.freedesktop.DBus proxy")?;

    let rules = [
        MatchRule::builder()
            .msg_type(MessageType::Signal)
            .interface(UPOWER_IFACE)?
            .member("DeviceAdded")?
            .build(),
        MatchRule::builder()
            .msg_type(MessageType::Signal)
            .interface(UPOWER_IFACE)?
            .member("DeviceRemoved")?
            .build(),
        MatchRule::builder()
            .msg_type(MessageType::Signal)
            .interface(DBUS_PROPERTIES_IFACE)?
            .member("PropertiesChanged")?
            .path_namespace(UPOWER_PATH)?
            .build(),
        MatchRule::builder()
            .msg_type(MessageType::Signal)
            .interface(DBUS_PROPERTIES_IFACE)?
            .member("PropertiesChanged")?
            .path(POWER_PROFILES_ENDPOINTS[1].path)?
            .build(),
    ];

    for rule in rules {
        dbus.add_match_rule(rule)
            .await
            .context("Failed to register D-Bus match rule for battery watcher")?;
    }

    Ok(())
}

fn is_relevant_signal(msg: &Message) -> bool {
    if msg.message_type() != MessageType::Signal {
        return false;
    }
    let header = msg.header();
    let iface = header.interface().map(|i| i.as_str());
    let member = header.member().map(|m| m.as_str());
    let path = header.path().map(|p| p.as_str());

    match (iface, member) {
        (Some(UPOWER_IFACE), Some("DeviceAdded" | "DeviceRemoved")) => true,
        (Some(DBUS_PROPERTIES_IFACE), Some("PropertiesChanged")) => matches!(
            path,
            Some(p) if p.starts_with(UPOWER_PATH) || p == POWER_PROFILES_ENDPOINTS[1].path
        ),
        _ => false,
    }
}

pub async fn status_once(ctx: &AppContext) -> Result<WaybarOutput> {
    if ctx.is_daemon {
        if let Some(cached) = ctx.battery.cached() {
            return Ok(cached);
        }
    }

    let output = query_battery_output(ctx).await?;
    if ctx.is_daemon {
        ctx.battery.update(output.clone());
    }
    Ok(output)
}

pub async fn run_menu(ctx: &AppContext) -> Result<()> {
    let guard = match ctx.menu.begin_session(MenuKind::Battery) {
        MenuSession::ToggledOff => return Ok(()),
        MenuSession::Acquired(g) => g,
    };

    let (output, profiles_res) = tokio::join!(
        hyprland::detect_cursor_output(),
        fetch_power_profiles(ctx)
    );

    let (active, profiles) = match profiles_res {
        Ok(res) => res,
        Err(err) => {
            debug!("Failed to query power profiles: {err:#}");
            (None, Vec::new())
        }
    };

    let icons = &ctx.config.icons;
    let mut entries: Vec<(String, Option<String>)> = Vec::new();
    for prof in profiles {
        let (icon, label) = profile_meta(&prof, icons);
        let mark = if active.as_deref() == Some(prof.as_str()) {
            format!("  {}", icons.check)
        } else {
            String::new()
        };
        entries.push((format!("{icon}  {label}{mark}"), Some(prof)));
    }

    if entries.is_empty() {
        entries.push((
            format!("{}  No power profiles available", icons.battery_unknown),
            None,
        ));
    }

    let prompt_icon = icons
        .battery_levels
        .last()
        .map(String::as_str)
        .unwrap_or(&icons.battery_unknown);
    let opts = FuzzelOptions {
        prompt: format!("{prompt_icon}  "),
        placeholder: Some("Power profile...".to_string()),
        anchor: Anchor::TopRight,
        width: Some(24),
        max_lines: Some(8),
        minimal_lines: true,
        output,
        ..Default::default()
    };

    let Some(idx) = fuzzel::select(&ctx.config.fuzzel, &guard, &entries, &opts).await? else {
        return Ok(());
    };
    drop(guard);

    if let Some(chosen) = &entries[idx].1 {
        set_profile(ctx, chosen).await?;
    }
    Ok(())
}

pub async fn set_profile(ctx: &AppContext, name: &str) -> Result<()> {
    let profile = name.trim();
    if profile.is_empty() {
        return Err(anyhow!("Power profile name cannot be empty"));
    }

    let conn = ctx.system_bus().await?;
    if let Err(err) = set_active_profile_dbus(conn, profile).await {
        let notif = Notification::new("Power", "Failed to set power profile")
            .body(err.to_string())
            .urgency(Urgency::Critical)
            .sync_id("waybar-power-profile");
        ctx.notifier.send_best_effort(&notif).await;
        return Err(err);
    }

    if ctx.is_daemon {
        refresh_and_publish(ctx).await;
    }

    let label = profile_label(profile);
    let notif = Notification::new("Power", "Power Profile")
        .body(label)
        .urgency(Urgency::Low)
        .sync_id("waybar-power-profile");
    ctx.notifier.send_best_effort(&notif).await;
    Ok(())
}

async fn query_battery_output(ctx: &AppContext) -> Result<WaybarOutput> {
    let conn = ctx.system_bus().await?;
    let (devices_res, display_dev, active_profile) = tokio::join!(
        enumerate_upower_devices(conn),
        fetch_upower_device(conn, UPOWER_DISPLAY_DEVICE_PATH),
        get_active_profile_dbus(conn),
    );

    let devices = devices_res?;
    let display = display_dev.ok();
    let profile_label_opt = active_profile.map(|p| profile_label(&p));

    Ok(build_waybar_output(
        &devices,
        display.as_ref(),
        profile_label_opt.as_deref(),
        &ctx.config,
    ))
}

async fn enumerate_upower_devices(conn: &Connection) -> Result<Vec<UPowerDevice>> {
    let reply = conn
        .call_method(
            Some(UPOWER_DEST),
            UPOWER_PATH,
            Some(UPOWER_IFACE),
            "EnumerateDevices",
            &(),
        )
        .await
        .context("Failed to call org.freedesktop.UPower.EnumerateDevices")?;

    let paths: Vec<OwnedObjectPath> = reply
        .body()
        .deserialize()
        .context("Failed to deserialize UPower device paths")?;

    let mut devices = Vec::with_capacity(paths.len());
    for path in paths {
        match fetch_upower_device(conn, path.as_str()).await {
            Ok(dev) => devices.push(dev),
            Err(err) => debug!("Skipping unreadable UPower device {}: {err:#}", path.as_str()),
        }
    }
    Ok(devices)
}

async fn fetch_upower_device(conn: &Connection, path: &str) -> Result<UPowerDevice> {
    let reply = conn
        .call_method(
            Some(UPOWER_DEST),
            path,
            Some(DBUS_PROPERTIES_IFACE),
            "GetAll",
            &(UPOWER_DEVICE_IFACE,),
        )
        .await
        .with_context(|| format!("Failed to read UPower properties for {path}"))?;

    let props: HashMap<String, OwnedValue> = reply
        .body()
        .deserialize()
        .with_context(|| format!("Failed to deserialize UPower properties for {path}"))?;

    Ok(UPowerDevice {
        path: path.to_string(),
        device_type: prop_u32(&props, "Type").unwrap_or(0),
        power_supply: prop_bool(&props, "PowerSupply").unwrap_or(false),
        is_present: prop_bool(&props, "IsPresent").unwrap_or(false),
        state: prop_u32(&props, "State").unwrap_or(UPOWER_STATE_UNKNOWN),
        percentage: prop_f64(&props, "Percentage"),
        energy_rate: prop_f64(&props, "EnergyRate"),
        capacity: prop_f64(&props, "Capacity").filter(|&c| c.is_finite() && c > 0.0),
        time_to_empty: prop_i64(&props, "TimeToEmpty").unwrap_or(0),
        time_to_full: prop_i64(&props, "TimeToFull").unwrap_or(0),
        model: prop_string(&props, "Model").unwrap_or_default(),
        vendor: prop_string(&props, "Vendor").unwrap_or_default(),
        native_path: prop_string(&props, "NativePath").unwrap_or_default(),
        icon_name: prop_string(&props, "IconName").unwrap_or_default(),
    })
}

async fn fetch_power_profiles(ctx: &AppContext) -> Result<(Option<String>, Vec<String>)> {
    let conn = ctx.system_bus().await?;
    let mut last_err = None;

    for ep in POWER_PROFILES_ENDPOINTS {
        let res = conn
            .call_method(
                Some(ep.dest),
                ep.path,
                Some(DBUS_PROPERTIES_IFACE),
                "GetAll",
                &(ep.iface,),
            )
            .await;

        match res {
            Ok(reply) => {
                let props: HashMap<String, OwnedValue> = reply
                    .body()
                    .deserialize()
                    .context("Failed to deserialize PowerProfiles properties")?;
                let active = prop_string(&props, "ActiveProfile").filter(|s| !s.is_empty());
                let profiles = extract_profile_names(&props);
                return Ok((active, profiles));
            }
            Err(err) => {
                debug!("PowerProfiles GetAll on {} failed: {err}", ep.dest);
                last_err = Some(err);
            }
        }
    }

    Err(anyhow!(
        "PowerProfiles D-Bus service unavailable: {}",
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown error".to_string())
    ))
}

async fn get_active_profile_dbus(conn: &Connection) -> Option<String> {
    for ep in POWER_PROFILES_ENDPOINTS {
        let Ok(reply) = conn
            .call_method(
                Some(ep.dest),
                ep.path,
                Some(DBUS_PROPERTIES_IFACE),
                "Get",
                &(ep.iface, "ActiveProfile"),
            )
            .await
        else {
            continue;
        };

        if let Ok(val) = reply.body().deserialize::<OwnedValue>() {
            if let Some(s) = owned_value_to_string(&val) {
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
    }
    None
}

async fn set_active_profile_dbus(conn: &Connection, profile: &str) -> Result<()> {
    let mut first_err: Option<zbus::Error> = None;

    for (idx, ep) in POWER_PROFILES_ENDPOINTS.iter().enumerate() {
        let val = Value::from(profile);
        match conn
            .call_method(
                Some(ep.dest),
                ep.path,
                Some(DBUS_PROPERTIES_IFACE),
                "Set",
                &(ep.iface, "ActiveProfile", val),
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(err) => {
                debug!("PowerProfiles Set ActiveProfile on {} failed: {err}", ep.dest);
                if idx == 0 {
                    first_err = Some(err);
                } else {
                    let chosen = first_err.unwrap_or(err);
                    return Err(anyhow!(
                        "Failed to set power profile '{profile}': {chosen}"
                    ));
                }
            }
        }
    }

    Err(anyhow!(
        "Failed to set power profile '{profile}': {}",
        first_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "PowerProfiles service unavailable".to_string())
    ))
}

fn extract_profile_names(props: &HashMap<String, OwnedValue>) -> Vec<String> {
    let Some(raw) = props.get("Profiles") else {
        return Vec::new();
    };
    let Ok(items) = Vec::<HashMap<String, OwnedValue>>::try_from(raw.clone()) else {
        return Vec::new();
    };

    let mut names = Vec::new();
    for entry in items {
        if let Some(name) = prop_string(&entry, "Profile") {
            let trimmed = name.trim();
            if !trimmed.is_empty() && !names.iter().any(|existing| existing == trimmed) {
                names.push(trimmed.to_string());
            }
        }
    }
    sort_profiles(&mut names);
    names
}

fn sort_profiles(profiles: &mut [String]) {
    profiles.sort_by(|a, b| {
        let rank_a = PROFILE_ORDER
            .iter()
            .position(|&p| p == a)
            .unwrap_or(PROFILE_ORDER.len());
        let rank_b = PROFILE_ORDER
            .iter()
            .position(|&p| p == b)
            .unwrap_or(PROFILE_ORDER.len());
        rank_a.cmp(&rank_b).then_with(|| a.cmp(b))
    });
}

fn build_waybar_output(
    devices: &[UPowerDevice],
    display_device: Option<&UPowerDevice>,
    active_profile_label: Option<&str>,
    config: &Config,
) -> WaybarOutput {
    let primary = select_primary_battery(devices, display_device);
    let primary_path = primary.map(|d| d.path.as_str()).unwrap_or("");

    let mut peripherals: Vec<PeripheralBattery> = devices
        .iter()
        .filter(|dev| {
            !dev.path.is_empty()
                && dev.path != primary_path
                && !dev.path.ends_with("/DisplayDevice")
                && dev.device_type != UPOWER_TYPE_LINE_POWER
                && dev.is_present
        })
        .filter_map(|dev| {
            let pct = dev.valid_percentage()?;
            Some(PeripheralBattery {
                icon: peripheral_icon(dev, &config.icons).to_string(),
                name: peripheral_name(dev),
                percentage: pct,
            })
        })
        .collect();

    peripherals.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });

    let fmt = TooltipFormatter::new(&config.tooltip);

    let Some(primary) = primary else {
        if let Some(first) = peripherals.first() {
            let mut lines: Vec<String> = peripherals
                .iter()
                .map(|p| fmt.line(&format!("{}  {} · {}%", p.icon, p.name, p.percentage)))
                .collect();
            if let Some(prof) = active_profile_label {
                lines.push(fmt.subtext(prof));
                lines.push(fmt.footer("Left: Power profile"));
            }
            return WaybarOutput {
                text: format!("{} {}%", first.icon, first.percentage),
                tooltip: Some(lines.join("\n")),
                class: Vec::new(),
                percentage: Some(first.percentage),
            };
        }
        return WaybarOutput::empty();
    };

    let pct = primary.valid_percentage().unwrap_or(0);
    let (icon, classes, status_text) = classify_battery_state(
        pct,
        primary.state,
        primary.time_to_empty,
        primary.time_to_full,
        &config.battery,
        &config.icons,
    );

    let mut header_parts = vec![format!("{pct}%"), status_text];
    if let Some(rate) = primary.energy_rate {
        if rate.is_finite() && rate > 0.05 {
            header_parts.push(format!("{rate:.1}W"));
        }
    }

    let mut tooltip_lines = vec![fmt.header(&header_parts.join(" · "))];

    let mut meta_parts = Vec::new();
    if let Some(health) = primary.capacity {
        if health.is_finite() && health > 0.0 {
            meta_parts.push(format!("Health {}%", health.round() as u32));
        }
    }
    if let Some(prof) = active_profile_label {
        meta_parts.push(prof.to_string());
    }
    if !meta_parts.is_empty() {
        tooltip_lines.push(fmt.subtext(&meta_parts.join(" · ")));
    }

    if !peripherals.is_empty() {
        tooltip_lines.push(fmt.divider());
        for p in &peripherals {
            tooltip_lines.push(fmt.line(&format!(
                "{}  {} · {}%",
                p.icon, p.name, p.percentage
            )));
        }
    }

    tooltip_lines.push(fmt.footer("Left: Power profile"));

    WaybarOutput {
        text: format!("{icon} {pct}%"),
        tooltip: Some(tooltip_lines.join("\n")),
        class: classes,
        percentage: Some(pct),
    }
}

fn select_primary_battery<'a>(
    devices: &'a [UPowerDevice],
    display_device: Option<&'a UPowerDevice>,
) -> Option<&'a UPowerDevice> {
    let real_battery = devices.iter().find(|dev| {
        !dev.path.ends_with("/DisplayDevice")
            && dev.device_type == UPOWER_TYPE_BATTERY
            && dev.power_supply
            && dev.is_present
            && dev.valid_percentage().is_some()
    });
    if real_battery.is_some() {
        return real_battery;
    }

    display_device.filter(|dev| {
        dev.device_type == UPOWER_TYPE_BATTERY
            && dev.is_present
            && dev.valid_percentage().is_some()
    })
}

fn classify_battery_state<'a>(
    pct: u8,
    state: u32,
    time_to_empty: i64,
    time_to_full: i64,
    battery_cfg: &BatteryConfig,
    icons: &'a IconsConfig,
) -> (&'a str, Vec<String>, String) {
    match state {
        UPOWER_STATE_CHARGING => {
            let status = match format_duration(time_to_full) {
                Some(t) => format!("Charging ({t} to full)"),
                None => "Charging".to_string(),
            };
            (&icons.battery_charging, vec!["charging".to_string()], status)
        }
        UPOWER_STATE_FULLY_CHARGED | UPOWER_STATE_PENDING_CHARGE => {
            let status = if state == UPOWER_STATE_FULLY_CHARGED {
                "Full".to_string()
            } else {
                "Plugged in".to_string()
            };
            (&icons.battery_plugged, vec!["plugged".to_string()], status)
        }
        _ => {
            let mut classes = Vec::new();
            if pct <= battery_cfg.critical_threshold {
                classes.push("critical".to_string());
            } else if pct <= battery_cfg.warning_threshold {
                classes.push("warning".to_string());
            }
            let status = match format_duration(time_to_empty) {
                Some(t) => format!("{t} left"),
                None => "Discharging".to_string(),
            };
            (battery_icon(pct, icons), classes, status)
        }
    }
}

fn battery_icon(pct: u8, icons: &IconsConfig) -> &str {
    let levels = &icons.battery_levels;
    if levels.is_empty() {
        return &icons.battery_unknown;
    }
    let idx = (usize::from(pct.min(100)) * levels.len() / 101).min(levels.len() - 1);
    &levels[idx]
}

fn format_duration(seconds: i64) -> Option<String> {
    if seconds <= 0 {
        return None;
    }
    if seconds < 60 {
        return Some("< 1m".to_string());
    }
    let total_mins = seconds / 60;
    let hours = total_mins / 60;
    let mins = total_mins % 60;
    match (hours, mins) {
        (0, m) => Some(format!("{m}m")),
        (h, 0) => Some(format!("{h}h")),
        (h, m) => Some(format!("{h}h {m}m")),
    }
}

fn peripheral_icon<'a>(dev: &UPowerDevice, icons: &'a IconsConfig) -> &'a str {
    match dev.device_type {
        UPOWER_TYPE_MONITOR => return &icons.monitor,
        UPOWER_TYPE_MOUSE | UPOWER_TYPE_TOUCHPAD => return &icons.bt_mouse,
        UPOWER_TYPE_KEYBOARD => return &icons.bt_keyboard,
        UPOWER_TYPE_PDA | UPOWER_TYPE_PHONE => return &icons.bt_phone,
        UPOWER_TYPE_TABLET => return &icons.tablet,
        UPOWER_TYPE_COMPUTER => return &icons.bt_computer,
        UPOWER_TYPE_GAMING_INPUT => return &icons.bt_gamepad,
        UPOWER_TYPE_PEN => return &icons.stylus,
        UPOWER_TYPE_HEADSET => return &icons.bt_headset,
        UPOWER_TYPE_SPEAKERS => return &icons.bt_speaker,
        UPOWER_TYPE_HEADPHONES => return &icons.bt_headphones,
        UPOWER_TYPE_VIDEO | UPOWER_TYPE_CAMERA => return &icons.bt_camera,
        _ => {}
    }

    let haystack = format!(
        "{} {} {} {}",
        dev.model, dev.native_path, dev.icon_name, dev.path
    )
    .to_ascii_lowercase();

    if haystack.contains("mouse") || haystack.contains("trackball") || haystack.contains("touchpad") {
        return &icons.bt_mouse;
    }
    if haystack.contains("keyboard") || haystack.contains("kbd") {
        return &icons.bt_keyboard;
    }
    if haystack.contains("headset") {
        return &icons.bt_headset;
    }
    if haystack.contains("headphone")
        || haystack.contains("earbud")
        || haystack.contains("airpod")
        || haystack.contains("buds")
    {
        return &icons.bt_headphones;
    }
    if haystack.contains("gaming")
        || haystack.contains("gamepad")
        || haystack.contains("controller")
        || haystack.contains("joystick")
        || haystack.contains("dualsense")
        || haystack.contains("dualshock")
    {
        return &icons.bt_gamepad;
    }
    if haystack.contains("phone") {
        return &icons.bt_phone;
    }
    if haystack.contains("tablet") {
        return &icons.tablet;
    }
    if haystack.contains("stylus") || haystack.contains("pen") {
        return &icons.stylus;
    }
    if haystack.contains("speaker") {
        return &icons.bt_speaker;
    }
    if haystack.contains("camera") || haystack.contains("webcam") {
        return &icons.bt_camera;
    }
    if haystack.contains("computer") || haystack.contains("laptop") {
        return &icons.bt_computer;
    }

    match dev.device_type {
        UPOWER_TYPE_MEDIA_PLAYER | UPOWER_TYPE_OTHER_AUDIO => &icons.bt_headphones,
        UPOWER_TYPE_BLUETOOTH_GENERIC_26 | UPOWER_TYPE_BLUETOOTH_GENERIC_28 => &icons.bluetooth,
        _ => icons
            .battery_levels
            .last()
            .map(String::as_str)
            .unwrap_or(&icons.battery_unknown),
    }
}

fn peripheral_name(dev: &UPowerDevice) -> String {
    let model = dev.model.trim();
    if !model.is_empty() {
        return model.to_string();
    }
    let vendor = dev.vendor.trim();
    let kind = device_type_label(dev.device_type);
    match (!vendor.is_empty(), kind) {
        (true, Some(k)) => format!("{vendor} {k}"),
        (false, Some(k)) => k.to_string(),
        (true, None) => vendor.to_string(),
        (false, None) => {
            let native = dev.native_path.trim();
            if !native.is_empty() {
                native.to_string()
            } else {
                "Device".to_string()
            }
        }
    }
}

fn device_type_label(device_type: u32) -> Option<&'static str> {
    match device_type {
        UPOWER_TYPE_BATTERY => Some("Battery"),
        UPOWER_TYPE_UPS => Some("UPS"),
        UPOWER_TYPE_MONITOR => Some("Monitor"),
        UPOWER_TYPE_MOUSE => Some("Mouse"),
        UPOWER_TYPE_KEYBOARD => Some("Keyboard"),
        UPOWER_TYPE_PDA => Some("PDA"),
        UPOWER_TYPE_PHONE => Some("Phone"),
        UPOWER_TYPE_MEDIA_PLAYER => Some("Media Player"),
        UPOWER_TYPE_TABLET => Some("Tablet"),
        UPOWER_TYPE_COMPUTER => Some("Computer"),
        UPOWER_TYPE_GAMING_INPUT => Some("Gamepad"),
        UPOWER_TYPE_PEN => Some("Stylus"),
        UPOWER_TYPE_TOUCHPAD => Some("Touchpad"),
        UPOWER_TYPE_MODEM => Some("Modem"),
        UPOWER_TYPE_NETWORK => Some("Network"),
        UPOWER_TYPE_HEADSET => Some("Headset"),
        UPOWER_TYPE_SPEAKERS => Some("Speaker"),
        UPOWER_TYPE_HEADPHONES => Some("Headphones"),
        UPOWER_TYPE_VIDEO => Some("Video"),
        UPOWER_TYPE_OTHER_AUDIO => Some("Audio"),
        UPOWER_TYPE_CAMERA => Some("Camera"),
        UPOWER_TYPE_BLUETOOTH_GENERIC_26 | UPOWER_TYPE_BLUETOOTH_GENERIC_28 => Some("Bluetooth"),
        _ => None,
    }
}

fn profile_meta<'a>(profile: &str, icons: &'a IconsConfig) -> (&'a str, String) {
    let icon = match profile {
        "power-saver" => icons.profile_power_saver.as_str(),
        "balanced" => icons.profile_balanced.as_str(),
        "performance" => icons.profile_performance.as_str(),
        _ => icons.battery_plugged.as_str(),
    };
    (icon, profile_label(profile))
}

fn profile_label(profile: &str) -> String {
    match profile {
        "power-saver" => "Power Saver".to_string(),
        "balanced" => "Balanced".to_string(),
        "performance" => "Performance".to_string(),
        other => other
            .split('-')
            .filter(|part| !part.is_empty())
            .map(|part| {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn prop_u32(props: &HashMap<String, OwnedValue>, key: &str) -> Option<u32> {
    props.get(key).and_then(|v| u32::try_from(v).ok())
}

fn prop_bool(props: &HashMap<String, OwnedValue>, key: &str) -> Option<bool> {
    props.get(key).and_then(|v| bool::try_from(v).ok())
}

fn prop_f64(props: &HashMap<String, OwnedValue>, key: &str) -> Option<f64> {
    props.get(key).and_then(|v| f64::try_from(v).ok())
}

fn prop_i64(props: &HashMap<String, OwnedValue>, key: &str) -> Option<i64> {
    props.get(key).and_then(|v| i64::try_from(v).ok())
}

fn prop_string(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    props.get(key).and_then(owned_value_to_string)
}

fn owned_value_to_string(val: &OwnedValue) -> Option<String> {
    String::try_from(val.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_battery(path: &str, pct: f64, state: u32) -> UPowerDevice {
        UPowerDevice {
            path: path.to_string(),
            device_type: UPOWER_TYPE_BATTERY,
            power_supply: true,
            is_present: true,
            state,
            percentage: Some(pct),
            energy_rate: Some(12.34),
            capacity: Some(91.6),
            time_to_empty: 3 * 3600 + 24 * 60,
            time_to_full: 0,
            model: "BAT0".to_string(),
            vendor: "ACME".to_string(),
            native_path: "BAT0".to_string(),
            icon_name: String::new(),
        }
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(-10), None);
        assert_eq!(format_duration(0), None);
        assert_eq!(format_duration(1), Some("< 1m".to_string()));
        assert_eq!(format_duration(59), Some("< 1m".to_string()));
        assert_eq!(format_duration(60), Some("1m".to_string()));
        assert_eq!(format_duration(45 * 60), Some("45m".to_string()));
        assert_eq!(format_duration(3600), Some("1h".to_string()));
        assert_eq!(format_duration(3 * 3600 + 24 * 60), Some("3h 24m".to_string()));
        assert_eq!(format_duration(10 * 3600 + 5 * 60 + 40), Some("10h 5m".to_string()));
    }

    #[test]
    fn test_battery_icon_indexing() {
        let icons = IconsConfig::default();
        assert_eq!(battery_icon(0, &icons), icons.battery_levels[0]);
        assert_eq!(battery_icon(10, &icons), icons.battery_levels[0]);
        assert_eq!(battery_icon(11, &icons), icons.battery_levels[1]);
        assert_eq!(battery_icon(50, &icons), icons.battery_levels[4]);
        assert_eq!(battery_icon(90, &icons), icons.battery_levels[8]);
        assert_eq!(battery_icon(91, &icons), icons.battery_levels[9]);
        assert_eq!(battery_icon(100, &icons), icons.battery_levels[9]);
        assert_eq!(battery_icon(150, &icons), icons.battery_levels[9]);

        let mut empty_icons = icons.clone();
        empty_icons.battery_levels.clear();
        assert_eq!(battery_icon(50, &empty_icons), empty_icons.battery_unknown);
    }

    #[test]
    fn test_classify_battery_state() {
        let cfg = BatteryConfig {
            warning_threshold: 25,
            critical_threshold: 10,
        };
        let icons = IconsConfig::default();

        // Charging with time to full
        let (ic, cls, st) = classify_battery_state(62, UPOWER_STATE_CHARGING, 0, 45 * 60, &cfg, &icons);
        assert_eq!(ic, icons.battery_charging);
        assert_eq!(cls, vec!["charging"]);
        assert_eq!(st, "Charging (45m to full)");

        // Charging without time estimate
        let (_, cls, st) = classify_battery_state(62, UPOWER_STATE_CHARGING, 0, 0, &cfg, &icons);
        assert_eq!(cls, vec!["charging"]);
        assert_eq!(st, "Charging");

        // Fully charged & pending charge
        let (ic, cls, st) = classify_battery_state(100, UPOWER_STATE_FULLY_CHARGED, 0, 0, &cfg, &icons);
        assert_eq!(ic, icons.battery_plugged);
        assert_eq!(cls, vec!["plugged"]);
        assert_eq!(st, "Full");

        let (ic, cls, st) = classify_battery_state(80, UPOWER_STATE_PENDING_CHARGE, 0, 0, &cfg, &icons);
        assert_eq!(ic, icons.battery_plugged);
        assert_eq!(cls, vec!["plugged"]);
        assert_eq!(st, "Plugged in");

        // Discharging normal / warning / critical
        let (_, cls, st) = classify_battery_state(50, 2, 3 * 3600 + 24 * 60, 0, &cfg, &icons);
        assert!(cls.is_empty());
        assert_eq!(st, "3h 24m left");

        let (_, cls, st) = classify_battery_state(25, 2, 0, 0, &cfg, &icons);
        assert_eq!(cls, vec!["warning"]);
        assert_eq!(st, "Discharging");

        let (_, cls, _) = classify_battery_state(10, 2, 600, 0, &cfg, &icons);
        assert_eq!(cls, vec!["critical"]);
    }

    #[test]
    fn test_primary_selection_and_display_device_fallback() {
        let real = sample_battery("/org/freedesktop/UPower/devices/battery_BAT0", 75.0, 2);
        let display = sample_battery(UPOWER_DISPLAY_DEVICE_PATH, 74.0, 2);

        let devices = vec![real.clone()];
        let chosen = select_primary_battery(&devices, Some(&display)).unwrap();
        assert_eq!(chosen.path, real.path);

        // Fallback to DisplayDevice when no real battery is present
        let mut line_power = UPowerDevice {
            path: "/org/freedesktop/UPower/devices/line_power_AC".to_string(),
            device_type: UPOWER_TYPE_LINE_POWER,
            power_supply: true,
            is_present: true,
            ..Default::default()
        };
        line_power.percentage = Some(0.0);
        let devices = [line_power];
        let chosen = select_primary_battery(&devices, Some(&display)).unwrap();
        assert_eq!(chosen.path, UPOWER_DISPLAY_DEVICE_PATH);

        // DisplayDevice ignored if not Type == Battery or not IsPresent
        let mut absent_display = display.clone();
        absent_display.is_present = false;
        assert!(select_primary_battery(&[], Some(&absent_display)).is_none());

        let mut non_battery_display = display;
        non_battery_display.device_type = UPOWER_TYPE_LINE_POWER;
        assert!(select_primary_battery(&[], Some(&non_battery_display)).is_none());
    }

    #[test]
    fn test_peripheral_icon_resolution() {
        let icons = IconsConfig::default();

        let mouse = UPowerDevice {
            device_type: UPOWER_TYPE_MOUSE,
            ..Default::default()
        };
        assert_eq!(peripheral_icon(&mouse, &icons), icons.bt_mouse);

        let kbd = UPowerDevice {
            device_type: UPOWER_TYPE_KEYBOARD,
            ..Default::default()
        };
        assert_eq!(peripheral_icon(&kbd, &icons), icons.bt_keyboard);

        let headset = UPowerDevice {
            device_type: UPOWER_TYPE_HEADSET,
            ..Default::default()
        };
        assert_eq!(peripheral_icon(&headset, &icons), icons.bt_headset);

        let bt_earbuds = UPowerDevice {
            device_type: UPOWER_TYPE_BLUETOOTH_GENERIC_26,
            model: "AirPods Pro".to_string(),
            ..Default::default()
        };
        assert_eq!(peripheral_icon(&bt_earbuds, &icons), icons.bt_headphones);

        let bt_generic = UPowerDevice {
            device_type: UPOWER_TYPE_BLUETOOTH_GENERIC_26,
            model: "Mystery Gadget".to_string(),
            ..Default::default()
        };
        assert_eq!(peripheral_icon(&bt_generic, &icons), icons.bluetooth);
    }

    #[test]
    fn test_tooltip_formatting_and_escaping() {
        let cfg = Config::default();
        let mut bat = sample_battery("/org/freedesktop/UPower/devices/battery_BAT0", 82.4, 2);
        bat.energy_rate = Some(9.64);
        bat.capacity = Some(94.2);

        let mouse = UPowerDevice {
            path: "/org/freedesktop/UPower/devices/mouse_hid".to_string(),
            device_type: UPOWER_TYPE_MOUSE,
            power_supply: false,
            is_present: true,
            state: 2,
            percentage: Some(65.0),
            model: "MX Master <3> & Co".to_string(),
            ..Default::default()
        };

        let line_power = UPowerDevice {
            path: "/org/freedesktop/UPower/devices/line_power_AC".to_string(),
            device_type: UPOWER_TYPE_LINE_POWER,
            power_supply: true,
            is_present: true,
            ..Default::default()
        };

        let out = build_waybar_output(
            &[line_power, bat, mouse],
            None,
            Some("Balanced"),
            &cfg,
        );

        assert_eq!(out.percentage, Some(82));
        assert!(out.class.is_empty());
        assert!(out.text.ends_with(" 82%"));

        let tooltip = out.tooltip.expect("tooltip must be present");
        assert!(tooltip.contains("<b>82% · 3h 24m left · 9.6W</b>"));
        assert!(tooltip.contains("<span alpha='75%'>Health 94% · Balanced</span>"));
        assert!(tooltip.contains("<span alpha='30%'>────────────────────</span>"));
        assert!(tooltip.contains("MX Master &lt;3&gt; &amp; Co · 65%"));
        assert!(tooltip.contains("<span alpha='55%'>Left: Power profile</span>"));
        // Theme-agnostic defaults must not contain hardcoded hex colors.
        assert!(!tooltip.contains("#cdd6f4"));
    }

    #[test]
    fn test_tooltip_hides_tiny_energy_rate_and_handles_no_battery() {
        let cfg = Config::default();
        let mut bat = sample_battery("/org/freedesktop/UPower/devices/battery_BAT0", 100.0, UPOWER_STATE_FULLY_CHARGED);
        bat.energy_rate = Some(0.03);
        bat.capacity = None;

        let out = build_waybar_output(&[bat], None, None, &cfg);
        let tooltip = out.tooltip.unwrap();
        assert!(tooltip.contains("<b>100% · Full</b>"));
        assert!(!tooltip.contains("0.0W"));
        assert_eq!(out.class, vec!["plugged"]);

        // No primary and no peripherals -> empty WaybarOutput
        let empty = build_waybar_output(&[], None, Some("Balanced"), &cfg);
        assert_eq!(empty, WaybarOutput::empty());
    }

    #[test]
    fn test_profile_sorting_and_labels() {
        let mut profs = vec![
            "performance".to_string(),
            "custom-mode".to_string(),
            "balanced".to_string(),
            "power-saver".to_string(),
        ];
        sort_profiles(&mut profs);
        assert_eq!(
            profs,
            vec!["power-saver", "balanced", "performance", "custom-mode"]
        );
        assert_eq!(profile_label("power-saver"), "Power Saver");
        assert_eq!(profile_label("balanced"), "Balanced");
        assert_eq!(profile_label("performance"), "Performance");
        assert_eq!(profile_label("ultra-quiet"), "Ultra Quiet");
    }

    #[test]
    fn test_extract_profile_names_and_props() {
        let mut p1: HashMap<String, Value<'_>> = HashMap::new();
        p1.insert("Profile".to_string(), Value::from("performance"));
        p1.insert("Driver".to_string(), Value::from("platform_profile"));

        let mut p2: HashMap<String, Value<'_>> = HashMap::new();
        p2.insert("Profile".to_string(), Value::from("power-saver"));

        let mut p3: HashMap<String, Value<'_>> = HashMap::new();
        p3.insert("Profile".to_string(), Value::from("balanced"));

        let profiles_val = OwnedValue::try_from(Value::from(vec![p1, p2, p3])).unwrap();

        let mut props: HashMap<String, OwnedValue> = HashMap::new();
        props.insert("Profiles".to_string(), profiles_val);
        props.insert(
            "ActiveProfile".to_string(),
            OwnedValue::try_from(Value::from("balanced")).unwrap(),
        );
        props.insert(
            "Type".to_string(),
            OwnedValue::try_from(Value::U32(2)).unwrap(),
        );
        props.insert(
            "IsPresent".to_string(),
            OwnedValue::try_from(Value::Bool(true)).unwrap(),
        );
        props.insert(
            "Percentage".to_string(),
            OwnedValue::try_from(Value::F64(78.5)).unwrap(),
        );
        props.insert(
            "TimeToEmpty".to_string(),
            OwnedValue::try_from(Value::I64(3600)).unwrap(),
        );

        assert_eq!(
            extract_profile_names(&props),
            vec!["power-saver", "balanced", "performance"]
        );
        assert_eq!(
            prop_string(&props, "ActiveProfile").as_deref(),
            Some("balanced")
        );
        assert_eq!(prop_u32(&props, "Type"), Some(2));
        assert_eq!(prop_bool(&props, "IsPresent"), Some(true));
        assert_eq!(prop_f64(&props, "Percentage"), Some(78.5));
        assert_eq!(prop_i64(&props, "TimeToEmpty"), Some(3600));
    }
}
