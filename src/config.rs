use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::warn;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub fuzzel: FuzzelConfig,
    pub tooltip: TooltipConfig,
    pub battery: BatteryConfig,
    pub backlight: BacklightConfig,
    pub power: PowerConfig,
    pub screenshot: ScreenshotConfig,
    pub media: MediaConfig,
    pub icons: IconsConfig,
}

impl Config {
    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("/tmp"))
                    .join(".config")
            })
            .join("fuzzyland")
            .join("config.toml")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => match toml::from_str::<Config>(&raw) {
                Ok(cfg) => cfg,
                Err(err) => {
                    warn!("Failed to parse {}: {err}; using defaults", path.display());
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                warn!("Failed to read {}: {err}; using defaults", path.display());
                Self::default()
            }
        }
    }
}

pub fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("fuzzyland")
}

pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FuzzelConfig {
    pub binary: String,
    pub anchor_margin_x: u32,
    pub anchor_margin_y: u32,
}

impl Default for FuzzelConfig {
    fn default() -> Self {
        Self {
            binary: "fuzzel".to_string(),
            anchor_margin_x: 10,
            anchor_margin_y: 10,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TooltipConfig {
    pub text: String,
    pub subtext: String,
    pub muted: String,
    pub separator: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BatteryConfig {
    pub warning_threshold: u8,
    pub critical_threshold: u8,
}

impl Default for BatteryConfig {
    fn default() -> Self {
        Self {
            warning_threshold: 25,
            critical_threshold: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BacklightConfig {
    pub debounce_ms: u64,
    pub poll_interval_secs: u64,
    pub i2c_settle_ms: u64,
    pub internal_prefixes: Vec<String>,
}

impl Default for BacklightConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 120,
            poll_interval_secs: 30,
            i2c_settle_ms: 40,
            internal_prefixes: vec![
                "eDP-".to_string(),
                "LVDS-".to_string(),
                "DSI-".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PowerConfig {
    pub confirm: Vec<String>,
    pub layout_path: Option<String>,
}

impl Default for PowerConfig {
    fn default() -> Self {
        Self {
            confirm: vec![
                "logout".to_string(),
                "reboot".to_string(),
                "shutdown".to_string(),
            ],
            layout_path: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenshotConfig {
    pub save_dir: String,
    pub slurp_bg: String,
    pub slurp_border: String,
    pub slurp_selection: String,
    pub slurp_box: String,
    pub slurp_font: String,
}

impl Default for ScreenshotConfig {
    fn default() -> Self {
        Self {
            save_dir: "~/Pictures/Screenshots".to_string(),
            slurp_bg: "#00000080".to_string(),
            slurp_border: "#ffffffcc".to_string(),
            slurp_selection: "#ffffff22".to_string(),
            slurp_box: "#000000bb".to_string(),
            slurp_font: "monospace".to_string(),
        }
    }
}

impl ScreenshotConfig {
    pub fn resolved_save_dir(&self) -> PathBuf {
        if let Ok(env_dir) = std::env::var("SCREENSHOTS_DIR") {
            if !env_dir.trim().is_empty() && self.save_dir == Self::default().save_dir {
                return expand_tilde(env_dir.trim());
            }
        }
        expand_tilde(&self.save_dir)
    }

    pub fn effective_slurp_bg(&self) -> String {
        std::env::var("SCREENSHOT_SLURP_BG")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| self.slurp_bg.clone())
    }

    pub fn effective_slurp_border(&self) -> String {
        std::env::var("SCREENSHOT_SLURP_BORDER")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| self.slurp_border.clone())
    }

    pub fn effective_slurp_selection(&self) -> String {
        std::env::var("SCREENSHOT_SLURP_SELECTION")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| self.slurp_selection.clone())
    }

    pub fn effective_slurp_box(&self) -> String {
        std::env::var("SCREENSHOT_SLURP_BOX")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| self.slurp_box.clone())
    }

    pub fn effective_slurp_font(&self) -> String {
        std::env::var("SCREENSHOT_SLURP_FONT")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| self.slurp_font.clone())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaConfig {
    pub max_track_len: usize,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self { max_track_len: 44 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IconsConfig {
    // Common
    pub check: String,
    pub cancel: String,
    pub back: String,
    pub info: String,
    pub settings: String,
    pub trash: String,
    pub refresh: String,
    pub toggle_on: String,
    pub toggle_off: String,
    pub lock_badge: String,
    pub copy: String,

    // Network
    pub wifi_levels: Vec<String>,
    pub wifi_off: String,
    pub ethernet: String,
    pub ethernet_off: String,
    pub vpn: String,
    pub hotspot: String,

    // Bluetooth
    pub bluetooth: String,
    pub bluetooth_off: String,
    pub bluetooth_connected: String,
    pub bt_headset: String,
    pub bt_headphones: String,
    pub bt_speaker: String,
    pub bt_keyboard: String,
    pub bt_mouse: String,
    pub bt_gamepad: String,
    pub bt_phone: String,
    pub bt_computer: String,
    pub bt_camera: String,

    // Battery & Power Profiles
    pub battery_levels: Vec<String>,
    pub battery_charging: String,
    pub battery_plugged: String,
    pub battery_unknown: String,
    pub profile_power_saver: String,
    pub profile_balanced: String,
    pub profile_performance: String,
    pub tablet: String,
    pub stylus: String,

    // Audio & Displays
    pub speaker: String,
    pub mic: String,
    pub eq: String,
    pub monitor: String,

    // Power
    pub power_lock: String,
    pub power_suspend: String,
    pub power_hibernate: String,
    pub power_logout: String,
    pub power_reboot: String,
    pub power_shutdown: String,

    // Clipboard
    pub clipboard: String,
    pub clipboard_image: String,

    // Screenshot
    pub camera: String,
    pub region: String,
    pub window: String,
    pub all_monitors: String,
    pub save: String,

    // Media
    pub media_prev: String,
    pub media_play: String,
    pub media_pause: String,
    pub media_next: String,
}

impl Default for IconsConfig {
    fn default() -> Self {
        Self {
            check: "󰄬".to_string(),
            cancel: "󰅖".to_string(),
            back: "󰁍".to_string(),
            info: "󰋼".to_string(),
            settings: "󰒓".to_string(),
            trash: "󰆴".to_string(),
            refresh: "󰑐".to_string(),
            toggle_on: "󰔡".to_string(),
            toggle_off: "󰔢".to_string(),
            lock_badge: "󰌾".to_string(),
            copy: "󰅍".to_string(),

            wifi_levels: vec![
                "󰤯".to_string(),
                "󰤟".to_string(),
                "󰤢".to_string(),
                "󰤥".to_string(),
                "󰤨".to_string(),
            ],
            wifi_off: "󰤭".to_string(),
            ethernet: "󰈀".to_string(),
            ethernet_off: "󰈂".to_string(),
            vpn: "󰦝".to_string(),
            hotspot: "󰀂".to_string(),

            bluetooth: "󰂯".to_string(),
            bluetooth_off: "󰂲".to_string(),
            bluetooth_connected: "󰂱".to_string(),
            bt_headset: "󰋎".to_string(),
            bt_headphones: "󰋋".to_string(),
            bt_speaker: "󰓃".to_string(),
            bt_keyboard: "󰌌".to_string(),
            bt_mouse: "󰍽".to_string(),
            bt_gamepad: "󰊴".to_string(),
            bt_phone: "󰏲".to_string(),
            bt_computer: "󰌢".to_string(),
            bt_camera: "󰄀".to_string(),

            battery_levels: vec![
                "󰁺".to_string(),
                "󰁻".to_string(),
                "󰁼".to_string(),
                "󰁽".to_string(),
                "󰁾".to_string(),
                "󰁿".to_string(),
                "󰂀".to_string(),
                "󰂁".to_string(),
                "󰂂".to_string(),
                "󰁹".to_string(),
            ],
            battery_charging: "󰂄".to_string(),
            battery_plugged: "󰚥".to_string(),
            battery_unknown: "󰂑".to_string(),
            profile_power_saver: "󰌪".to_string(),
            profile_balanced: "󰾅".to_string(),
            profile_performance: "󰓅".to_string(),
            tablet: "󰓶".to_string(),
            stylus: "󰏫".to_string(),

            speaker: "󰕾".to_string(),
            mic: "󰍬".to_string(),
            eq: "󰺢".to_string(),
            monitor: "󰍹".to_string(),

            power_lock: "󰌾".to_string(),
            power_suspend: "󰤄".to_string(),
            power_hibernate: "󰜗".to_string(),
            power_logout: "󰍃".to_string(),
            power_reboot: "󰜉".to_string(),
            power_shutdown: "󰐥".to_string(),

            clipboard: "󰅌".to_string(),
            clipboard_image: "󰋩".to_string(),

            camera: "󰄀".to_string(),
            region: "󰩭".to_string(),
            window: "󰖯".to_string(),
            all_monitors: "󰍺".to_string(),
            save: "󰆓".to_string(),

            media_prev: "⏮".to_string(),
            media_play: "▶".to_string(),
            media_pause: "⏸".to_string(),
            media_next: "⏭".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_uses_theme_agnostic_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.fuzzel.binary, "fuzzel");
        assert_eq!(cfg.fuzzel.anchor_margin_x, 10);
        assert!(cfg.tooltip.text.is_empty());
        assert!(cfg.tooltip.subtext.is_empty());
        assert!(cfg.tooltip.muted.is_empty());
        assert!(cfg.tooltip.separator.is_empty());
        assert_eq!(cfg.battery.warning_threshold, 25);
        assert_eq!(cfg.battery.critical_threshold, 10);
        assert_eq!(cfg.backlight.debounce_ms, 120);
        assert_eq!(cfg.backlight.poll_interval_secs, 30);
        assert_eq!(cfg.backlight.i2c_settle_ms, 40);
        assert_eq!(cfg.backlight.internal_prefixes, vec!["eDP-", "LVDS-", "DSI-"]);
        assert_eq!(cfg.power.confirm, vec!["logout", "reboot", "shutdown"]);
        assert!(cfg.power.layout_path.is_none());
        assert_eq!(cfg.screenshot.save_dir, "~/Pictures/Screenshots");
        assert_eq!(cfg.media.max_track_len, 44);
        assert_eq!(cfg.icons.wifi_levels.len(), 5);
        assert_eq!(cfg.icons.battery_levels.len(), 10);
    }

    #[test]
    fn partial_toml_overrides_only_specified_fields() {
        let raw = r##"
[tooltip]
text = "#cdd6f4"
muted = "#7f849c"

[battery]
warning_threshold = 30

[power]
layout_path = "~/.config/wlogout/layout"

[media]
max_track_len = 60
"##;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.tooltip.text, "#cdd6f4");
        assert!(cfg.tooltip.subtext.is_empty());
        assert_eq!(cfg.tooltip.muted, "#7f849c");
        assert_eq!(cfg.battery.warning_threshold, 30);
        assert_eq!(cfg.battery.critical_threshold, 10);
        assert_eq!(
            cfg.power.layout_path.as_deref(),
            Some("~/.config/wlogout/layout")
        );
        assert_eq!(cfg.power.confirm, vec!["logout", "reboot", "shutdown"]);
        assert_eq!(cfg.media.max_track_len, 60);
    }

    #[test]
    fn load_from_missing_file_returns_defaults() {
        let cfg = Config::load_from(Path::new("/nonexistent/fuzzyland/config.toml"));
        assert_eq!(cfg.fuzzel.binary, "fuzzel");
    }

    #[test]
    fn expand_tilde_handles_home_and_absolute_paths() {
        assert_eq!(expand_tilde("/tmp/shots"), PathBuf::from("/tmp/shots"));
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_tilde("~"), home);
            assert_eq!(
                expand_tilde("~/Pictures/Screenshots"),
                home.join("Pictures/Screenshots")
            );
        }
    }

    #[test]
    fn catppuccin_example_config_parses_cleanly() {
        let raw = include_str!("../examples/catppuccin-mocha.toml");
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.tooltip.text, "#cdd6f4");
        assert_eq!(cfg.screenshot.slurp_border, "#b4befeff");
        assert_eq!(cfg.screenshot.slurp_font, "JetBrains Mono");
    }
}
