use crate::core::clipboard;
use crate::core::fuzzel::{self, Anchor, FuzzelOptions};
use crate::core::hyprland;
use crate::core::notify::{Notification, Urgency};
use crate::core::process::spawn_reaped;
use crate::daemon::menu::{MenuGuard, MenuKind, MenuSession};
use crate::daemon::AppContext;
use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, warn};
use zbus::proxy::CacheProperties;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::Connection;

const NM_SERVICE: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";

// NMDeviceType constants
const NM_DEVICE_TYPE_ETHERNET: u32 = 1;
const NM_DEVICE_TYPE_WIFI: u32 = 2;
const NM_DEVICE_TYPE_MODEM: u32 = 8;
const NM_DEVICE_TYPE_BOND: u32 = 10;
const NM_DEVICE_TYPE_VLAN: u32 = 11;
const NM_DEVICE_TYPE_BRIDGE: u32 = 13;
const NM_DEVICE_TYPE_GENERIC: u32 = 14;
const NM_DEVICE_TYPE_TUN: u32 = 16;
const NM_DEVICE_TYPE_VETH: u32 = 20;
const NM_DEVICE_TYPE_DUMMY: u32 = 22;
const NM_DEVICE_TYPE_WIREGUARD: u32 = 29;
const NM_DEVICE_TYPE_WIFI_P2P: u32 = 30;
const NM_DEVICE_TYPE_LOOPBACK: u32 = 32;

// NMDeviceState constants
const NM_DEVICE_STATE_UNMANAGED: u32 = 10;
const NM_DEVICE_STATE_UNAVAILABLE: u32 = 20;
const NM_DEVICE_STATE_DISCONNECTED: u32 = 30;
const NM_DEVICE_STATE_PREPARE: u32 = 40;
const NM_DEVICE_STATE_CONFIG: u32 = 50;
const NM_DEVICE_STATE_NEED_AUTH: u32 = 60;
const NM_DEVICE_STATE_IP_CONFIG: u32 = 70;
const NM_DEVICE_STATE_IP_CHECK: u32 = 80;
const NM_DEVICE_STATE_SECONDARIES: u32 = 90;
const NM_DEVICE_STATE_ACTIVATED: u32 = 100;
const NM_DEVICE_STATE_DEACTIVATING: u32 = 110;
const NM_DEVICE_STATE_FAILED: u32 = 120;

// NMActiveConnectionState constants
const NM_ACTIVE_CONN_ACTIVATED: u32 = 2;
const NM_ACTIVE_CONN_DEACTIVATING: u32 = 3;
const NM_ACTIVE_CONN_DEACTIVATED: u32 = 4;

// NMActiveConnectionStateReason constants
const NM_ACTIVE_CONN_REASON_NO_SECRETS: u32 = 9;
const NM_ACTIVE_CONN_REASON_LOGIN_FAILED: u32 = 10;

// NM80211ApFlags & NM80211ApSecurityFlags constants
pub const NM_802_11_AP_FLAGS_PRIVACY: u32 = 0x0000_0001;
pub const NM_802_11_AP_SEC_KEY_MGMT_PSK: u32 = 0x0000_0100;
pub const NM_802_11_AP_SEC_KEY_MGMT_802_1X: u32 = 0x0000_0200;
pub const NM_802_11_AP_SEC_KEY_MGMT_SAE: u32 = 0x0000_0400;
pub const NM_802_11_AP_SEC_KEY_MGMT_OWE: u32 = 0x0000_0800;
pub const NM_802_11_AP_SEC_KEY_MGMT_OWE_TM: u32 = 0x0000_1000;
pub const NM_802_11_AP_SEC_KEY_MGMT_EAP_SUITE_B_192: u32 = 0x0000_2000;

const SCAN_TIMEOUT: Duration = Duration::from_secs(3);
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(18);

type SettingsMap = HashMap<String, HashMap<String, OwnedValue>>;

// ---------------------------------------------------------------------------
// NetworkManager zbus D-Bus Proxies
// ---------------------------------------------------------------------------

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManager {
    fn enable(&self, enable: bool) -> zbus::Result<()>;
    fn get_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    fn activate_connection(
        &self,
        connection: &ObjectPath<'_>,
        device: &ObjectPath<'_>,
        specific_object: &ObjectPath<'_>,
    ) -> zbus::Result<OwnedObjectPath>;
    fn add_and_activate_connection(
        &self,
        connection: HashMap<&str, HashMap<&str, Value<'_>>>,
        device: &ObjectPath<'_>,
        specific_object: &ObjectPath<'_>,
    ) -> zbus::Result<(OwnedObjectPath, OwnedObjectPath)>;
    fn deactivate_connection(&self, active_connection: &ObjectPath<'_>) -> zbus::Result<()>;

    #[zbus(property)]
    fn networking_enabled(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn wireless_enabled(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn wireless_hardware_enabled(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_wireless_enabled(&self, value: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn active_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Device",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Device {
    #[zbus(property)]
    fn interface(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn device_type(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn hw_address(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn active_connection(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn ip4_config(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn ip6_config(&self) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Device.Wireless",
    default_service = "org.freedesktop.NetworkManager"
)]
trait WirelessDevice {
    fn get_all_access_points(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    fn request_scan(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;

    #[zbus(property)]
    fn active_access_point(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn bitrate(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn last_scan(&self) -> zbus::Result<i64>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.AccessPoint",
    default_service = "org.freedesktop.NetworkManager"
)]
trait AccessPoint {
    #[zbus(property)]
    fn flags(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn wpa_flags(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn rsn_flags(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn ssid(&self) -> zbus::Result<Vec<u8>>;
    #[zbus(property)]
    fn strength(&self) -> zbus::Result<u8>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Connection.Active",
    default_service = "org.freedesktop.NetworkManager"
)]
trait ActiveConnection {
    #[zbus(signal, name = "StateChanged")]
    fn nm_state_changed(&self, state: u32, reason: u32) -> zbus::Result<()>;

    #[zbus(property)]
    fn connection(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn uuid(&self) -> zbus::Result<String>;
    #[zbus(property, name = "Type")]
    fn conn_type(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.IP4Config",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Ip4Config {
    #[zbus(property)]
    fn address_data(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
    #[zbus(property)]
    fn gateway(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn nameserver_data(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
    #[zbus(property)]
    fn nameservers(&self) -> zbus::Result<Vec<u32>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.IP6Config",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Ip6Config {
    #[zbus(property)]
    fn address_data(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Settings",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager/Settings"
)]
trait Settings {
    fn list_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Settings.Connection",
    default_service = "org.freedesktop.NetworkManager"
)]
trait SettingsConnection {
    fn get_settings(&self) -> zbus::Result<SettingsMap>;
    fn get_secrets(&self, setting_name: &str) -> zbus::Result<SettingsMap>;
    fn update(&self, properties: HashMap<&str, HashMap<&str, Value<'_>>>) -> zbus::Result<()>;
    fn delete(&self) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// Pure Types & Helpers (tested in #[cfg(test)])
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiSecurity {
    Open,
    Owe,
    Wep,
    WpaPsk,
    Wpa2Psk,
    Wpa3Sae,
    Wpa2Wpa3Sae,
    Enterprise,
}

impl WifiSecurity {
    pub fn is_secured(self) -> bool {
        !matches!(self, Self::Open | Self::Owe)
    }

    pub fn key_mgmt(self) -> Option<&'static str> {
        match self {
            Self::Open | Self::Owe => None,
            Self::Wep => Some("none"),
            Self::WpaPsk | Self::Wpa2Psk => Some("wpa-psk"),
            Self::Wpa3Sae | Self::Wpa2Wpa3Sae => Some("sae"),
            Self::Enterprise => Some("wpa-eap"),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Owe => "OWE",
            Self::Wep => "WEP",
            Self::WpaPsk => "WPA",
            Self::Wpa2Psk => "WPA2",
            Self::Wpa3Sae => "WPA3",
            Self::Wpa2Wpa3Sae => "WPA2/WPA3",
            Self::Enterprise => "802.1X",
        }
    }

    fn priority(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::Owe => 1,
            Self::Wep => 2,
            Self::WpaPsk => 3,
            Self::Wpa2Psk => 4,
            Self::Wpa2Wpa3Sae => 5,
            Self::Wpa3Sae => 6,
            Self::Enterprise => 7,
        }
    }
}

pub fn classify_ap_security(flags: u32, wpa_flags: u32, rsn_flags: u32) -> WifiSecurity {
    let has_sae = (rsn_flags & NM_802_11_AP_SEC_KEY_MGMT_SAE) != 0;
    let has_rsn_psk = (rsn_flags & NM_802_11_AP_SEC_KEY_MGMT_PSK) != 0;
    let has_wpa_psk = (wpa_flags & NM_802_11_AP_SEC_KEY_MGMT_PSK) != 0;
    let has_eap = ((wpa_flags | rsn_flags)
        & (NM_802_11_AP_SEC_KEY_MGMT_802_1X | NM_802_11_AP_SEC_KEY_MGMT_EAP_SUITE_B_192))
        != 0;
    let has_owe = (rsn_flags
        & (NM_802_11_AP_SEC_KEY_MGMT_OWE | NM_802_11_AP_SEC_KEY_MGMT_OWE_TM))
        != 0;

    if has_sae && (has_rsn_psk || has_wpa_psk) {
        WifiSecurity::Wpa2Wpa3Sae
    } else if has_sae {
        WifiSecurity::Wpa3Sae
    } else if has_rsn_psk {
        WifiSecurity::Wpa2Psk
    } else if has_wpa_psk {
        WifiSecurity::WpaPsk
    } else if has_eap {
        WifiSecurity::Enterprise
    } else if has_owe {
        WifiSecurity::Owe
    } else if rsn_flags != 0 {
        WifiSecurity::Wpa2Psk
    } else if wpa_flags != 0 {
        WifiSecurity::WpaPsk
    } else if (flags & NM_802_11_AP_FLAGS_PRIVACY) != 0 {
        WifiSecurity::Wep
    } else {
        WifiSecurity::Open
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiNetwork {
    pub ssid: String,
    pub ssid_bytes: Vec<u8>,
    pub signal: u8,
    pub security: WifiSecurity,
    pub active: bool,
    pub ap_path: String,
    pub device_path: String,
}

pub fn deduplicate_and_sort_wifi(aps: Vec<WifiNetwork>) -> Vec<WifiNetwork> {
    let mut by_ssid: HashMap<String, WifiNetwork> = HashMap::new();

    for ap in aps {
        if ap.ssid.trim().is_empty() {
            continue;
        }
        match by_ssid.get_mut(&ap.ssid) {
            None => {
                by_ssid.insert(ap.ssid.clone(), ap);
            }
            Some(existing) => {
                let became_active = ap.active && !existing.active;
                if ap.active {
                    existing.active = true;
                }
                if became_active || (!existing.active && ap.signal > existing.signal) {
                    existing.ap_path = ap.ap_path;
                    existing.device_path = ap.device_path;
                    existing.ssid_bytes = ap.ssid_bytes;
                }
                if ap.signal > existing.signal {
                    existing.signal = ap.signal;
                }
                if ap.security.priority() > existing.security.priority() {
                    existing.security = ap.security;
                }
            }
        }
    }

    let mut list: Vec<WifiNetwork> = by_ssid.into_values().collect();
    list.sort_by(|a, b| {
        (!a.active)
            .cmp(&(!b.active))
            .then_with(|| b.signal.cmp(&a.signal))
            .then_with(|| {
                a.ssid
                    .to_ascii_lowercase()
                    .cmp(&b.ssid.to_ascii_lowercase())
            })
            .then_with(|| a.ssid.cmp(&b.ssid))
    });
    list
}

pub fn wifi_signal_icon(signal: u8, levels: &[String]) -> &str {
    if levels.is_empty() {
        return "󰤨";
    }
    let clamped = usize::from(signal.min(100));
    let idx = (clamped * levels.len() / 101).min(levels.len() - 1);
    &levels[idx]
}

pub fn is_global_ipv6(addr: &str) -> bool {
    let ip_part = addr.split('/').next().unwrap_or("").trim();
    if ip_part.is_empty() {
        return false;
    }
    if let Ok(ip) = ip_part.parse::<Ipv6Addr>() {
        let is_link_local = (ip.segments()[0] & 0xffc0) == 0xfe80;
        !is_link_local && !ip.is_loopback() && !ip.is_unspecified()
    } else {
        !ip_part.to_ascii_lowercase().starts_with("fe80:")
    }
}

pub fn format_bitrate(kbps: u32) -> String {
    if kbps >= 1000 {
        if kbps.is_multiple_of(1000) {
            format!("{} Mbit/s", kbps / 1000)
        } else {
            format!("{:.1} Mbit/s", f64::from(kbps) / 1000.0)
        }
    } else {
        format!("{kbps} Kbit/s")
    }
}

fn device_type_name(dtype: u32) -> &'static str {
    match dtype {
        NM_DEVICE_TYPE_ETHERNET => "ethernet",
        NM_DEVICE_TYPE_WIFI => "wifi",
        NM_DEVICE_TYPE_MODEM => "modem",
        NM_DEVICE_TYPE_BOND => "bond",
        NM_DEVICE_TYPE_VLAN => "vlan",
        NM_DEVICE_TYPE_BRIDGE => "bridge",
        NM_DEVICE_TYPE_GENERIC => "generic",
        NM_DEVICE_TYPE_TUN => "tun",
        NM_DEVICE_TYPE_VETH => "veth",
        NM_DEVICE_TYPE_DUMMY => "dummy",
        NM_DEVICE_TYPE_WIREGUARD => "wireguard",
        NM_DEVICE_TYPE_WIFI_P2P => "wifi-p2p",
        NM_DEVICE_TYPE_LOOPBACK => "loopback",
        _ => "other",
    }
}

fn device_state_name(state: u32) -> &'static str {
    match state {
        NM_DEVICE_STATE_UNMANAGED => "unmanaged",
        NM_DEVICE_STATE_UNAVAILABLE => "unavailable",
        NM_DEVICE_STATE_DISCONNECTED => "disconnected",
        NM_DEVICE_STATE_PREPARE => "connecting",
        NM_DEVICE_STATE_CONFIG => "configuring",
        NM_DEVICE_STATE_NEED_AUTH => "authenticating",
        NM_DEVICE_STATE_IP_CONFIG => "obtaining IP",
        NM_DEVICE_STATE_IP_CHECK => "checking IP",
        NM_DEVICE_STATE_SECONDARIES => "connecting",
        NM_DEVICE_STATE_ACTIVATED => "connected",
        NM_DEVICE_STATE_DEACTIVATING => "disconnecting",
        NM_DEVICE_STATE_FAILED => "failed",
        _ => "unknown",
    }
}

fn activation_reason_message(reason: u32) -> &'static str {
    match reason {
        2 => "Disconnected by user",
        3 => "Device disconnected",
        4 => "Network service stopped",
        5 => "IP configuration failed",
        6 => "Connection timed out",
        7 => "Service start timed out",
        8 => "Service failed to start",
        NM_ACTIVE_CONN_REASON_NO_SECRETS => "Authentication secrets were required",
        NM_ACTIVE_CONN_REASON_LOGIN_FAILED => "Authentication failed",
        11 => "Connection profile was removed",
        12 => "Dependency failed",
        13 => "Device failed to initialize",
        14 => "Device was removed",
        _ => "Connection failed",
    }
}

// ---------------------------------------------------------------------------
// Internal Data Models
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct DeviceInfo {
    path: OwnedObjectPath,
    interface: String,
    device_type: u32,
    state: u32,
    hw_address: String,
    active_connection: OwnedObjectPath,
    ip4_config: OwnedObjectPath,
    ip6_config: OwnedObjectPath,
}

#[derive(Debug, Clone)]
struct ActiveConnInfo {
    path: OwnedObjectPath,
    settings_path: OwnedObjectPath,
    id: String,
    uuid: String,
    conn_type: String,
    devices: Vec<OwnedObjectPath>,
}

#[derive(Debug, Clone)]
struct SavedProfile {
    path: OwnedObjectPath,
    id: String,
    uuid: String,
    conn_type: String,
    wifi_ssid: Option<Vec<u8>>,
    wifi_mode: Option<String>,
}

#[derive(Debug, Clone)]
struct ActiveHotspot {
    active_path: OwnedObjectPath,
    settings_path: OwnedObjectPath,
    ssid: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuActionResult {
    Exit,
    Back,
    Rescan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivationOutcome {
    Activated,
    Failed(u32),
    TimedOut,
}

// ---------------------------------------------------------------------------
// Entry Points
// ---------------------------------------------------------------------------

static WIFI_TOGGLE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn toggle_wifi(ctx: &AppContext) -> Result<()> {
    let _guard = WIFI_TOGGLE_LOCK.lock().await;

    let conn = match ctx.system_bus().await {
        Ok(conn) => conn,
        Err(err) => {
            notify_err(ctx, "Failed to toggle Wi-Fi", &err.to_string()).await;
            return Err(err);
        }
    };
    let nm = match nm_proxy(conn).await {
        Ok(nm) => nm,
        Err(err) => {
            notify_err(ctx, "Failed to toggle Wi-Fi", &err.to_string()).await;
            return Err(err);
        }
    };
    let enabled = match nm.wireless_enabled().await {
        Ok(enabled) => enabled,
        Err(err) => {
            notify_err(ctx, "Failed to toggle Wi-Fi", &err.to_string()).await;
            return Err(err.into());
        }
    };

    set_wifi_enabled(ctx, &nm, !enabled).await
}

async fn set_wifi_enabled(
    ctx: &AppContext,
    nm: &NetworkManagerProxy<'_>,
    enable: bool,
) -> Result<()> {
    match nm.set_wireless_enabled(enable).await {
        Ok(()) => {
            if enable
                && matches!(nm.wireless_enabled().await, Ok(false))
                && matches!(nm.wireless_hardware_enabled().await, Ok(false))
            {
                let msg = "Wi-Fi is blocked by a hardware switch";
                notify_err(ctx, "Failed to enable Wi-Fi", msg).await;
                return Err(anyhow!("{msg}"));
            }
            notify_info(
                ctx,
                if enable {
                    "Wi-Fi enabled"
                } else {
                    "Wi-Fi disabled"
                },
                "",
            )
            .await;
            Ok(())
        }
        Err(err) => {
            notify_err(
                ctx,
                if enable {
                    "Failed to enable Wi-Fi"
                } else {
                    "Failed to disable Wi-Fi"
                },
                &err.to_string(),
            )
            .await;
            Err(err.into())
        }
    }
}

pub async fn run_menu(ctx: &AppContext) -> Result<()> {
    let guard = match ctx.menu.begin_session(MenuKind::Network) {
        MenuSession::ToggledOff => return Ok(()),
        MenuSession::Acquired(guard) => guard,
    };

    let output = hyprland::detect_cursor_output().await;
    let conn = ctx.system_bus().await?;

    let mut rescan = false;
    loop {
        if !guard.is_current() {
            return Ok(());
        }
        match show_main_menu(ctx, conn, &guard, output.as_deref(), rescan).await? {
            MenuActionResult::Exit => return Ok(()),
            MenuActionResult::Back => rescan = false,
            MenuActionResult::Rescan => rescan = true,
        }
    }
}

// ---------------------------------------------------------------------------
// Main Menu
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum MainAction {
    NetOn,
    NetOff,
    WifiOn,
    WifiOff,
    Wifi(WifiNetwork),
    Ethernet(DeviceInfo),
    Rescan,
    HiddenWifi(Option<DeviceInfo>),
    Hotspot(Option<DeviceInfo>),
    Vpn,
    Info,
    Settings,
}

async fn show_main_menu(
    ctx: &AppContext,
    conn: &Connection,
    guard: &MenuGuard,
    output: Option<&str>,
    rescan: bool,
) -> Result<MenuActionResult> {
    let icons = &ctx.config.icons;
    let nm = nm_proxy(conn).await?;
    let net_enabled = nm.networking_enabled().await.unwrap_or(true);

    let mut entries: Vec<(String, MainAction)> = Vec::new();
    let wifi_strong = icons
        .wifi_levels
        .last()
        .map(String::as_str)
        .unwrap_or("󰤨");

    if !net_enabled {
        entries.push((
            format!("{}  Enable Networking", icons.toggle_on),
            MainAction::NetOn,
        ));
        entries.push((
            format!("{}  Edit Connections...", icons.settings),
            MainAction::Settings,
        ));
    } else {
        let devices = list_devices(conn, &nm).await;
        let active_conns = list_active_connections(conn, &nm).await;
        let saved_profiles = list_saved_profiles(conn).await;

        let active_by_path: HashMap<&str, &ActiveConnInfo> = active_conns
            .iter()
            .map(|c| (c.path.as_str(), c))
            .collect();

        let eth_devs: Vec<DeviceInfo> = devices
            .iter()
            .filter(|d| d.device_type == NM_DEVICE_TYPE_ETHERNET)
            .cloned()
            .collect();
        let wifi_devs: Vec<DeviceInfo> = devices
            .iter()
            .filter(|d| d.device_type == NM_DEVICE_TYPE_WIFI)
            .cloned()
            .collect();
        let primary_wifi = wifi_devs.first().cloned();

        // Ethernet entries
        for eth in &eth_devs {
            let label = if eth.state == NM_DEVICE_STATE_ACTIVATED {
                let conn_name = active_by_path
                    .get(eth.active_connection.as_str())
                    .map(|c| c.id.as_str())
                    .unwrap_or(eth.interface.as_str());
                format!("{}  {}  {}", icons.ethernet, conn_name, icons.check)
            } else if eth.state == NM_DEVICE_STATE_UNAVAILABLE {
                format!("{}  Ethernet (Unplugged)", icons.ethernet_off)
            } else {
                format!(
                    "{}  Ethernet ({})",
                    icons.ethernet,
                    device_state_name(eth.state)
                )
            };
            entries.push((label, MainAction::Ethernet(eth.clone())));
        }

        // Wi-Fi entries
        let w_enabled = nm.wireless_enabled().await.unwrap_or(false);
        if !wifi_devs.is_empty() && w_enabled {
            let active_ap = find_active_hotspot(conn, &active_conns).await;
            if let Some(ref hs) = active_ap {
                entries.push((
                    format!("{}  Hotspot: {}  {}", icons.hotspot, hs.ssid, icons.check),
                    MainAction::Hotspot(primary_wifi.clone()),
                ));
            }

            if rescan {
                request_scan_all(conn, &wifi_devs).await;
            }
            let mut networks = list_wifi_networks(conn, &wifi_devs).await;
            if networks.is_empty() && !rescan {
                request_scan_all(conn, &wifi_devs).await;
                networks = list_wifi_networks(conn, &wifi_devs).await;
            }

            for net in networks {
                let sig_icon = wifi_signal_icon(net.signal, &icons.wifi_levels);
                let lock = if net.security.is_secured() {
                    format!(" {}", icons.lock_badge)
                } else {
                    String::new()
                };
                let mark = if net.active {
                    format!("  {}", icons.check)
                } else {
                    String::new()
                };
                let label = format!("{sig_icon}{lock}  {} ({}%){mark}", net.ssid, net.signal);
                entries.push((label, MainAction::Wifi(net)));
            }

            entries.push((format!("{}  Rescan Wi-Fi", icons.refresh), MainAction::Rescan));
            entries.push((
                "󱛃  Hidden Wi-Fi Network...".to_string(),
                MainAction::HiddenWifi(primary_wifi.clone()),
            ));
            if active_ap.is_none() {
                entries.push((
                    format!("{}  Create Wi-Fi Hotspot...", icons.hotspot),
                    MainAction::Hotspot(primary_wifi.clone()),
                ));
            }
        } else if !wifi_devs.is_empty() {
            entries.push((format!("{wifi_strong}  Enable Wi-Fi"), MainAction::WifiOn));
        }

        // VPN / WireGuard entry
        let active_uuids: HashSet<&str> = active_conns.iter().map(|c| c.uuid.as_str()).collect();
        let vpns: Vec<&SavedProfile> = saved_profiles
            .iter()
            .filter(|p| is_vpn_type(&p.conn_type))
            .collect();
        let active_vpns: Vec<&SavedProfile> = vpns
            .iter()
            .copied()
            .filter(|v| active_uuids.contains(v.uuid.as_str()))
            .collect();
        let vpn_label = if let Some(first) = active_vpns.first() {
            format!("{}  VPN: {}  {}", icons.vpn, first.id, icons.check)
        } else if !vpns.is_empty() {
            format!("{}  VPN Connections ({})...", icons.vpn, vpns.len())
        } else {
            format!("{}  VPN Connections...", icons.vpn)
        };
        entries.push((vpn_label, MainAction::Vpn));

        // Info & toggles
        entries.push((
            format!("{}  Connection Information", icons.info),
            MainAction::Info,
        ));
        if !wifi_devs.is_empty() && w_enabled {
            entries.push((format!("{}  Disable Wi-Fi", icons.wifi_off), MainAction::WifiOff));
        }
        entries.push((
            format!("{}  Disable Networking", icons.toggle_off),
            MainAction::NetOff,
        ));
        entries.push((
            format!("{}  Edit Connections...", icons.settings),
            MainAction::Settings,
        ));
    }

    let opts = menu_opts(
        output,
        format!("{wifi_strong}  "),
        "Search networks or actions...",
        16,
    );
    let Some(idx) = fuzzel::select(&ctx.config.fuzzel, guard, &entries, &opts).await? else {
        return Ok(MenuActionResult::Exit);
    };

    match &entries[idx].1 {
        MainAction::NetOn => {
            match nm.enable(true).await {
                Ok(()) => notify_info(ctx, "Networking enabled", "").await,
                Err(err) => notify_err(ctx, "Failed to enable networking", &err.to_string()).await,
            }
            Ok(MenuActionResult::Exit)
        }
        MainAction::NetOff => {
            match nm.enable(false).await {
                Ok(()) => notify_info(ctx, "Networking disabled", "").await,
                Err(err) => notify_err(ctx, "Failed to disable networking", &err.to_string()).await,
            }
            Ok(MenuActionResult::Exit)
        }
        MainAction::WifiOn => {
            let _guard = WIFI_TOGGLE_LOCK.lock().await;
            let _ = set_wifi_enabled(ctx, &nm, true).await;
            Ok(MenuActionResult::Exit)
        }
        MainAction::WifiOff => {
            let _guard = WIFI_TOGGLE_LOCK.lock().await;
            let _ = set_wifi_enabled(ctx, &nm, false).await;
            Ok(MenuActionResult::Exit)
        }
        MainAction::Rescan => Ok(MenuActionResult::Rescan),
        MainAction::Wifi(net) => {
            if net.active {
                show_active_wifi_actions(ctx, conn, guard, output, net).await
            } else {
                connect_wifi(ctx, conn, guard, output, net).await?;
                Ok(MenuActionResult::Exit)
            }
        }
        MainAction::Ethernet(eth) => handle_ethernet(ctx, conn, guard, output, eth).await,
        MainAction::HiddenWifi(wifi_dev) => {
            connect_hidden_wifi(ctx, conn, guard, output, wifi_dev.as_ref()).await?;
            Ok(MenuActionResult::Exit)
        }
        MainAction::Hotspot(wifi_dev) => {
            handle_hotspot(ctx, conn, guard, output, wifi_dev.as_ref()).await
        }
        MainAction::Vpn => handle_vpn(ctx, conn, guard, output).await,
        MainAction::Info => show_connection_info(ctx, conn, guard, output, None).await,
        MainAction::Settings => {
            open_connection_editor(ctx).await;
            Ok(MenuActionResult::Exit)
        }
    }
}

// ---------------------------------------------------------------------------
// Wi-Fi Actions (Active, Connect, Hidden, Rescan)
// ---------------------------------------------------------------------------

async fn show_active_wifi_actions(
    ctx: &AppContext,
    conn: &Connection,
    guard: &MenuGuard,
    output: Option<&str>,
    net: &WifiNetwork,
) -> Result<MenuActionResult> {
    let icons = &ctx.config.icons;
    let nm = nm_proxy(conn).await?;
    let active_conns = list_active_connections(conn, &nm).await;
    let devices = list_devices(conn, &nm).await;

    let active_wifi = active_conns
        .iter()
        .find(|c| {
            c.conn_type == "802-11-wireless"
                && c.devices.iter().any(|d| d.as_str() == net.device_path)
        })
        .or_else(|| {
            active_conns
                .iter()
                .find(|c| c.conn_type == "802-11-wireless")
        })
        .cloned();

    let dev_interface = devices
        .iter()
        .find(|d| d.path.as_str() == net.device_path)
        .map(|d| d.interface.clone());

    #[derive(Clone, Copy)]
    enum ActiveWifiAction {
        Info,
        Disconnect,
        Forget,
        Back,
    }

    let entries = vec![
        (
            format!("{}  Connection Information", icons.info),
            ActiveWifiAction::Info,
        ),
        ("󰌙  Disconnect".to_string(), ActiveWifiAction::Disconnect),
        (
            format!("{}  Forget Network", icons.trash),
            ActiveWifiAction::Forget,
        ),
        (format!("{}  Back", icons.back), ActiveWifiAction::Back),
    ];

    let wifi_icon = icons
        .wifi_levels
        .last()
        .map(String::as_str)
        .unwrap_or("󰤨");
    let opts = menu_opts(
        output,
        format!("{wifi_icon}  {} ", net.ssid),
        "Choose action...",
        6,
    );
    let Some(idx) = fuzzel::select(&ctx.config.fuzzel, guard, &entries, &opts).await? else {
        return Ok(MenuActionResult::Exit);
    };

    match entries[idx].1 {
        ActiveWifiAction::Info => {
            show_connection_info(ctx, conn, guard, output, dev_interface.as_deref()).await
        }
        ActiveWifiAction::Disconnect => {
            if let Some(active) = active_wifi {
                match nm.deactivate_connection(&active.path).await {
                    Ok(()) => notify_info(ctx, "Wi-Fi disconnected", &net.ssid).await,
                    Err(err) => {
                        notify_err(ctx, "Failed to disconnect", &err.to_string()).await;
                    }
                }
            }
            Ok(MenuActionResult::Exit)
        }
        ActiveWifiAction::Forget => {
            let confirm_opts = menu_opts(
                output,
                format!("{}  ", icons.trash),
                "Confirm...",
                2,
            );
            let confirmed = fuzzel::confirm(
                &ctx.config.fuzzel,
                guard,
                &format!("{}  Yes, forget {}", icons.trash, net.ssid),
                &format!("{}  Cancel", icons.cancel),
                &confirm_opts,
            )
            .await?;

            if confirmed {
                if let Some(active) = active_wifi {
                    if !is_root_path(&active.settings_path) {
                        match delete_settings_connection(conn, &active.settings_path).await {
                            Ok(()) => notify_info(ctx, "Network forgotten", &net.ssid).await,
                            Err(err) => {
                                notify_err(ctx, "Failed to forget network", &err.to_string()).await;
                            }
                        }
                    }
                }
            }
            Ok(MenuActionResult::Exit)
        }
        ActiveWifiAction::Back => Ok(MenuActionResult::Back),
    }
}

async fn connect_wifi(
    ctx: &AppContext,
    conn: &Connection,
    guard: &MenuGuard,
    output: Option<&str>,
    net: &WifiNetwork,
) -> Result<()> {
    let nm = nm_proxy(conn).await?;
    let saved_profiles = list_saved_profiles(conn).await;

    let saved_match = saved_profiles.iter().find(|p| {
        p.conn_type == "802-11-wireless"
            && p.wifi_mode.as_deref() != Some("ap")
            && p.wifi_ssid.as_deref() == Some(net.ssid_bytes.as_slice())
    });

    let dev_path = ObjectPath::try_from(net.device_path.as_str())
        .context("Invalid Wi-Fi device object path")?;
    let ap_path = ObjectPath::try_from(net.ap_path.as_str())
        .unwrap_or_else(|_| root_path());

    // 1. Try activating an existing saved profile first.
    if let Some(saved) = saved_match {
        match nm
            .activate_connection(&saved.path, &dev_path, &ap_path)
            .await
        {
            Ok(active_path) => match wait_for_activation(conn, &active_path).await {
                ActivationOutcome::Activated => {
                    notify_info(ctx, "Connected to Wi-Fi", &net.ssid).await;
                    return Ok(());
                }
                ActivationOutcome::Failed(reason)
                    if net.security.is_secured()
                        && matches!(
                            reason,
                            NM_ACTIVE_CONN_REASON_NO_SECRETS | NM_ACTIVE_CONN_REASON_LOGIN_FAILED
                        ) =>
                {
                    // Fall through to prompt for a fresh password and update the saved profile.
                    debug!(
                        "Saved Wi-Fi profile '{}' failed auth (reason={reason}); re-prompting",
                        net.ssid
                    );
                }
                ActivationOutcome::Failed(reason) => {
                    notify_err(
                        ctx,
                        &format!("Failed to connect to {}", net.ssid),
                        activation_reason_message(reason),
                    )
                    .await;
                    return Ok(());
                }
                ActivationOutcome::TimedOut => {
                    notify_err(
                        ctx,
                        &format!("Failed to connect to {}", net.ssid),
                        "Connection timed out",
                    )
                    .await;
                    return Ok(());
                }
            },
            Err(err) => {
                let msg = err.to_string();
                if !net.security.is_secured() || !msg.to_ascii_lowercase().contains("secret") {
                    notify_err(ctx, &format!("Failed to connect to {}", net.ssid), &msg).await;
                    return Ok(());
                }
            }
        }
    }

    // 2. Enterprise 802.1X requires EAP identity/certs -> guide to nm-connection-editor.
    if net.security == WifiSecurity::Enterprise {
        notify_info(
            ctx,
            "Enterprise Wi-Fi (802.1X)",
            &format!("Configure '{}' in Connection Editor.", net.ssid),
        )
        .await;
        open_connection_editor(ctx).await;
        return Ok(());
    }

    // 3. Prompt password if secured.
    let password = if net.security.is_secured() {
        let opts = input_opts(
            output,
            format!("{} {}: ", ctx.config.icons.lock_badge, net.ssid),
            "Wi-Fi password",
        );
        match fuzzel::password(&ctx.config.fuzzel, guard, &opts).await? {
            Some(pw) if !pw.is_empty() => Some(pw),
            _ => return Ok(()),
        }
    } else {
        None
    };

    // 4. Either update existing saved profile or add-and-activate a new one.
    let active_res = if let Some(saved) = saved_match {
        let settings = build_wifi_settings(
            &net.ssid,
            &net.ssid_bytes,
            Some(&saved.uuid),
            net.security,
            password.as_deref(),
            false,
        );
        match update_settings_connection(conn, &saved.path, settings).await {
            Ok(()) => nm
                .activate_connection(&saved.path, &dev_path, &ap_path)
                .await
                .map_err(anyhow::Error::from),
            Err(err) => Err(err),
        }
    } else {
        let settings = build_wifi_settings(
            &net.ssid,
            &net.ssid_bytes,
            None,
            net.security,
            password.as_deref(),
            false,
        );
        nm.add_and_activate_connection(settings, &dev_path, &ap_path)
            .await
            .map(|(_, active_path)| active_path)
            .map_err(anyhow::Error::from)
    };

    match active_res {
        Ok(active_path) => match wait_for_activation(conn, &active_path).await {
            ActivationOutcome::Activated => {
                notify_info(ctx, "Connected to Wi-Fi", &net.ssid).await;
            }
            ActivationOutcome::Failed(reason) => {
                notify_err(
                    ctx,
                    &format!("Failed to connect to {}", net.ssid),
                    activation_reason_message(reason),
                )
                .await;
            }
            ActivationOutcome::TimedOut => {
                notify_err(
                    ctx,
                    &format!("Failed to connect to {}", net.ssid),
                    "Connection timed out",
                )
                .await;
            }
        },
        Err(err) => {
            notify_err(
                ctx,
                &format!("Failed to connect to {}", net.ssid),
                &err.to_string(),
            )
            .await;
        }
    }

    Ok(())
}

async fn connect_hidden_wifi(
    ctx: &AppContext,
    conn: &Connection,
    guard: &MenuGuard,
    output: Option<&str>,
    wifi_dev: Option<&DeviceInfo>,
) -> Result<()> {
    let Some(wifi_dev) = wifi_dev else {
        notify_err(ctx, "No Wi-Fi adapter available", "").await;
        return Ok(());
    };

    let ssid_opts = input_opts(output, "󱛃 Hidden SSID: ".to_string(), "Network name");
    let Some(ssid_raw) = fuzzel::input(&ctx.config.fuzzel, guard, &ssid_opts).await? else {
        return Ok(());
    };
    let ssid = ssid_raw.trim().to_string();
    if ssid.is_empty() {
        return Ok(());
    }

    let pw_opts = input_opts(
        output,
        format!("{} {ssid}: ", ctx.config.icons.lock_badge),
        "Password (leave empty if open)",
    );
    let Some(password) = fuzzel::password(&ctx.config.fuzzel, guard, &pw_opts).await? else {
        return Ok(());
    };

    let security = if password.is_empty() {
        WifiSecurity::Open
    } else {
        WifiSecurity::Wpa2Psk
    };
    let pw_opt = if password.is_empty() {
        None
    } else {
        Some(password.as_str())
    };

    let settings = build_wifi_settings(&ssid, ssid.as_bytes(), None, security, pw_opt, true);
    let nm = nm_proxy(conn).await?;
    let root = root_path();

    match nm
        .add_and_activate_connection(settings, &wifi_dev.path, &root)
        .await
    {
        Ok((_, active_path)) => match wait_for_activation(conn, &active_path).await {
            ActivationOutcome::Activated => {
                notify_info(ctx, "Connected to hidden Wi-Fi", &ssid).await;
            }
            ActivationOutcome::Failed(reason) => {
                notify_err(
                    ctx,
                    &format!("Failed to connect to {ssid}"),
                    activation_reason_message(reason),
                )
                .await;
            }
            ActivationOutcome::TimedOut => {
                notify_err(
                    ctx,
                    &format!("Failed to connect to {ssid}"),
                    "Connection timed out",
                )
                .await;
            }
        },
        Err(err) => {
            notify_err(ctx, &format!("Failed to connect to {ssid}"), &err.to_string()).await;
        }
    }

    Ok(())
}

async fn request_scan_all(conn: &Connection, wifi_devs: &[DeviceInfo]) {
    for dev in wifi_devs {
        let Ok(proxy) = WirelessDeviceProxy::builder(conn).path(dev.path.as_ref()) else {
            continue;
        };
        let Ok(proxy) = proxy.build().await else {
            continue;
        };

        let old_scan = proxy.last_scan().await.unwrap_or(-1);
        let mut stream = proxy.receive_last_scan_changed().await;

        if let Err(err) = proxy.request_scan(HashMap::new()).await {
            debug!("RequestScan on {} returned: {err}", dev.interface);
            continue;
        }

        let _ = tokio::time::timeout(SCAN_TIMEOUT, async {
            while let Some(change) = stream.next().await {
                if let Ok(new_scan) = change.get().await {
                    if new_scan != old_scan {
                        break;
                    }
                }
            }
        })
        .await;
    }
}

// ---------------------------------------------------------------------------
// Ethernet Actions
// ---------------------------------------------------------------------------

async fn handle_ethernet(
    ctx: &AppContext,
    conn: &Connection,
    guard: &MenuGuard,
    output: Option<&str>,
    eth: &DeviceInfo,
) -> Result<MenuActionResult> {
    let icons = &ctx.config.icons;
    if eth.state == NM_DEVICE_STATE_UNAVAILABLE {
        notify_info(ctx, "Ethernet cable unplugged", &eth.interface).await;
        return Ok(MenuActionResult::Exit);
    }

    let nm = nm_proxy(conn).await?;

    if eth.state == NM_DEVICE_STATE_ACTIVATED {
        let active_conns = list_active_connections(conn, &nm).await;
        let active_eth = active_conns
            .into_iter()
            .find(|c| c.path == eth.active_connection || c.devices.contains(&eth.path));
        let con_name = active_eth
            .as_ref()
            .map(|c| c.id.clone())
            .unwrap_or_else(|| eth.interface.clone());

        #[derive(Clone, Copy)]
        enum EthAction {
            Info,
            Disconnect,
            Settings,
            Back,
        }

        let entries = vec![
            (
                format!("{}  Connection Information", icons.info),
                EthAction::Info,
            ),
            ("󰌙  Disconnect Ethernet".to_string(), EthAction::Disconnect),
            (
                format!("{}  Edit Connections...", icons.settings),
                EthAction::Settings,
            ),
            (format!("{}  Back", icons.back), EthAction::Back),
        ];

        let opts = menu_opts(
            output,
            format!("{}  {con_name} ", icons.ethernet),
            "Choose action...",
            6,
        );
        let Some(idx) = fuzzel::select(&ctx.config.fuzzel, guard, &entries, &opts).await? else {
            return Ok(MenuActionResult::Exit);
        };

        return match entries[idx].1 {
            EthAction::Info => {
                show_connection_info(ctx, conn, guard, output, Some(&eth.interface)).await
            }
            EthAction::Disconnect => {
                if let Some(active) = active_eth {
                    match nm.deactivate_connection(&active.path).await {
                        Ok(()) => notify_info(ctx, "Ethernet disconnected", &con_name).await,
                        Err(err) => {
                            notify_err(ctx, "Failed to disconnect Ethernet", &err.to_string())
                                .await;
                        }
                    }
                }
                Ok(MenuActionResult::Exit)
            }
            EthAction::Settings => {
                open_connection_editor(ctx).await;
                Ok(MenuActionResult::Exit)
            }
            EthAction::Back => Ok(MenuActionResult::Back),
        };
    }

    // Disconnected (cable present): auto-connect single matching profile or show picker.
    let saved_profiles = list_saved_profiles(conn).await;
    let eth_conns: Vec<SavedProfile> = saved_profiles
        .into_iter()
        .filter(|p| p.conn_type == "802-3-ethernet")
        .collect();

    let root = root_path();
    if eth_conns.is_empty() {
        // Ask NM to auto-activate / auto-create a wired connection on this device.
        match nm
            .activate_connection(&root, &eth.path, &root)
            .await
        {
            Ok(active_path) => match wait_for_activation(conn, &active_path).await {
                ActivationOutcome::Activated => {
                    notify_info(ctx, "Ethernet connected", &eth.interface).await;
                }
                ActivationOutcome::Failed(reason) => {
                    notify_err(
                        ctx,
                        "Failed to connect Ethernet",
                        activation_reason_message(reason),
                    )
                    .await;
                }
                ActivationOutcome::TimedOut => {
                    notify_err(ctx, "Failed to connect Ethernet", "Connection timed out").await;
                }
            },
            Err(err) => {
                notify_err(ctx, "Failed to connect Ethernet", &err.to_string()).await;
            }
        }
        return Ok(MenuActionResult::Exit);
    }

    if eth_conns.len() == 1 {
        let profile = &eth_conns[0];
        activate_ethernet_profile(ctx, conn, &nm, eth, profile).await;
        return Ok(MenuActionResult::Exit);
    }

    enum EthPick {
        Profile(SavedProfile),
        Back,
    }

    let mut entries: Vec<(String, EthPick)> = eth_conns
        .into_iter()
        .map(|p| (format!("{}  {}", icons.ethernet, p.id), EthPick::Profile(p)))
        .collect();
    entries.push((format!("{}  Back", icons.back), EthPick::Back));

    let opts = menu_opts(
        output,
        format!("{}  ", icons.ethernet),
        "Select Ethernet profile...",
        8,
    );
    let Some(idx) = fuzzel::select(&ctx.config.fuzzel, guard, &entries, &opts).await? else {
        return Ok(MenuActionResult::Exit);
    };

    match &entries[idx].1 {
        EthPick::Profile(profile) => {
            activate_ethernet_profile(ctx, conn, &nm, eth, profile).await;
            Ok(MenuActionResult::Exit)
        }
        EthPick::Back => Ok(MenuActionResult::Back),
    }
}

async fn activate_ethernet_profile(
    ctx: &AppContext,
    conn: &Connection,
    nm: &NetworkManagerProxy<'_>,
    eth: &DeviceInfo,
    profile: &SavedProfile,
) {
    let root = root_path();
    match nm
        .activate_connection(&profile.path, &eth.path, &root)
        .await
    {
        Ok(active_path) => match wait_for_activation(conn, &active_path).await {
            ActivationOutcome::Activated => {
                notify_info(ctx, "Ethernet connected", &profile.id).await;
            }
            ActivationOutcome::Failed(reason) => {
                notify_err(
                    ctx,
                    "Failed to connect Ethernet",
                    activation_reason_message(reason),
                )
                .await;
            }
            ActivationOutcome::TimedOut => {
                notify_err(ctx, "Failed to connect Ethernet", "Connection timed out").await;
            }
        },
        Err(err) => {
            notify_err(ctx, "Failed to connect Ethernet", &err.to_string()).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Hotspot Actions
// ---------------------------------------------------------------------------

async fn handle_hotspot(
    ctx: &AppContext,
    conn: &Connection,
    guard: &MenuGuard,
    output: Option<&str>,
    wifi_dev: Option<&DeviceInfo>,
) -> Result<MenuActionResult> {
    let icons = &ctx.config.icons;
    let nm = nm_proxy(conn).await?;
    let active_conns = list_active_connections(conn, &nm).await;

    if let Some(active_ap) = find_active_hotspot(conn, &active_conns).await {
        let psk = fetch_wifi_psk(conn, &active_ap.settings_path).await;

        #[derive(Clone)]
        enum HotspotAction {
            Copy(String, String),
            Stop,
            Back,
        }

        let mut entries = vec![(
            format!("{}  SSID: {}", icons.copy, active_ap.ssid),
            HotspotAction::Copy("Hotspot SSID".to_string(), active_ap.ssid.clone()),
        )];
        if let Some(psk) = psk.filter(|s| !s.is_empty()) {
            entries.push((
                format!("{}  Password: {psk}", icons.copy),
                HotspotAction::Copy("Hotspot Password".to_string(), psk),
            ));
        }
        entries.push(("󰌙  Stop Hotspot".to_string(), HotspotAction::Stop));
        entries.push((format!("{}  Back", icons.back), HotspotAction::Back));

        let opts = menu_opts(
            output,
            format!("{}  ", icons.hotspot),
            "Hotspot active...",
            6,
        );
        let Some(idx) = fuzzel::select(&ctx.config.fuzzel, guard, &entries, &opts).await? else {
            return Ok(MenuActionResult::Exit);
        };

        return match &entries[idx].1 {
            HotspotAction::Copy(label, value) => {
                copy_and_notify(ctx, value, label).await;
                Ok(MenuActionResult::Exit)
            }
            HotspotAction::Stop => {
                match nm.deactivate_connection(&active_ap.active_path).await {
                    Ok(()) => notify_info(ctx, "Wi-Fi hotspot stopped", "").await,
                    Err(err) => {
                        notify_err(ctx, "Failed to stop hotspot", &err.to_string()).await;
                    }
                }
                Ok(MenuActionResult::Exit)
            }
            HotspotAction::Back => Ok(MenuActionResult::Back),
        };
    }

    let Some(wifi_dev) = wifi_dev else {
        notify_err(ctx, "No Wi-Fi adapter available", "").await;
        return Ok(MenuActionResult::Exit);
    };

    let default_ssid = format!("{}-hotspot", read_hostname());
    let ssid_opts = input_opts(
        output,
        format!("{} Hotspot SSID: ", icons.hotspot),
        &default_ssid,
    );
    let Some(ssid_input) = fuzzel::input(&ctx.config.fuzzel, guard, &ssid_opts).await? else {
        return Ok(MenuActionResult::Exit);
    };
    let ssid = if ssid_input.trim().is_empty() {
        default_ssid
    } else {
        ssid_input.trim().to_string()
    };

    let pw_opts = input_opts(
        output,
        format!("{} Password (8+ chars): ", icons.lock_badge),
        "WPA2 hotspot password",
    );
    let Some(password) = fuzzel::password(&ctx.config.fuzzel, guard, &pw_opts).await? else {
        return Ok(MenuActionResult::Exit);
    };
    if password.len() < 8 {
        notify_err(
            ctx,
            "Hotspot password too short",
            "WPA2 requires at least 8 characters.",
        )
        .await;
        return Ok(MenuActionResult::Exit);
    }

    // Reuse existing single "Hotspot" AP profile if present, otherwise create it.
    let saved_profiles = list_saved_profiles(conn).await;
    let existing_hotspot = saved_profiles.iter().find(|p| {
        p.id == "Hotspot"
            && p.conn_type == "802-11-wireless"
            && p.wifi_mode.as_deref() == Some("ap")
    });

    let root = root_path();
    let activation_res = if let Some(existing) = existing_hotspot {
        let settings = build_hotspot_settings(
            Some(&existing.uuid),
            &wifi_dev.interface,
            &ssid,
            &password,
        );
        match update_settings_connection(conn, &existing.path, settings).await {
            Ok(()) => nm
                .activate_connection(&existing.path, &wifi_dev.path, &root)
                .await
                .map_err(anyhow::Error::from),
            Err(err) => Err(err),
        }
    } else {
        let settings = build_hotspot_settings(None, &wifi_dev.interface, &ssid, &password);
        nm.add_and_activate_connection(settings, &wifi_dev.path, &root)
            .await
            .map(|(_, active_path)| active_path)
            .map_err(anyhow::Error::from)
    };

    match activation_res {
        Ok(active_path) => match wait_for_activation(conn, &active_path).await {
            ActivationOutcome::Activated => {
                notify_info(ctx, "Wi-Fi hotspot started", &format!("SSID: {ssid}")).await;
            }
            ActivationOutcome::Failed(reason) => {
                notify_err(
                    ctx,
                    "Failed to start hotspot",
                    activation_reason_message(reason),
                )
                .await;
            }
            ActivationOutcome::TimedOut => {
                notify_err(ctx, "Failed to start hotspot", "Hotspot activation timed out").await;
            }
        },
        Err(err) => {
            notify_err(ctx, "Failed to start hotspot", &err.to_string()).await;
        }
    }

    Ok(MenuActionResult::Exit)
}

// ---------------------------------------------------------------------------
// VPN / WireGuard Submenu
// ---------------------------------------------------------------------------

async fn handle_vpn(
    ctx: &AppContext,
    conn: &Connection,
    guard: &MenuGuard,
    output: Option<&str>,
) -> Result<MenuActionResult> {
    let icons = &ctx.config.icons;
    let nm = nm_proxy(conn).await?;
    let saved_profiles = list_saved_profiles(conn).await;
    let active_conns = list_active_connections(conn, &nm).await;

    let active_by_uuid: HashMap<&str, &ActiveConnInfo> = active_conns
        .iter()
        .map(|c| (c.uuid.as_str(), c))
        .collect();

    let mut vpns: Vec<SavedProfile> = saved_profiles
        .into_iter()
        .filter(|p| is_vpn_type(&p.conn_type))
        .collect();

    if vpns.is_empty() {
        open_connection_editor(ctx).await;
        return Ok(MenuActionResult::Exit);
    }

    vpns.sort_by(|a, b| {
        let a_active = active_by_uuid.contains_key(a.uuid.as_str());
        let b_active = active_by_uuid.contains_key(b.uuid.as_str());
        (!a_active)
            .cmp(&(!b_active))
            .then_with(|| a.id.to_ascii_lowercase().cmp(&b.id.to_ascii_lowercase()))
    });

    enum VpnAction {
        Toggle(SavedProfile, Option<OwnedObjectPath>),
        Settings,
        Back,
    }

    let mut entries: Vec<(String, VpnAction)> = Vec::new();
    for v in vpns {
        let active_path = active_by_uuid.get(v.uuid.as_str()).map(|a| a.path.clone());
        let mark = if active_path.is_some() {
            format!("  {}", icons.check)
        } else {
            String::new()
        };
        entries.push((
            format!("{}  {}{mark}", icons.vpn, v.id),
            VpnAction::Toggle(v, active_path),
        ));
    }
    entries.push((
        format!("{}  Configure VPN...", icons.settings),
        VpnAction::Settings,
    ));
    entries.push((format!("{}  Back", icons.back), VpnAction::Back));

    let opts = menu_opts(
        output,
        format!("{}  ", icons.vpn),
        "VPN connections...",
        10,
    );
    let Some(idx) = fuzzel::select(&ctx.config.fuzzel, guard, &entries, &opts).await? else {
        return Ok(MenuActionResult::Exit);
    };

    match &entries[idx].1 {
        VpnAction::Toggle(profile, Some(active_path)) => {
            match nm.deactivate_connection(active_path).await {
                Ok(()) => notify_info(ctx, "VPN disconnected", &profile.id).await,
                Err(err) => notify_err(ctx, "VPN disconnect failed", &err.to_string()).await,
            }
            Ok(MenuActionResult::Exit)
        }
        VpnAction::Toggle(profile, None) => {
            let root = root_path();
            match nm
                .activate_connection(&profile.path, &root, &root)
                .await
            {
                Ok(active_path) => match wait_for_activation(conn, &active_path).await {
                    ActivationOutcome::Activated => {
                        notify_info(ctx, "VPN connected", &profile.id).await;
                    }
                    ActivationOutcome::Failed(reason) => {
                        notify_err(
                            ctx,
                            "VPN connect failed",
                            activation_reason_message(reason),
                        )
                        .await;
                    }
                    ActivationOutcome::TimedOut => {
                        notify_err(ctx, "VPN connect failed", "Connection timed out").await;
                    }
                },
                Err(err) => notify_err(ctx, "VPN connect failed", &err.to_string()).await,
            }
            Ok(MenuActionResult::Exit)
        }
        VpnAction::Settings => {
            open_connection_editor(ctx).await;
            Ok(MenuActionResult::Exit)
        }
        VpnAction::Back => Ok(MenuActionResult::Back),
    }
}

// ---------------------------------------------------------------------------
// Connection Information View
// ---------------------------------------------------------------------------

async fn show_connection_info(
    ctx: &AppContext,
    conn: &Connection,
    guard: &MenuGuard,
    output: Option<&str>,
    device_filter: Option<&str>,
) -> Result<MenuActionResult> {
    let icons = &ctx.config.icons;
    let nm = nm_proxy(conn).await?;
    let devices = list_devices(conn, &nm).await;
    let active_conns = list_active_connections(conn, &nm).await;

    let active_by_path: HashMap<&str, &ActiveConnInfo> = active_conns
        .iter()
        .map(|c| (c.path.as_str(), c))
        .collect();

    let connected_devs: Vec<&DeviceInfo> = devices
        .iter()
        .filter(|d| {
            d.device_type != NM_DEVICE_TYPE_LOOPBACK
                && d.device_type != NM_DEVICE_TYPE_WIFI_P2P
                && d.state == NM_DEVICE_STATE_ACTIVATED
                && device_filter.is_none_or(|f| d.interface == f)
        })
        .collect();

    enum InfoAction {
        Copy(String, String),
        Back,
    }

    let mut entries: Vec<(String, InfoAction)> = Vec::new();

    if connected_devs.is_empty() {
        entries.push(("󰲜  No active connections".to_string(), InfoAction::Back));
    } else {
        for dev in connected_devs {
            let mut rows: Vec<(String, String)> = Vec::new();
            rows.push(("Interface".to_string(), dev.interface.clone()));
            rows.push((
                "Type".to_string(),
                device_type_name(dev.device_type).to_string(),
            ));
            if let Some(active) = active_by_path.get(dev.active_connection.as_str()) {
                if !active.id.is_empty() {
                    rows.push(("Connection".to_string(), active.id.clone()));
                }
            }
            if !dev.hw_address.is_empty() {
                rows.push(("MAC".to_string(), dev.hw_address.clone()));
            }

            // IPv4 details
            if !is_root_path(&dev.ip4_config) {
                if let Ok(ip4) = Ip4ConfigProxy::builder(conn)
                    .path(dev.ip4_config.as_ref())
                    .map(|b| b.cache_properties(CacheProperties::No))
                {
                    if let Ok(ip4) = ip4.build().await {
                        if let Ok(addrs) = ip4.address_data().await {
                            for entry in addrs {
                                if let (Some(addr), Some(prefix)) = (
                                    entry.get("address").and_then(owned_as_string),
                                    entry.get("prefix").and_then(owned_as_u32),
                                ) {
                                    rows.push(("IPv4".to_string(), format!("{addr}/{prefix}")));
                                }
                            }
                        }
                        if let Ok(gw) = ip4.gateway().await {
                            if !gw.is_empty() {
                                rows.push(("Gateway".to_string(), gw));
                            }
                        }
                        let mut dns_added = false;
                        if let Ok(ns_data) = ip4.nameserver_data().await {
                            for entry in ns_data {
                                if let Some(addr) = entry.get("address").and_then(owned_as_string) {
                                    rows.push(("DNS".to_string(), addr));
                                    dns_added = true;
                                }
                            }
                        }
                        if !dns_added {
                            if let Ok(ns_legacy) = ip4.nameservers().await {
                                for raw_ip in ns_legacy {
                                    let ip = Ipv4Addr::from(raw_ip.to_ne_bytes());
                                    rows.push(("DNS".to_string(), ip.to_string()));
                                }
                            }
                        }
                    }
                }
            }

            // IPv6 global addresses (excluding link-local fe80::/10)
            if !is_root_path(&dev.ip6_config) {
                if let Ok(ip6) = Ip6ConfigProxy::builder(conn)
                    .path(dev.ip6_config.as_ref())
                    .map(|b| b.cache_properties(CacheProperties::No))
                {
                    if let Ok(ip6) = ip6.build().await {
                        if let Ok(addrs) = ip6.address_data().await {
                            for entry in addrs {
                                if let (Some(addr), Some(prefix)) = (
                                    entry.get("address").and_then(owned_as_string),
                                    entry.get("prefix").and_then(owned_as_u32),
                                ) {
                                    if is_global_ipv6(&addr) {
                                        rows.push(("IPv6".to_string(), format!("{addr}/{prefix}")));
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Wi-Fi Signal, Bitrate, Security
            if dev.device_type == NM_DEVICE_TYPE_WIFI {
                if let Ok(w_builder) = WirelessDeviceProxy::builder(conn)
                    .path(dev.path.as_ref())
                    .map(|b| b.cache_properties(CacheProperties::No))
                {
                    if let Ok(w_proxy) = w_builder.build().await {
                        let ap_path = w_proxy.active_access_point().await.ok();
                        let bitrate = w_proxy.bitrate().await.unwrap_or(0);

                        if let Some(ap_path) = ap_path.filter(|p| !is_root_path(p)) {
                            if let Ok(ap_builder) = AccessPointProxy::builder(conn)
                                .path(ap_path.as_ref())
                                .map(|b| b.cache_properties(CacheProperties::No))
                            {
                                if let Ok(ap_proxy) = ap_builder.build().await {
                                    if let Ok(strength) = ap_proxy.strength().await {
                                        rows.push(("Signal".to_string(), format!("{strength}%")));
                                    }
                                    if bitrate > 0 {
                                        rows.push(("Speed".to_string(), format_bitrate(bitrate)));
                                    }
                                    let flags = ap_proxy.flags().await.unwrap_or(0);
                                    let wpa = ap_proxy.wpa_flags().await.unwrap_or(0);
                                    let rsn = ap_proxy.rsn_flags().await.unwrap_or(0);
                                    let sec = classify_ap_security(flags, wpa, rsn);
                                    rows.push(("Security".to_string(), sec.label().to_string()));
                                }
                            }
                        } else if bitrate > 0 {
                            rows.push(("Speed".to_string(), format_bitrate(bitrate)));
                        }
                    }
                }
            }

            for (k, v) in rows {
                entries.push((format!("{}  {k}: {v}", icons.copy), InfoAction::Copy(k, v)));
            }
        }
    }

    entries.push((format!("{}  Back", icons.back), InfoAction::Back));

    let opts = menu_opts(
        output,
        format!("{}  ", icons.info),
        "Select item to copy...",
        16,
    );
    let Some(idx) = fuzzel::select(&ctx.config.fuzzel, guard, &entries, &opts).await? else {
        return Ok(MenuActionResult::Exit);
    };

    match &entries[idx].1 {
        InfoAction::Copy(k, v) => {
            copy_and_notify(ctx, v, k).await;
            Ok(MenuActionResult::Exit)
        }
        InfoAction::Back => Ok(MenuActionResult::Back),
    }
}

// ---------------------------------------------------------------------------
// NetworkManager D-Bus Queries & Helpers
// ---------------------------------------------------------------------------

async fn nm_proxy(conn: &Connection) -> Result<NetworkManagerProxy<'_>> {
    NetworkManagerProxy::builder(conn)
        .destination(NM_SERVICE)?
        .path(NM_PATH)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .context("Failed to connect to org.freedesktop.NetworkManager")
}

async fn list_devices(conn: &Connection, nm: &NetworkManagerProxy<'_>) -> Vec<DeviceInfo> {
    let Ok(paths) = nm.get_devices().await else {
        return Vec::new();
    };
    let mut devices = Vec::with_capacity(paths.len());
    for path in paths {
        let Ok(builder) = DeviceProxy::builder(conn)
            .path(path.as_ref())
            .map(|b| b.cache_properties(CacheProperties::No))
        else {
            continue;
        };
        let Ok(proxy) = builder.build().await else {
            continue;
        };
        let Ok(interface) = proxy.interface().await else {
            continue;
        };
        let device_type = proxy.device_type().await.unwrap_or(0);
        let state = proxy.state().await.unwrap_or(0);
        let hw_address = proxy.hw_address().await.unwrap_or_default();
        let active_connection = proxy
            .active_connection()
            .await
            .unwrap_or_else(|_| owned_root_path());
        let ip4_config = proxy
            .ip4_config()
            .await
            .unwrap_or_else(|_| owned_root_path());
        let ip6_config = proxy
            .ip6_config()
            .await
            .unwrap_or_else(|_| owned_root_path());

        devices.push(DeviceInfo {
            path,
            interface,
            device_type,
            state,
            hw_address,
            active_connection,
            ip4_config,
            ip6_config,
        });
    }
    devices
}

async fn list_active_connections(
    conn: &Connection,
    nm: &NetworkManagerProxy<'_>,
) -> Vec<ActiveConnInfo> {
    let Ok(paths) = nm.active_connections().await else {
        return Vec::new();
    };
    let mut list = Vec::with_capacity(paths.len());
    for path in paths {
        let Ok(builder) = ActiveConnectionProxy::builder(conn)
            .path(path.as_ref())
            .map(|b| b.cache_properties(CacheProperties::No))
        else {
            continue;
        };
        let Ok(proxy) = builder.build().await else {
            continue;
        };
        let id = proxy.id().await.unwrap_or_default();
        let uuid = proxy.uuid().await.unwrap_or_default();
        let conn_type = proxy.conn_type().await.unwrap_or_default();
        let settings_path = proxy
            .connection()
            .await
            .unwrap_or_else(|_| owned_root_path());
        let devices = proxy.devices().await.unwrap_or_default();

        list.push(ActiveConnInfo {
            path,
            settings_path,
            id,
            uuid,
            conn_type,
            devices,
        });
    }
    list
}

async fn list_saved_profiles(conn: &Connection) -> Vec<SavedProfile> {
    let Ok(settings_builder) = SettingsProxy::builder(conn)
        .destination(NM_SERVICE)
        .and_then(|b| b.path(NM_SETTINGS_PATH))
    else {
        return Vec::new();
    };
    let Ok(settings_proxy) = settings_builder
        .cache_properties(CacheProperties::No)
        .build()
        .await
    else {
        return Vec::new();
    };
    let Ok(paths) = settings_proxy.list_connections().await else {
        return Vec::new();
    };

    let mut profiles = Vec::with_capacity(paths.len());
    for path in paths {
        let Ok(builder) = SettingsConnectionProxy::builder(conn)
            .path(path.as_ref())
            .map(|b| b.cache_properties(CacheProperties::No))
        else {
            continue;
        };
        let Ok(proxy) = builder.build().await else {
            continue;
        };
        let Ok(map) = proxy.get_settings().await else {
            continue;
        };

        let Some(conn_sec) = map.get("connection") else {
            continue;
        };
        let id = conn_sec
            .get("id")
            .and_then(owned_as_string)
            .unwrap_or_default();
        let uuid = conn_sec
            .get("uuid")
            .and_then(owned_as_string)
            .unwrap_or_default();
        let conn_type = conn_sec
            .get("type")
            .and_then(owned_as_string)
            .unwrap_or_default();

        let (wifi_ssid, wifi_mode) = if let Some(wifi_sec) = map.get("802-11-wireless") {
            (
                wifi_sec.get("ssid").and_then(owned_as_bytes),
                wifi_sec.get("mode").and_then(owned_as_string),
            )
        } else {
            (None, None)
        };

        profiles.push(SavedProfile {
            path,
            id,
            uuid,
            conn_type,
            wifi_ssid,
            wifi_mode,
        });
    }
    profiles
}

async fn list_wifi_networks(conn: &Connection, wifi_devs: &[DeviceInfo]) -> Vec<WifiNetwork> {
    let mut raw_aps = Vec::new();

    for dev in wifi_devs {
        let Ok(w_builder) = WirelessDeviceProxy::builder(conn)
            .path(dev.path.as_ref())
            .map(|b| b.cache_properties(CacheProperties::No))
        else {
            continue;
        };
        let Ok(w_proxy) = w_builder.build().await else {
            continue;
        };

        let active_ap = w_proxy
            .active_access_point()
            .await
            .unwrap_or_else(|_| owned_root_path());
        let Ok(ap_paths) = w_proxy.get_all_access_points().await else {
            continue;
        };

        for ap_path in ap_paths {
            let Ok(ap_builder) = AccessPointProxy::builder(conn)
                .path(ap_path.as_ref())
                .map(|b| b.cache_properties(CacheProperties::No))
            else {
                continue;
            };
            let Ok(ap_proxy) = ap_builder.build().await else {
                continue;
            };

            let Ok(ssid_bytes) = ap_proxy.ssid().await else {
                continue;
            };
            if ssid_bytes.is_empty() {
                continue;
            }
            let ssid = String::from_utf8_lossy(&ssid_bytes).trim().to_string();
            if ssid.is_empty() {
                continue;
            }

            let strength = ap_proxy.strength().await.unwrap_or(0);
            let flags = ap_proxy.flags().await.unwrap_or(0);
            let wpa_flags = ap_proxy.wpa_flags().await.unwrap_or(0);
            let rsn_flags = ap_proxy.rsn_flags().await.unwrap_or(0);
            let security = classify_ap_security(flags, wpa_flags, rsn_flags);
            let active = !is_root_path(&active_ap) && ap_path == active_ap;

            raw_aps.push(WifiNetwork {
                ssid,
                ssid_bytes,
                signal: strength,
                security,
                active,
                ap_path: ap_path.to_string(),
                device_path: dev.path.to_string(),
            });
        }
    }

    deduplicate_and_sort_wifi(raw_aps)
}

async fn find_active_hotspot(
    conn: &Connection,
    active_conns: &[ActiveConnInfo],
) -> Option<ActiveHotspot> {
    for active in active_conns {
        if active.conn_type != "802-11-wireless" || is_root_path(&active.settings_path) {
            continue;
        }
        let Ok(builder) = SettingsConnectionProxy::builder(conn)
            .path(active.settings_path.as_ref())
            .map(|b| b.cache_properties(CacheProperties::No))
        else {
            continue;
        };
        let Ok(proxy) = builder.build().await else {
            continue;
        };
        let Ok(settings) = proxy.get_settings().await else {
            continue;
        };
        let Some(wifi) = settings.get("802-11-wireless") else {
            continue;
        };
        if wifi.get("mode").and_then(owned_as_string).as_deref() == Some("ap") {
            let ssid = wifi
                .get("ssid")
                .and_then(owned_as_bytes)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| active.id.clone());
            return Some(ActiveHotspot {
                active_path: active.path.clone(),
                settings_path: active.settings_path.clone(),
                ssid,
            });
        }
    }
    None
}

async fn fetch_wifi_psk(conn: &Connection, settings_path: &OwnedObjectPath) -> Option<String> {
    if is_root_path(settings_path) {
        return None;
    }
    let builder = SettingsConnectionProxy::builder(conn)
        .path(settings_path.as_ref())
        .ok()?
        .cache_properties(CacheProperties::No);
    let proxy = builder.build().await.ok()?;
    let secrets = proxy.get_secrets("802-11-wireless-security").await.ok()?;
    secrets
        .get("802-11-wireless-security")
        .and_then(|sec| sec.get("psk"))
        .and_then(owned_as_string)
}

async fn update_settings_connection(
    conn: &Connection,
    settings_path: &OwnedObjectPath,
    properties: HashMap<&str, HashMap<&str, Value<'_>>>,
) -> Result<()> {
    let proxy = SettingsConnectionProxy::builder(conn)
        .path(settings_path.as_ref())?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    proxy
        .update(properties)
        .await
        .map_err(|e| anyhow!("{e}"))
}

async fn delete_settings_connection(
    conn: &Connection,
    settings_path: &OwnedObjectPath,
) -> Result<()> {
    let proxy = SettingsConnectionProxy::builder(conn)
        .path(settings_path.as_ref())?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    proxy.delete().await.map_err(|e| anyhow!("{e}"))
}

async fn wait_for_activation(conn: &Connection, active_path: &OwnedObjectPath) -> ActivationOutcome {
    let Ok(builder) = ActiveConnectionProxy::builder(conn)
        .path(active_path.as_ref())
        .map(|b| b.cache_properties(CacheProperties::No))
    else {
        return ActivationOutcome::Failed(0);
    };
    let Ok(proxy) = builder.build().await else {
        return ActivationOutcome::Failed(0);
    };

    let Ok(mut stream) = proxy.receive_nm_state_changed().await else {
        return match proxy.state().await {
            Ok(NM_ACTIVE_CONN_ACTIVATED) => ActivationOutcome::Activated,
            _ => ActivationOutcome::Failed(0),
        };
    };

    match proxy.state().await {
        Ok(NM_ACTIVE_CONN_ACTIVATED) => return ActivationOutcome::Activated,
        Ok(NM_ACTIVE_CONN_DEACTIVATING | NM_ACTIVE_CONN_DEACTIVATED) => {
            return ActivationOutcome::Failed(0);
        }
        Err(_) => return ActivationOutcome::Failed(0),
        _ => {}
    }

    let waited = tokio::time::timeout(ACTIVATION_TIMEOUT, async {
        let mut last_reason = 0u32;
        while let Some(sig) = stream.next().await {
            let Ok(args) = sig.args() else {
                continue;
            };
            if args.reason != 0 {
                last_reason = args.reason;
            }
            match args.state {
                NM_ACTIVE_CONN_ACTIVATED => return ActivationOutcome::Activated,
                NM_ACTIVE_CONN_DEACTIVATING if args.reason != 0 => {
                    return ActivationOutcome::Failed(args.reason);
                }
                NM_ACTIVE_CONN_DEACTIVATED => {
                    return ActivationOutcome::Failed(last_reason);
                }
                _ => {}
            }
        }
        ActivationOutcome::Failed(last_reason)
    })
    .await;

    match waited {
        Ok(outcome) => outcome,
        Err(_) => match proxy.state().await {
            Ok(NM_ACTIVE_CONN_ACTIVATED) => ActivationOutcome::Activated,
            _ => ActivationOutcome::TimedOut,
        },
    }
}

// ---------------------------------------------------------------------------
// Settings Builders
// ---------------------------------------------------------------------------

fn build_wifi_settings<'a>(
    ssid: &'a str,
    ssid_bytes: &[u8],
    existing_uuid: Option<&'a str>,
    security: WifiSecurity,
    password: Option<&'a str>,
    hidden: bool,
) -> HashMap<&'a str, HashMap<&'a str, Value<'a>>> {
    let mut conn_sec: HashMap<&'a str, Value<'a>> = HashMap::new();
    conn_sec.insert("id", Value::from(ssid));
    conn_sec.insert("type", Value::from("802-11-wireless"));
    if let Some(uuid) = existing_uuid {
        conn_sec.insert("uuid", Value::from(uuid));
    }

    let mut wifi_sec: HashMap<&'a str, Value<'a>> = HashMap::new();
    wifi_sec.insert("ssid", Value::from(ssid_bytes.to_vec()));
    wifi_sec.insert("mode", Value::from("infrastructure"));
    if hidden {
        wifi_sec.insert("hidden", Value::from(true));
    }

    let mut settings: HashMap<&'a str, HashMap<&'a str, Value<'a>>> = HashMap::new();
    settings.insert("connection", conn_sec);
    settings.insert("802-11-wireless", wifi_sec);

    if let (Some(key_mgmt), Some(pw)) = (security.key_mgmt(), password) {
        let mut sec: HashMap<&'a str, Value<'a>> = HashMap::new();
        sec.insert("key-mgmt", Value::from(key_mgmt));
        if security == WifiSecurity::Wep {
            sec.insert("wep-key0", Value::from(pw));
            sec.insert("wep-key-type", Value::from(1u32));
        } else {
            sec.insert("psk", Value::from(pw));
        }
        settings.insert("802-11-wireless-security", sec);
    }

    settings
}

fn build_hotspot_settings<'a>(
    existing_uuid: Option<&'a str>,
    interface_name: &'a str,
    ssid: &str,
    password: &'a str,
) -> HashMap<&'a str, HashMap<&'a str, Value<'a>>> {
    let mut conn_sec: HashMap<&'a str, Value<'a>> = HashMap::new();
    conn_sec.insert("id", Value::from("Hotspot"));
    conn_sec.insert("type", Value::from("802-11-wireless"));
    conn_sec.insert("autoconnect", Value::from(false));
    conn_sec.insert("interface-name", Value::from(interface_name));
    if let Some(uuid) = existing_uuid {
        conn_sec.insert("uuid", Value::from(uuid));
    }

    let mut wifi_sec: HashMap<&'a str, Value<'a>> = HashMap::new();
    wifi_sec.insert("ssid", Value::from(ssid.as_bytes().to_vec()));
    wifi_sec.insert("mode", Value::from("ap"));

    let mut wireless_sec: HashMap<&'a str, Value<'a>> = HashMap::new();
    wireless_sec.insert("key-mgmt", Value::from("wpa-psk"));
    wireless_sec.insert("psk", Value::from(password));

    let mut ipv4_sec: HashMap<&'a str, Value<'a>> = HashMap::new();
    ipv4_sec.insert("method", Value::from("shared"));

    let mut ipv6_sec: HashMap<&'a str, Value<'a>> = HashMap::new();
    ipv6_sec.insert("method", Value::from("ignore"));

    let mut settings: HashMap<&'a str, HashMap<&'a str, Value<'a>>> = HashMap::new();
    settings.insert("connection", conn_sec);
    settings.insert("802-11-wireless", wifi_sec);
    settings.insert("802-11-wireless-security", wireless_sec);
    settings.insert("ipv4", ipv4_sec);
    settings.insert("ipv6", ipv6_sec);
    settings
}

// ---------------------------------------------------------------------------
// Utility Helpers
// ---------------------------------------------------------------------------

fn root_path() -> ObjectPath<'static> {
    ObjectPath::from_static_str_unchecked("/")
}

fn owned_root_path() -> OwnedObjectPath {
    OwnedObjectPath::from(root_path())
}

fn is_root_path(path: &OwnedObjectPath) -> bool {
    path.as_str() == "/"
}

fn is_vpn_type(conn_type: &str) -> bool {
    matches!(conn_type, "vpn" | "wireguard")
}

fn owned_as_string(v: &OwnedValue) -> Option<String> {
    String::try_from(v.try_clone().ok()?).ok()
}

fn owned_as_u32(v: &OwnedValue) -> Option<u32> {
    u32::try_from(v.try_clone().ok()?).ok()
}

fn owned_as_bytes(v: &OwnedValue) -> Option<Vec<u8>> {
    Vec::<u8>::try_from(v.try_clone().ok()?).ok()
}

fn read_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "linux".to_string())
}

fn menu_opts(
    output: Option<&str>,
    prompt: String,
    placeholder: &str,
    max_lines: usize,
) -> FuzzelOptions {
    FuzzelOptions {
        prompt,
        placeholder: Some(placeholder.to_string()),
        anchor: Anchor::TopRight,
        width: Some(38),
        max_lines: Some(max_lines),
        minimal_lines: true,
        output: output.map(ToString::to_string),
        ..Default::default()
    }
}

fn input_opts(output: Option<&str>, prompt: String, placeholder: &str) -> FuzzelOptions {
    FuzzelOptions {
        prompt,
        placeholder: Some(placeholder.to_string()),
        anchor: Anchor::TopRight,
        width: Some(38),
        output: output.map(ToString::to_string),
        ..Default::default()
    }
}

async fn notify_info(ctx: &AppContext, summary: &str, body: &str) {
    let mut notif = Notification::new("Network", summary).urgency(Urgency::Normal);
    if !body.is_empty() {
        notif = notif.body(body);
    }
    ctx.notifier.send_best_effort(&notif).await;
}

async fn notify_err(ctx: &AppContext, summary: &str, body: &str) {
    let mut notif = Notification::new("Network", summary).urgency(Urgency::Critical);
    if !body.is_empty() {
        notif = notif.body(body);
    }
    ctx.notifier.send_best_effort(&notif).await;
}

async fn copy_and_notify(ctx: &AppContext, value: &str, label: &str) {
    match clipboard::copy_text(value).await {
        Ok(()) => {
            let body = if label.is_empty() {
                value.to_string()
            } else {
                format!("{label}: {value}")
            };
            notify_info(ctx, "Copied to clipboard", &body).await;
        }
        Err(err) => {
            notify_err(ctx, "Failed to copy to clipboard", &err.to_string()).await;
        }
    }
}

async fn open_connection_editor(ctx: &AppContext) {
    if let Err(err) = spawn_reaped(Command::new("nm-connection-editor").process_group(0)) {
        warn!("Failed to launch nm-connection-editor: {err}");
        notify_err(
            ctx,
            "Failed to open Connection Editor",
            "Is nm-connection-editor installed?",
        )
        .await;
    }
}

// ---------------------------------------------------------------------------
// Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_ap(
        ssid: &str,
        signal: u8,
        security: WifiSecurity,
        active: bool,
        ap_path: &str,
    ) -> WifiNetwork {
        WifiNetwork {
            ssid: ssid.to_string(),
            ssid_bytes: ssid.as_bytes().to_vec(),
            signal,
            security,
            active,
            ap_path: ap_path.to_string(),
            device_path: "/org/freedesktop/NetworkManager/Devices/1".to_string(),
        }
    }

    #[test]
    fn deduplicate_and_sort_wifi_orders_active_then_signal_then_ssid() {
        let aps = vec![
            mk_ap("Zeta", 75, WifiSecurity::Wpa2Psk, false, "/ap/1"),
            mk_ap("alpha", 75, WifiSecurity::Open, false, "/ap/2"),
            mk_ap("Beta", 90, WifiSecurity::Wpa3Sae, false, "/ap/3"),
            mk_ap("Office", 42, WifiSecurity::Wpa2Psk, true, "/ap/4"),
            mk_ap("Office", 68, WifiSecurity::Open, false, "/ap/5"),
            mk_ap("alpha", 80, WifiSecurity::Wpa2Psk, false, "/ap/6"),
            mk_ap("", 99, WifiSecurity::Open, false, "/ap/7"),
            mk_ap("   ", 99, WifiSecurity::Open, false, "/ap/8"),
        ];

        let sorted = deduplicate_and_sort_wifi(aps);
        let names: Vec<&str> = sorted.iter().map(|n| n.ssid.as_str()).collect();
        assert_eq!(names, vec!["Office", "Beta", "alpha", "Zeta"]);

        // Active network preserves active=true, takes max signal and strongest security,
        // and keeps active AP path.
        assert!(sorted[0].active);
        assert_eq!(sorted[0].signal, 68);
        assert_eq!(sorted[0].security, WifiSecurity::Wpa2Psk);
        assert_eq!(sorted[0].ap_path, "/ap/4");

        // Inactive duplicate keeps strongest signal, strongest security, and strongest AP path.
        assert_eq!(sorted[2].ssid, "alpha");
        assert_eq!(sorted[2].signal, 80);
        assert_eq!(sorted[2].security, WifiSecurity::Wpa2Psk);
        assert_eq!(sorted[2].ap_path, "/ap/6");
    }

    #[test]
    fn deduplicate_wifi_case_insensitive_tiebreak() {
        let aps = vec![
            mk_ap("gamma", 50, WifiSecurity::Open, false, "/ap/1"),
            mk_ap("Alpha", 50, WifiSecurity::Open, false, "/ap/2"),
            mk_ap("beta", 50, WifiSecurity::Open, false, "/ap/3"),
        ];

        let sorted = deduplicate_and_sort_wifi(aps);
        let names: Vec<&str> = sorted.iter().map(|n| n.ssid.as_str()).collect();
        assert_eq!(names, vec!["Alpha", "beta", "gamma"]);
    }

    #[test]
    fn classify_ap_security_covers_open_wep_wpa2_wpa3_eap_owe() {
        // Open
        let open = classify_ap_security(0, 0, 0);
        assert_eq!(open, WifiSecurity::Open);
        assert!(!open.is_secured());
        assert_eq!(open.key_mgmt(), None);
        assert_eq!(open.label(), "Open");

        // OWE (Enhanced Open)
        let owe = classify_ap_security(0, 0, NM_802_11_AP_SEC_KEY_MGMT_OWE);
        assert_eq!(owe, WifiSecurity::Owe);
        assert!(!owe.is_secured());
        assert_eq!(owe.key_mgmt(), None);

        // WEP
        let wep = classify_ap_security(NM_802_11_AP_FLAGS_PRIVACY, 0, 0);
        assert_eq!(wep, WifiSecurity::Wep);
        assert!(wep.is_secured());
        assert_eq!(wep.key_mgmt(), Some("none"));

        // WPA1 PSK
        let wpa = classify_ap_security(
            NM_802_11_AP_FLAGS_PRIVACY,
            NM_802_11_AP_SEC_KEY_MGMT_PSK | 0x0044,
            0,
        );
        assert_eq!(wpa, WifiSecurity::WpaPsk);
        assert!(wpa.is_secured());
        assert_eq!(wpa.key_mgmt(), Some("wpa-psk"));

        // WPA2 PSK
        let wpa2 = classify_ap_security(
            NM_802_11_AP_FLAGS_PRIVACY,
            0,
            NM_802_11_AP_SEC_KEY_MGMT_PSK | 0x0088,
        );
        assert_eq!(wpa2, WifiSecurity::Wpa2Psk);
        assert!(wpa2.is_secured());
        assert_eq!(wpa2.key_mgmt(), Some("wpa-psk"));
        assert_eq!(wpa2.label(), "WPA2");

        // WPA3 SAE only
        let wpa3 = classify_ap_security(
            NM_802_11_AP_FLAGS_PRIVACY,
            0,
            NM_802_11_AP_SEC_KEY_MGMT_SAE | 0x0088,
        );
        assert_eq!(wpa3, WifiSecurity::Wpa3Sae);
        assert!(wpa3.is_secured());
        assert_eq!(wpa3.key_mgmt(), Some("sae"));
        assert_eq!(wpa3.label(), "WPA3");

        // WPA2/WPA3 transitional
        let wpa23 = classify_ap_security(
            NM_802_11_AP_FLAGS_PRIVACY,
            0,
            NM_802_11_AP_SEC_KEY_MGMT_PSK | NM_802_11_AP_SEC_KEY_MGMT_SAE | 0x0088,
        );
        assert_eq!(wpa23, WifiSecurity::Wpa2Wpa3Sae);
        assert!(wpa23.is_secured());
        assert_eq!(wpa23.key_mgmt(), Some("sae"));
        assert_eq!(wpa23.label(), "WPA2/WPA3");

        // Enterprise 802.1X
        let eap = classify_ap_security(
            NM_802_11_AP_FLAGS_PRIVACY,
            0,
            NM_802_11_AP_SEC_KEY_MGMT_802_1X | 0x0088,
        );
        assert_eq!(eap, WifiSecurity::Enterprise);
        assert!(eap.is_secured());
        assert_eq!(eap.key_mgmt(), Some("wpa-eap"));
    }

    #[test]
    fn wifi_signal_icon_maps_five_steps_cleanly() {
        let levels: Vec<String> = vec!["0", "1", "2", "3", "4"]
            .into_iter()
            .map(String::from)
            .collect();

        assert_eq!(wifi_signal_icon(0, &levels), "0");
        assert_eq!(wifi_signal_icon(20, &levels), "0");
        assert_eq!(wifi_signal_icon(21, &levels), "1");
        assert_eq!(wifi_signal_icon(40, &levels), "1");
        assert_eq!(wifi_signal_icon(41, &levels), "2");
        assert_eq!(wifi_signal_icon(60, &levels), "2");
        assert_eq!(wifi_signal_icon(61, &levels), "3");
        assert_eq!(wifi_signal_icon(80, &levels), "3");
        assert_eq!(wifi_signal_icon(81, &levels), "4");
        assert_eq!(wifi_signal_icon(100, &levels), "4");
        assert_eq!(wifi_signal_icon(255, &levels), "4");

        // Empty slice falls back gracefully without panicking.
        assert_eq!(wifi_signal_icon(50, &[]), "󰤨");
    }

    #[test]
    fn ipv6_link_local_filtering() {
        assert!(!is_global_ipv6("fe80::1"));
        assert!(!is_global_ipv6("fe80::1234:5678:9abc:def0/64"));
        assert!(!is_global_ipv6("FE80::1/64"));
        assert!(!is_global_ipv6("febf::1/64"));
        assert!(!is_global_ipv6("::1/128"));
        assert!(!is_global_ipv6("::/128"));
        assert!(!is_global_ipv6(""));

        assert!(is_global_ipv6("2001:db8::1/64"));
        assert!(is_global_ipv6("2606:4700:4700::1111/64"));
        assert!(is_global_ipv6("fd12:3456:789a::1/64"));
        assert!(is_global_ipv6("fc00::1"));
    }

    #[test]
    fn format_bitrate_formats_kbps_and_mbps() {
        assert_eq!(format_bitrate(500), "500 Kbit/s");
        assert_eq!(format_bitrate(54_000), "54 Mbit/s");
        assert_eq!(format_bitrate(866_700), "866.7 Mbit/s");
    }
}
